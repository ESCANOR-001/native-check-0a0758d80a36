//! Ascending, bounded catch-up for a truncated Remote Control stream. Only a
//! single page is held in memory. A fixed high-water mark prevents an active
//! conversation from making recovery chase an indefinitely growing history.
use super::{checked_json_limit, decode, Envelope, SseFrame};
use crate::claude_host::Identity;
use reqwest::Client;
use serde_json::Value;
use std::{
    collections::{HashSet, VecDeque},
    time::Duration,
};

const PAGE_BYTES: usize = crate::agent::MAX_FRAME * 2;

pub(super) fn sequence(row: &Value) -> Result<u64, String> {
    let n = row["sequence_num"].as_u64().or_else(|| {
        row["sequence_num"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|s| s.parse().ok())
    });
    n.filter(|n| *n > 0 && *n <= i64::MAX as u64)
        .ok_or_else(|| "Invalid Claude history sequence".into())
}

pub(super) struct Gap {
    after: u64,
    upper: Option<u64>,
    page: VecDeque<Value>,
    page_size: usize,
}
impl Gap {
    pub fn new(after: u64) -> Self {
        Self {
            after,
            upper: None,
            page: VecDeque::new(),
            page_size: 50,
        }
    }
    async fn page(client: &Client, url: &str, params: &[(&str, String)]) -> Result<Value, String> {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut response = client
                .get(format!("{url}/events"))
                .query(params)
                .send()
                .await
                .map_err(|_| "Cannot recover Claude history")?;
            checked_json_limit(&mut response, PAGE_BYTES).await
        })
        .await
        .map_err(|_| "Claude history recovery timed out")?
    }
    pub async fn next(
        &mut self,
        client: &Client,
        url: &str,
        identity: &Identity,
        issued: &HashSet<String>,
    ) -> Result<Option<Envelope>, String> {
        if self.upper.is_none() {
            let page = Self::page(client, url, &[("limit", "1".into())]).await?;
            let rows = page["data"]
                .as_array()
                .filter(|r| r.len() <= 1)
                .ok_or("Invalid Claude history boundary")?;
            let upper = rows.first().map(sequence).transpose()?.unwrap_or(0);
            if upper < self.after {
                return Err("Claude history is shorter than its saved cursor".into());
            }
            self.upper = Some(upper);
        }
        let upper = self.upper.unwrap();
        if self.after == upper {
            return Ok(None);
        }
        while self.page.is_empty() {
            let result = Self::page(
                client,
                url,
                &[
                    ("limit", self.page_size.to_string()),
                    ("sort_order", "asc".into()),
                    ("cursor", self.after.to_string()),
                ],
            )
            .await;
            let mut page = match result {
                Err(e)
                    if e == "Claude metadata exceeds the response budget" && self.page_size > 1 =>
                {
                    self.page_size = self.page_size.div_ceil(2);
                    continue;
                }
                other => other?,
            };
            let rows = page["data"]
                .as_array_mut()
                .filter(|r| !r.is_empty() && r.len() <= self.page_size)
                .ok_or("Claude history gap is unavailable; saved messages are retained")?;
            let mut previous = self.after;
            // Validate the entire page before emitting any of it. A stale or
            // wrong-direction page cannot silently skip uncommitted messages.
            for row in rows.iter() {
                let n = sequence(row)?;
                if n != previous + 1 {
                    return Err("Claude history page has a missing or reordered event".into());
                }
                previous = n;
            }
            let selected: VecDeque<_> = std::mem::take(rows)
                .into_iter()
                .take_while(|r| sequence(r).is_ok_and(|n| n <= upper))
                .collect();
            if selected.is_empty() {
                return Err("Claude history did not reach its recovery boundary".into());
            }
            self.page = selected;
        }
        let row = self.page.front().unwrap();
        let n = sequence(row)?;
        let data = serde_json::to_vec(row).map_err(|_| "Invalid Claude history record")?;
        if data.len() > crate::agent::MAX_FRAME {
            return Err("Claude history event exceeds the stream budget".into());
        }
        let envelope = decode(
            SseFrame {
                event: "client_event".into(),
                id: n.to_string(),
                data,
            },
            identity,
            issued,
        )?
        .ok_or("Claude history record was not durable")?;
        // No await between removing a row and returning it to the observer.
        // Its durable cursor is advanced only by the normal atomic commit.
        self.page.pop_front();
        self.after = n;
        Ok(Some(envelope))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_remote::Remote;
    use axum::{
        extract::{Query, State},
        response::{IntoResponse, Response},
        routing::get,
        Json, Router,
    };
    use serde_json::json;
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    };

    struct Fixture {
        requests: Mutex<Vec<(usize, u64)>>,
        delay: AtomicBool,
        bad_page: AtomicBool,
        oversized: AtomicBool,
        no_notice: AtomicBool,
    }
    fn identity() -> Identity {
        Identity {
            native_session: "00000000-0000-4000-8000-000000000001".into(),
            bridge_session: "cse_backfill".into(),
        }
    }
    fn row(n: u64) -> Value {
        json!({"sequence_num":n,"event_id":format!("event-{n}"),"source":"worker",
            "payload":{"type":"assistant","message":{"content":[{"type":"text","text":format!("message {n}")}]}}})
    }
    async fn page(
        State(s): State<Arc<Fixture>>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        if q.get("sort_order").map(String::as_str) != Some("asc") {
            return Json(json!({"data":[row(120)],"resume_cursor":"120"})).into_response();
        }
        let after: u64 = q["cursor"].parse().unwrap();
        let limit: usize = q["limit"].parse().unwrap();
        s.requests.lock().unwrap().push((limit, after));
        if s.oversized.load(Ordering::SeqCst) && limit > 25 {
            return "x".repeat(PAGE_BYTES + 1).into_response();
        }
        if s.delay.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        let mut rows: Vec<_> = ((after + 1)..=120).take(limit).map(row).collect();
        if s.bad_page.load(Ordering::SeqCst) {
            rows[0] = row(after + 2);
        }
        Json(
            json!({"resume_cursor":rows.last().map(|r|sequence(r).unwrap().to_string()),"data":rows}),
        ).into_response()
    }
    async fn fixture() -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
        let state = Arc::new(Fixture {
            requests: Mutex::new(vec![]),
            delay: AtomicBool::new(false),
            bad_page: AtomicBool::new(false),
            oversized: AtomicBool::new(false),
            no_notice: AtomicBool::new(false),
        });
        let app=Router::new().route("/cse_backfill",get(||async {Json(json!({"response_shape":{"id":"cse_backfill","connection_status":"connected"}}))}))
            .route("/cse_backfill/events",get(page))
            .route("/cse_backfill/events/stream",get(|State(s):State<Arc<Fixture>>|async move {
                let notice=if s.no_notice.load(Ordering::SeqCst) {""} else {"event: catch_up_truncated\ndata: {}\n\n"};
                ([("content-type","text/event-stream")],format!("{notice}event: client_event\nid: 120\ndata: {}\n\nevent: client_event\nid: 121\ndata: {}\n\n",row(120),row(121)))
            })).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, state, task)
    }
    #[tokio::test]
    async fn truncated_stream_backfills_in_order_with_barriers_and_deduplicates_live_tail() {
        let (url, state, task) = fixture().await;
        let mut remote = Remote::fixture(&url, identity(), 0).await.unwrap();
        assert_eq!(remote.next().await.unwrap().recovery, Some(true));
        for n in 1..=120 {
            assert_eq!(remote.next().await.unwrap().sequence, Some(n));
        }
        assert_eq!(remote.next().await.unwrap().recovery, Some(false));
        assert_eq!(remote.next().await.unwrap().sequence, Some(121));
        assert_eq!(
            *state.requests.lock().unwrap(),
            vec![(50, 0), (50, 50), (50, 100)]
        );
        task.abort();
    }
    #[tokio::test]
    async fn cancelled_page_fetch_resumes_from_saved_boundary_without_skipping_and_invalid_page_is_atomic(
    ) {
        let (base, state, task) = fixture().await;
        let client = Client::new();
        let url = format!("{base}cse_backfill");
        let mut gap = Gap::new(20);
        state.delay.store(true, Ordering::SeqCst);
        assert!(tokio::time::timeout(
            Duration::from_millis(40),
            gap.next(&client, &url, &identity(), &HashSet::new())
        )
        .await
        .is_err());
        assert_eq!(gap.after, 20);
        assert_eq!(gap.upper, Some(120));
        assert!(gap.page.is_empty());
        state.delay.store(false, Ordering::SeqCst);
        state.bad_page.store(true, Ordering::SeqCst);
        assert!(gap
            .next(&client, &url, &identity(), &HashSet::new())
            .await
            .is_err());
        assert_eq!(gap.after, 20);
        state.bad_page.store(false, Ordering::SeqCst);
        assert_eq!(
            gap.next(&client, &url, &identity(), &HashSet::new())
                .await
                .unwrap()
                .unwrap()
                .sequence,
            Some(21)
        );
        assert_eq!(gap.page.len(), 49);
        assert!(state.requests.lock().unwrap().iter().all(|(_, c)| *c == 20));
        task.abort();
    }
    #[tokio::test]
    async fn unannounced_gap_is_repaired_and_oversized_pages_shrink_without_replaying_inputs() {
        let (url, state, task) = fixture().await;
        state.no_notice.store(true, Ordering::SeqCst);
        state.oversized.store(true, Ordering::SeqCst);
        let mut remote = Remote::fixture(&url, identity(), 0).await.unwrap();
        assert_eq!(remote.next().await.unwrap().recovery, Some(true));
        for n in 1..=120 {
            assert_eq!(remote.next().await.unwrap().sequence, Some(n));
        }
        assert_eq!(remote.next().await.unwrap().recovery, Some(false));
        assert_eq!(remote.next().await.unwrap().sequence, Some(121));
        assert_eq!(
            *state.requests.lock().unwrap(),
            vec![(50, 0), (25, 0), (25, 25), (25, 50), (25, 75), (25, 100)]
        );
        task.abort();
    }
}
