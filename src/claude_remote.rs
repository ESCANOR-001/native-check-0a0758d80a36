//! Observer transport for an existing Claude Remote Control session. It owns no
//! CLI, does not enable sharing, and never retries a prompt or permission reply.
//! Cursor persistence and projection are the caller's responsibility.
use crate::claude_host::Identity;
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Response,
};
use serde_json::{json, Value};
use std::{collections::HashSet, io::Read, path::Path, time::Duration};

const API: &str = "https://api.anthropic.com/v1/code/sessions/";
const AUTH_LIMIT: u64 = 128 * 1024;
const METADATA_LIMIT: usize = 256 * 1024;
#[path = "claude_backfill.rs"]
mod backfill;

#[derive(Default, Debug)]
pub struct SseFrame {
    pub event: String,
    pub id: String,
    pub data: Vec<u8>,
}
/// Incremental framing, including CR, LF, split CRLF and split UTF-8. Neither a
/// partial last record nor an oversized record is treated as a committed event.
#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    frame: SseFrame,
    bytes: usize,
    skip_lf: bool,
}
impl Decoder {
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, String> {
        let mut frames = Vec::new();
        let mut output_bytes = 0usize;
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            self.bytes += 1;
            if self.bytes > crate::agent::MAX_FRAME {
                return Err("Claude event exceeds the stream budget".into());
            }
            if byte == b'\n' || byte == b'\r' {
                if byte == b'\r' {
                    self.skip_lf = true;
                }
                if self.line.is_empty() {
                    if !self.frame.data.is_empty() {
                        if self.frame.data.last() == Some(&b'\n') {
                            self.frame.data.pop();
                        }
                        output_bytes = output_bytes.saturating_add(self.frame.data.len());
                        if output_bytes > crate::agent::MAX_FRAME {
                            return Err("Claude event burst exceeds the byte budget".into());
                        }
                        frames.push(std::mem::take(&mut self.frame));
                        if frames.len() > 256 {
                            return Err("Claude event burst exceeds the stream budget".into());
                        }
                    } else {
                        self.frame = SseFrame::default();
                    }
                    self.bytes = 0;
                } else if self.line[0] != b':' {
                    let line = std::str::from_utf8(&self.line)
                        .map_err(|_| "Claude event is not valid UTF-8")?;
                    let (field, raw) = line.split_once(':').unwrap_or((line, ""));
                    let value = raw.strip_prefix(' ').unwrap_or(raw);
                    match field {
                        "event" | "id" if value.len() > 256 || value.contains('\0') => {
                            return Err("Invalid Claude event identity".into())
                        }
                        "event" => self.frame.event = value.into(),
                        "id" => self.frame.id = value.into(),
                        "data" => {
                            self.frame.data.extend_from_slice(value.as_bytes());
                            self.frame.data.push(b'\n');
                        }
                        _ => {}
                    }
                }
                self.line.clear();
            } else {
                self.line.push(byte);
            }
        }
        Ok(frames)
    }
}

#[derive(Clone, Debug)]
pub struct DeliveryReceipt {
    pub input: String,
    pub processed: bool,
}
#[derive(Debug)]
pub struct Envelope {
    pub sequence: Option<u64>,
    pub event_id: Option<String>,
    pub created_at: Option<String>,
    pub source: String,
    /// None means a filtered event whose cursor can still be committed.
    pub payload: Option<Value>,
    /// Server delivery metadata, never claims copied from a message payload.
    pub deliveries: Vec<DeliveryReceipt>,
    pub receipt_checkpoint: bool,
    /// A local recovery barrier, never supplied by a remote message payload.
    pub recovery: Option<bool>,
}
impl Envelope {
    pub fn needs_commit(&self) -> bool {
        self.sequence.is_some() || self.receipt_checkpoint
    }
    pub fn receipts(deliveries: Vec<DeliveryReceipt>) -> Self {
        Self {
            sequence: None,
            event_id: None,
            created_at: None,
            source: String::new(),
            payload: None,
            deliveries,
            receipt_checkpoint: true,
            recovery: None,
        }
    }
    fn recovery(active: bool) -> Self {
        let mut event = Self::receipts(vec![]);
        event.recovery = Some(active);
        event
    }
}
fn delivery(input: &Value, processed: bool) -> Result<DeliveryReceipt, String> {
    let input = input
        .as_str()
        .filter(|s| s.len() == 36 && uuid::Uuid::parse_str(s).is_ok())
        .ok_or("Invalid Claude delivery identity")?;
    Ok(DeliveryReceipt {
        input: input.into(),
        processed,
    })
}
fn valid_time(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        s.len() <= 64
            && time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                .is_ok()
    })
}
pub fn decode(
    frame: SseFrame,
    identity: &Identity,
    issued: &HashSet<String>,
) -> Result<Option<Envelope>, String> {
    if frame.event == "delivery_update" {
        let value: Value =
            serde_json::from_slice(&frame.data).map_err(|_| "Invalid Claude delivery JSON")?;
        let processed = match value["status"].as_str() {
            Some("DELIVERY_STATUS_PROCESSING") => false,
            Some("DELIVERY_STATUS_PROCESSED") => true,
            _ => return Ok(None), // RECEIVED does not prove model consumption.
        };
        if !value["timestamp"].is_null() && !valid_time(&value["timestamp"]) {
            return Err("Invalid Claude delivery time".into());
        }
        return Ok(Some(Envelope::receipts(vec![delivery(
            &value["event_id"],
            processed,
        )?])));
    }
    if frame.event == "catch_up_truncated" {
        return Err(
            "Claude's saved event range is incomplete; history must catch up before sending".into(),
        );
    }
    if !["client_event", "ephemeral_event"].contains(&frame.event.as_str()) {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_slice(&frame.data).map_err(|_| "Invalid Claude event JSON")?;
    let durable = frame.event == "client_event";
    let sequence = if durable {
        let seq = value["sequence_num"]
            .as_u64()
            .or_else(|| {
                value["sequence_num"]
                    .as_str()
                    .filter(|s| {
                        !s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit())
                    })
                    .and_then(|s| s.parse().ok())
            })
            .ok_or("Claude event has no durable sequence")?;
        if seq == 0
            || seq > i64::MAX as u64
            || (!frame.id.is_empty() && frame.id.parse::<u64>().ok() != Some(seq))
        {
            return Err("Claude event sequence is inconsistent".into());
        }
        Some(seq)
    } else {
        None
    };
    let event_id = if durable {
        Some(
            value["event_id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or("Claude event has no identity")?
                .to_owned(),
        )
    } else {
        None
    };
    let source = value["source"].as_str().unwrap_or("").to_owned();
    let mut payload = value["payload"].clone();
    let kind = payload["type"]
        .as_str()
        .ok_or("Claude event has no type")?
        .to_owned();
    if let Some(session) = payload["session_id"].as_str() {
        if session != identity.bridge_session && session != identity.native_session {
            return Err("Claude event belongs to another session".into());
        }
    }
    let worker = source == "worker";
    let allowed = if !durable {
        worker && kind == "stream_event"
    } else if worker {
        true
    } else if source == "client" {
        match kind.as_str() {
            "user" => {
                payload["tool_use_result"].is_null()
                    && payload["isMeta"] != true
                    && payload["isSynthetic"] != true
                    && payload["parent_tool_use_id"].is_null()
                    && payload["message"]["role"] == "user"
                    && (payload["message"]["content"].is_string()
                        || payload["message"]["content"].as_array().is_some_and(|a| {
                            a.iter().all(|b| {
                                matches!(b["type"].as_str(), Some("text" | "image" | "document"))
                            })
                        }))
                    && (payload["client_platform"] != "unknown"
                        || payload["origin"]["kind"] == "human")
            }
            "control_response" => payload["response"]["request_id"]
                .as_str()
                .is_some_and(|id| !issued.contains(id)),
            "control_request" => {
                payload["request"]["subtype"] == "interrupt"
                    && payload["request_id"]
                        .as_str()
                        .is_some_and(|id| !id.is_empty() && id.len() <= 256)
            }
            _ => false,
        }
    } else {
        false
    };
    if allowed {
        let object = payload.as_object_mut().ok_or("Invalid Claude payload")?;
        object.remove("ccr_event");
        object.remove("_remodex_ccr");
        if !worker && kind == "control_response" {
            let response = object
                .get_mut("response")
                .and_then(Value::as_object_mut)
                .ok_or("Invalid Claude control response")?;
            response.remove("pending_user_dialog_requests");
            response.remove("pending_permission_requests");
        }
    }
    Ok(Some(Envelope {
        sequence,
        event_id,
        created_at: value["created_at"]
            .as_str()
            .filter(|s| s.len() <= 64)
            .map(str::to_owned),
        source,
        payload: allowed.then_some(payload),
        deliveries: if durable
            && value["source"] == "client"
            && kind == "user"
            && (valid_time(&value["processing_at"]) || valid_time(&value["processed_at"]))
        {
            vec![delivery(
                &value["event_id"],
                valid_time(&value["processed_at"]),
            )?]
        } else {
            vec![]
        },
        receipt_checkpoint: false,
        recovery: None,
    }))
}

fn credentials(profile: &Path) -> Result<HeaderMap, String> {
    let path = profile.join(".credentials.json");
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let file = options
        .open(path)
        .map_err(|_| "Sign in to Claude Code in the configured profile before connecting")?;
    let meta = file
        .metadata()
        .map_err(|_| "Claude credentials are unavailable")?;
    if !meta.is_file() || meta.len() > AUTH_LIMIT {
        return Err("Claude credentials must be a small regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 || meta.nlink() != 1
        {
            return Err("Claude credentials must be private to this PC user".into());
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err("Claude credentials cannot be a reparse point".into());
        }
    }
    let mut data = Vec::new();
    file.take(AUTH_LIMIT + 1)
        .read_to_end(&mut data)
        .map_err(|_| "Cannot read Claude credentials")?;
    if data.len() > AUTH_LIMIT as usize {
        return Err("Claude credentials exceed the size limit".into());
    }
    let saved: Value =
        serde_json::from_slice(&data).map_err(|_| "Claude credentials are invalid")?;
    let auth = &saved["claudeAiOauth"];
    if auth["expiresAt"].as_i64().unwrap_or(0) <= crate::auth::now()
        || !auth["scopes"]
            .as_array()
            .is_some_and(|s| s.iter().any(|v| v == "user:sessions:claude_code"))
    {
        return Err(
            "Refresh Claude Code sign-in in the configured profile before connecting".into(),
        );
    }
    let token = auth["accessToken"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 16384)
        .ok_or("Claude sign-in has no session credential")?;
    let mut headers = HeaderMap::new();
    let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "Invalid Claude credential")?;
    authorization.set_sensitive(true);
    headers.insert("authorization", authorization);
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    headers.insert(
        "anthropic-client-platform",
        HeaderValue::from_static("android"),
    );
    if let Some(token) = saved["trustedDeviceToken"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        let mut token =
            HeaderValue::from_str(token).map_err(|_| "Invalid Claude device credential")?;
        token.set_sensitive(true);
        headers.insert("X-Trusted-Device-Token", token);
    }
    Ok(headers)
}
async fn checked_json(mut response: Response) -> Result<Value, String> {
    checked_json_limit(&mut response, METADATA_LIMIT).await
}
async fn checked_json_limit(response: &mut Response, limit: usize) -> Result<Value, String> {
    status(response.status().as_u16())?;
    let mut data = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Claude metadata connection failed")?
    {
        if data.len() + chunk.len() > limit {
            return Err("Claude metadata exceeds the response budget".into());
        }
        data.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&data).map_err(|_| "Claude returned invalid metadata".into())
}
fn status(code: u16) -> Result<(), String> {
    if (200..300).contains(&code) {
        return Ok(());
    }
    Err(match code {
        401 | 403 => "Claude requires sign-in or device verification on this PC",
        404 => "Claude's shared connection is no longer available; history is retained",
        429 => "Claude is limiting connection requests; retry later",
        _ => "Claude Remote Control is temporarily unavailable",
    }
    .into())
}
pub struct Remote {
    client: Client,
    url: String,
    identity: Identity,
    pub metadata: Value,
    response: Response,
    decoder: Decoder,
    buffered: std::collections::VecDeque<SseFrame>,
    issued: HashSet<String>,
    last_seen: u64,
    health_at: tokio::time::Instant,
    backfill: Option<backfill::Gap>,
    recovery_notice: bool,
}
impl Remote {
    #[cfg(test)]
    pub(crate) async fn fixture(
        base: &str,
        identity: Identity,
        after: u64,
    ) -> Result<Self, String> {
        Self::open_at(base, HeaderMap::new(), identity, after).await
    }
    pub async fn open(profile: &Path, identity: Identity, after: u64) -> Result<Self, String> {
        identity.validate()?;
        let headers = credentials(profile)?;
        Self::open_at(API, headers, identity, after).await
    }
    async fn open_at(
        base: &str,
        headers: HeaderMap,
        identity: Identity,
        after: u64,
    ) -> Result<Self, String> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(12))
            .default_headers(headers)
            .pool_max_idle_per_host(2)
            .build()
            .map_err(|_| "Cannot initialize Claude connection")?;
        let url = format!("{base}{}", identity.bridge_session);
        let metadata = tokio::time::timeout(Duration::from_secs(12), async {
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|_| "Cannot reach Claude Remote Control")?;
            checked_json(response).await
        })
        .await
        .map_err(|_| "Claude metadata timed out")??;
        let metadata = metadata["response_shape"].clone();
        if metadata["id"] != identity.bridge_session {
            return Err("Claude returned another shared session identity".into());
        }
        if metadata["connection_status"] != "connected" {
            return Err("The original Claude worker is disconnected; history is retained".into());
        }
        let mut request = client
            .get(format!("{url}/events/stream"))
            .header("accept", "text/event-stream");
        if after > 0 {
            request = request.header("Last-Event-ID", after.to_string());
        }
        let response = tokio::time::timeout(Duration::from_secs(12), request.send())
            .await
            .map_err(|_| "Claude event stream timed out")?
            .map_err(|_| "Cannot connect Claude event stream")?;
        status(response.status().as_u16())?;
        if !response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| h.starts_with("text/event-stream"))
        {
            return Err("Claude returned an invalid stream type".into());
        }
        Ok(Self {
            client,
            url,
            identity,
            metadata,
            response,
            decoder: Decoder::default(),
            buffered: Default::default(),
            issued: Default::default(),
            last_seen: after,
            health_at: tokio::time::Instant::now() + Duration::from_secs(15),
            backfill: None,
            recovery_notice: false,
        })
    }
    #[cfg(test)]
    pub(crate) fn check_health_now(&mut self) {
        self.health_at = tokio::time::Instant::now();
    }
    pub(crate) fn recover_history(&mut self) {
        self.backfill = Some(backfill::Gap::new(self.last_seen));
        self.recovery_notice = true;
    }
    async fn verify_worker(&self) -> Result<(), String> {
        let metadata = tokio::time::timeout(Duration::from_secs(12), async {
            let response = self
                .client
                .get(&self.url)
                .send()
                .await
                .map_err(|_| "Cannot confirm the original Claude worker connection")?;
            checked_json(response).await
        })
        .await
        .map_err(|_| "Claude worker verification timed out")??;
        if metadata["response_shape"]["id"] != self.identity.bridge_session
            || metadata["response_shape"]["connection_status"] != "connected"
        {
            return Err(
                "The original Claude worker disconnected; saved history is retained".into(),
            );
        }
        Ok(())
    }
    pub async fn next(&mut self) -> Result<Envelope, String> {
        loop {
            if self.recovery_notice {
                self.recovery_notice = false;
                return Ok(Envelope::recovery(true));
            }
            if let Some(gap) = self.backfill.as_mut() {
                // Gap owns its page/cursor across cancellation by the actor.
                // The caller still commits each row before requesting the next.
                if let Some(event) = gap
                    .next(&self.client, &self.url, &self.identity, &self.issued)
                    .await?
                {
                    self.last_seen = event.sequence.unwrap();
                    return Ok(event);
                }
                self.verify_worker().await?;
                self.health_at = tokio::time::Instant::now() + Duration::from_secs(15);
                self.backfill = None;
                return Ok(Envelope::recovery(false));
            }
            if tokio::time::Instant::now() >= self.health_at {
                self.health_at = tokio::time::Instant::now() + Duration::from_secs(15);
                self.verify_worker().await?;
            }
            if let Some(frame) = self.buffered.pop_front() {
                if frame.event == "catch_up_truncated" {
                    self.backfill = Some(backfill::Gap::new(self.last_seen));
                    return Ok(Envelope::recovery(true));
                }
                // A missing range can also occur without a truncation notice.
                // Keep the original frame until the authoritative REST history
                // has filled the gap, then discard its duplicate normally.
                if frame.event == "client_event" {
                    let value: Value = serde_json::from_slice(&frame.data)
                        .map_err(|_| "Invalid Claude event JSON")?;
                    if backfill::sequence(&value)? > self.last_seen.saturating_add(1) {
                        self.buffered.push_front(frame);
                        self.backfill = Some(backfill::Gap::new(self.last_seen));
                        return Ok(Envelope::recovery(true));
                    }
                }
                let Some(envelope) = decode(frame, &self.identity, &self.issued)? else {
                    continue;
                };
                if let Some(sequence) = envelope.sequence {
                    if sequence <= self.last_seen {
                        continue;
                    }
                    self.last_seen = sequence;
                }
                if envelope.source == "worker" {
                    if let Some(id) = envelope
                        .payload
                        .as_ref()
                        .and_then(|v| v["response"]["request_id"].as_str())
                    {
                        self.issued.remove(id);
                    }
                }
                return Ok(envelope);
            }
            let next_chunk = tokio::select! {
                _ = tokio::time::sleep_until(self.health_at) => continue,
                chunk = tokio::time::timeout(Duration::from_secs(45), self.response.chunk()) => chunk,
            };
            let chunk = next_chunk
                .map_err(|_| "Claude event stream became silent")?
                .map_err(|_| "Claude event stream disconnected")?
                .ok_or("Claude event stream closed")?;
            self.buffered.extend(self.decoder.push(&chunk)?);
        }
    }
    pub async fn send(&mut self, payload: Value) -> Result<(), String> {
        self.prepare_send(payload)?.deliver().await
    }
    /// Delivery SSE is ephemeral. Recover only unresolved local UUIDs from
    /// bounded server history; no prompt is ever replayed to discover its fate.
    pub async fn delivery_snapshot(
        &self,
        inputs: &[String],
    ) -> Result<Vec<DeliveryReceipt>, String> {
        if inputs.len() > 64 {
            return Err("Too many unresolved Claude deliveries".into());
        }
        let mut remaining: HashSet<_> = inputs.iter().cloned().collect();
        let mut found = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..32 {
            if remaining.is_empty() {
                break;
            }
            let mut request = self
                .client
                .get(format!("{}/events", self.url))
                .query(&[("limit", "50")]);
            if let Some(cursor) = &cursor {
                request = request.query(&[("cursor", cursor)]);
            }
            let page = tokio::time::timeout(Duration::from_secs(12), async {
                let response = request
                    .send()
                    .await
                    .map_err(|_| "Cannot recover Claude delivery receipts")?;
                checked_json_limit(&mut { response }, crate::agent::MAX_FRAME * 2).await
            })
            .await
            .map_err(|_| "Claude receipt recovery timed out")??;
            let rows = page["data"]
                .as_array()
                .filter(|a| a.len() <= 50)
                .ok_or("Invalid Claude receipt page")?;
            for row in rows {
                let Some(id) = row["event_id"].as_str() else {
                    continue;
                };
                if row["source"] == "client"
                    && row["payload"]["type"] == "user"
                    && remaining.remove(id)
                    && (valid_time(&row["processing_at"]) || valid_time(&row["processed_at"]))
                {
                    found.push(delivery(
                        &row["event_id"],
                        valid_time(&row["processed_at"]),
                    )?);
                }
            }
            let next = page["resume_cursor"].as_str().filter(|s| {
                !s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit())
            });
            if rows.is_empty() || next.is_none() {
                break;
            }
            if cursor.as_deref().is_some_and(|old| {
                next.and_then(|n| n.parse::<u64>().ok()) >= old.parse::<u64>().ok()
            }) {
                return Err("Claude receipt pagination did not advance".into());
            }
            cursor = next.map(str::to_owned);
        }
        Ok(found)
    }
    /// Release only a timed-out metadata read. This is local bookkeeping, not
    /// a remote cancellation or permission to repeat a prompt/mutation.
    pub fn forget_read(&mut self, id: &str) -> Result<(), String> {
        if !(id.starts_with("capabilities-") || id.starts_with("context-")) {
            return Err("Only metadata read identities can expire locally".into());
        }
        self.issued.remove(id);
        Ok(())
    }
    /// Reserve a command identity before starting I/O. Delivery owns its bounded
    /// body, so a slow POST does not prevent reads or durable stream commits.
    pub fn prepare_send(&mut self, mut payload: Value) -> Result<Delivery, String> {
        let kind = payload["type"].as_str().ok_or("Invalid remote command")?;
        if ![
            "user",
            "control_request",
            "control_response",
            "control_cancel_request",
        ]
        .contains(&kind)
        {
            return Err("Unsupported remote command".into());
        }
        if let Some(session) = payload["session_id"].as_str() {
            if session != self.identity.native_session && session != self.identity.bridge_session {
                return Err("Remote command targets another session".into());
            }
        }
        let control_id = if kind == "control_request" {
            if matches!(
                payload["request"]["subtype"].as_str(),
                Some("remote_control" | "rewind_files")
            ) {
                return Err("This control requires the original session owner".into());
            }
            let id = payload["request_id"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .ok_or("Remote control requires an identity")?;
            if self.issued.len() >= 64 {
                return Err("Too many pending Claude controls".into());
            }
            if self.issued.contains(id) {
                return Err("Claude control has already been sent".into());
            }
            Some(id.to_owned())
        } else {
            None
        };
        if payload["uuid"].is_null() {
            payload["uuid"] = json!(uuid::Uuid::new_v4().to_string());
        }
        let body = serde_json::to_vec(
            &json!({"session_id":self.identity.bridge_session,"events":[{"payload":payload}]}),
        )
        .map_err(|_| "Invalid remote command")?;
        if body.len() > crate::agent::MAX_INPUT_FRAME {
            return Err("Remote command exceeds its size limit".into());
        }
        if let Some(id) = control_id {
            self.issued.insert(id);
        }
        Ok(Delivery {
            client: self.client.clone(),
            url: format!("{}/events", self.url),
            body,
        })
    }
}

pub struct Delivery {
    client: Client,
    url: String,
    body: Vec<u8>,
}
impl Delivery {
    pub fn bytes(&self) -> usize {
        self.body.len()
    }
    pub async fn deliver(self) -> Result<(), String> {
        // A timeout can occur after acceptance. No automatic POST retry.
        let response = tokio::time::timeout(
            Duration::from_secs(20),
            self.client
                .post(self.url)
                .header("content-type", "application/json")
                .body(self.body)
                .send(),
        )
        .await
        .map_err(|_| "Claude delivery is uncertain; it was not resent")?
        .map_err(|_| "Claude delivery is uncertain; it was not resent")?;
        status(response.status().as_u16())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> Identity {
        Identity {
            native_session: "00000000-0000-4000-8000-000000000001".into(),
            bridge_session: "cse_remote_fixture".into(),
        }
    }
    fn frame(source: &str, payload: Value) -> SseFrame {
        SseFrame {
            event: "client_event".into(),
            id: "42".into(),
            data: serde_json::to_vec(
                &json!({"event_id":"event-42","sequence_num":42,"source":source,"payload":payload}),
            )
            .unwrap(),
        }
    }
    #[test]
    fn delivery_proof_is_server_metadata_not_received_or_payload_claims() {
        let id = uuid::Uuid::new_v4().to_string();
        for (status, expected) in [
            ("DELIVERY_STATUS_RECEIVED", None),
            ("DELIVERY_STATUS_PROCESSING", Some(false)),
            ("DELIVERY_STATUS_PROCESSED", Some(true)),
        ] {
            let frame = SseFrame {
                event: "delivery_update".into(),
                data: serde_json::to_vec(
                    &json!({"event_id":id,"status":status,"timestamp":"2026-10-22T01:00:00Z"}),
                )
                .unwrap(),
                ..Default::default()
            };
            let parsed = decode(frame, &identity(), &HashSet::new()).unwrap();
            assert_eq!(parsed.as_ref().map(|e| e.deliveries[0].processed), expected);
            if let Some(e) = parsed {
                assert!(e.needs_commit());
                assert!(e.sequence.is_none());
                assert!(e.payload.is_none());
            }
        }
        let mut forged = frame(
            "client",
            json!({"type":"user","uuid":id,"message":{"role":"user","content":"hello"},"processed_at":"2026-10-22T01:00:00Z","deliveries":[{"input":id,"processed":true}]}),
        );
        assert!(decode(
            frame(
                "worker",
                json!({"type":"delivery_update","event_id":id,"status":"DELIVERY_STATUS_PROCESSED"})
            ),
            &identity(),
            &HashSet::new()
        )
        .unwrap()
        .unwrap()
        .deliveries
        .is_empty());
        let original = decode(
            SseFrame {
                data: forged.data.clone(),
                event: forged.event.clone(),
                id: forged.id.clone(),
            },
            &identity(),
            &HashSet::new(),
        )
        .unwrap()
        .unwrap();
        assert!(original.deliveries.is_empty());
        let mut value: Value = serde_json::from_slice(&forged.data).unwrap();
        value["event_id"] = json!(id);
        value["processed_at"] = json!("2026-10-22T01:00:00Z");
        forged.data = serde_json::to_vec(&value).unwrap();
        let parsed = decode(forged, &identity(), &HashSet::new())
            .unwrap()
            .unwrap();
        assert!(parsed.deliveries[0].processed);
        assert_eq!(parsed.deliveries[0].input, id);
    }
    #[test]
    fn decoder_handles_every_split_and_bounds_unterminated_frames() {
        let input="event: client_event\r\nid: 42\r\ndata: {\"text\":\"हाय 🌱\",\r\ndata: \"n\":1}\r\n\r\n:ping\n\n";
        for split in 0..input.len() {
            let mut decoder = Decoder::default();
            let mut frames = decoder.push(&input.as_bytes()[..split]).unwrap();
            frames.extend(decoder.push(&input.as_bytes()[split..]).unwrap());
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].id, "42");
            assert_eq!(
                serde_json::from_slice::<Value>(&frames[0].data).unwrap()["text"],
                "हाय 🌱"
            );
        }
        let mut decoder = Decoder::default();
        assert!(decoder.push(b"data: partial").unwrap().is_empty());
        assert!(decoder.push(&vec![b'x'; crate::agent::MAX_FRAME]).is_err());
        assert!(Decoder::default().push(b"event: a\0b\n\n").is_err());
        let frames = Decoder::default()
            .push(b"event: client_event\rid: 7\rdata: {}\r\r")
            .unwrap();
        assert_eq!(frames.len(), 1);
    }
    #[test]
    fn envelopes_filter_other_clients_and_reject_wrong_identity_or_truncation() {
        let mut issued = HashSet::new();
        issued.insert("our-request".into());
        let id = identity();
        for kind in ["assistant", "result", "control_request", "stream_event"] {
            let e = decode(frame("client", json!({"type":kind})), &id, &issued)
                .unwrap()
                .unwrap();
            assert_eq!(e.sequence, Some(42));
            assert!(e.payload.is_none());
        }
        let reply = json!({"type":"control_response","response":{"request_id":"our-request","subtype":"success"}});
        assert!(decode(frame("client", reply.clone()), &id, &issued)
            .unwrap()
            .unwrap()
            .payload
            .is_none());
        assert!(decode(frame("worker", reply), &id, &issued)
            .unwrap()
            .unwrap()
            .payload
            .is_some());
        let resolution = json!({"type":"control_response","response":{"request_id":"permission-1","pending_permission_requests":[{"secret":"x"}],"subtype":"success"}});
        assert!(decode(frame("client", resolution), &id, &issued)
            .unwrap()
            .unwrap()
            .payload
            .unwrap()["response"]["pending_permission_requests"]
            .is_null());
        let prompt = json!({"type":"user","uuid":id.native_session,"message":{"role":"user","content":"hello"}});
        assert!(decode(frame("client", prompt.clone()), &id, &issued)
            .unwrap()
            .unwrap()
            .payload
            .is_some());
        let human = json!({"type":"user","uuid":id.native_session,"client_platform":"unknown","origin":{"kind":"human"},"message":{"role":"user","content":"real human message"}});
        assert!(decode(frame("client", human), &id, &issued)
            .unwrap()
            .unwrap()
            .payload
            .is_some());
        let mut bad = prompt;
        bad["message"]["content"] = json!([{"type":"tool_result","content":"private"}]);
        assert!(decode(frame("client", bad), &id, &issued)
            .unwrap()
            .unwrap()
            .payload
            .is_none());
        assert!(decode(
            frame("worker", json!({"type":"assistant","session_id":"other"})),
            &id,
            &issued
        )
        .is_err());
        let mut bad = frame("worker", json!({"type":"result"}));
        bad.id = "43".into();
        assert!(decode(bad, &id, &issued).is_err());
        let mut real = frame("worker", json!({"type":"result"}));
        let mut body: Value = serde_json::from_slice(&real.data).unwrap();
        body["sequence_num"] = json!("42");
        real.data = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            decode(real, &id, &issued).unwrap().unwrap().sequence,
            Some(42)
        );
        assert!(decode(
            SseFrame {
                event: "catch_up_truncated".into(),
                ..Default::default()
            },
            &id,
            &issued
        )
        .is_err());
    }
    #[test]
    fn credentials_are_bounded_private_expiry_checked_and_never_in_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let write = |expires: i64| {
            std::fs::write(&path,json!({"claudeAiOauth":{"accessToken":"PRIVATE_TEST_CREDENTIAL","expiresAt":expires,"scopes":["user:sessions:claude_code"]}}).to_string()).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        };
        write(0);
        let error = credentials(dir.path()).unwrap_err();
        assert!(!error.contains("PRIVATE_TEST"));
        write(crate::auth::now() + 60000);
        let headers = credentials(dir.path()).unwrap();
        assert!(headers["authorization"].is_sensitive());
        assert_eq!(headers["anthropic-client-platform"], "android");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(credentials(dir.path()).is_err());
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink("missing", &path).unwrap();
            assert!(credentials(dir.path()).is_err());
        }
    }
    #[tokio::test]
    async fn observer_sends_once_uses_resume_cursor_and_close_never_controls_owner() {
        use axum::{
            extract::State,
            http::{HeaderMap as Headers, StatusCode},
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{Arc, Mutex};
        let posts = Arc::new(Mutex::new(Vec::<Value>::new()));
        let app=Router::new().route("/cse_remote_fixture",get(||async {Json(json!({"response_shape":{"id":"cse_remote_fixture","connection_status":"connected","worker_status":"idle"}}))}))
            .route("/cse_remote_fixture/events/stream",get(|headers:Headers|async move {
                assert_eq!(headers["last-event-id"],"41");
                ([("content-type","text/event-stream")],format!("event: client_event\nid: 42\ndata: {}\n\n",json!({"sequence_num":42,"event_id":"remote-event","source":"worker","payload":{"type":"result"}})))
            }))
            .route("/cse_remote_fixture/events",post(|State(posts):State<Arc<Mutex<Vec<Value>>>>,Json(value):Json<Value>|async move {posts.lock().unwrap().push(value);StatusCode::SERVICE_UNAVAILABLE})).with_state(posts.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut remote = Remote::open_at(&base, HeaderMap::new(), identity(), 41)
            .await
            .unwrap();
        assert_eq!(remote.next().await.unwrap().sequence, Some(42));
        assert!(remote
            .send(json!({"type":"user","message":{"role":"user","content":"hello"}}))
            .await
            .is_err());
        assert_eq!(posts.lock().unwrap().len(), 1);
        assert!(remote.send(json!({"type":"control_request","request_id":"bad","request":{"subtype":"remote_control","enabled":false}})).await.is_err());
        drop(remote);
        assert_eq!(posts.lock().unwrap().len(), 1);
        let request = &posts.lock().unwrap()[0];
        assert_eq!(request["session_id"], "cse_remote_fixture");
        assert!(request["events"][0]["payload"]["uuid"].is_string());
        task.abort();
    }
}
