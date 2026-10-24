//! Bounded saved-history reads for a Desktop-owned Codex chat. This private
//! app-server is only a reader: no resume, start, interrupt or approval RPC is
//! exposed. Execution stays with the original Desktop owner.
use crate::{
    agent::{Agent, Connection, Event, Launch},
    chat::{AgentConfig, AgentKind},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader, Read},
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const ERROR: &str = "Codex saved history could not be verified; the original Desktop chat was not resumed or changed";
const TOTAL: u64 = 512 * 1024 * 1024;
const LINE: u64 = 8 * 1024 * 1024;

pub async fn capture_from_config(
    configuration: &Path,
    native: &str,
    destination: &Path,
) -> Result<Value, String> {
    let bytes = crate::profile_snapshot::bytes(configuration, 262144).map_err(|_| ERROR)?;
    let config: crate::chat::Config = serde_json::from_slice(&bytes).map_err(|_| ERROR)?;
    config.validate()?;
    capture(&config.codex, &config.workspace, native, destination).await
}

pub struct Reader {
    connection: Connection,
}
impl Reader {
    pub async fn open(config: &AgentConfig, cwd: &Path) -> Result<Self, String> {
        let connection = Connection::start(Launch {
            agent: Agent::Codex,
            executable: config.executable.clone(),
            cwd: cwd.into(),
            env: crate::discovery::environment(AgentKind::Codex, &config.profile),
            extra_args: crate::discovery::isolated_args(AgentKind::Codex),
        })
        .map_err(|_| ERROR)?;
        let mut reader = Self { connection };
        let result=async {
            reader.rpc("initialize",json!({"clientInfo":{"name":"remodex_saved_history_reader","version":"0.0.0"},"capabilities":{"experimentalApi":true}})).await?;
            reader.connection.send(&json!({"method":"initialized","params":{}})).await.map_err(|_|ERROR)?;
            Ok::<(),String>(())
        }.await;
        if let Err(error) = result {
            reader.close().await;
            return Err(error);
        }
        Ok(reader)
    }
    // Private method; callers cannot use this reader as a second writer.
    pub(crate) async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        if !matches!(
            method,
            "initialize"
                | "thread/read"
                | "thread/turns/list"
                | "thread/items/list"
                | "thread/loaded/list"
                | "model/list"
                | "account/rateLimits/read"
        ) {
            return Err(ERROR.into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        tokio::time::timeout(Duration::from_secs(12), async {
            self.connection
                .send(&json!({"id":id,"method":method,"params":params}))
                .await
                .map_err(|_| ERROR)?;
            for _ in 0..256 {
                match self.connection.events.recv().await {
                    Some(Event::Message(v)) if v["id"] == id && v.get("method").is_none() => {
                        if v.get("error").is_some() {
                            return Err(ERROR.into());
                        }
                        return v.get("result").cloned().ok_or_else(|| ERROR.into());
                    }
                    Some(Event::Message(_)) => {}
                    _ => return Err(ERROR.into()),
                }
            }
            Err(ERROR.into())
        })
        .await
        .map_err(|_| ERROR)?
    }
    pub async fn metadata(&mut self, native: &str) -> Result<Value, String> {
        if uuid::Uuid::parse_str(native).is_err() {
            return Err(ERROR.into());
        }
        let result = self
            .rpc(
                "thread/read",
                json!({"threadId":native,"includeTurns":false}),
            )
            .await?;
        let thread = &result["thread"];
        if thread["id"] != native
            || !thread.is_object()
            || thread["turns"].as_array().is_some_and(|t| !t.is_empty())
        {
            return Err(ERROR.into());
        }
        Ok(thread.clone())
    }
    pub async fn turns(&mut self, native: &str, cursor: Option<&str>) -> Result<Value, String> {
        self.rpc(
            "thread/turns/list",
            json!({"threadId":native,"cursor":cursor,"limit":1,"sortDirection":"asc"}),
        )
        .await
    }
    pub async fn items(
        &mut self,
        native: &str,
        turn: &str,
        cursor: Option<&str>,
    ) -> Result<Value, String> {
        self.rpc("thread/items/list",json!({"threadId":native,"turnId":turn,"cursor":cursor,"limit":8,"sortDirection":"asc"})).await
    }
    pub async fn close(self) {
        self.connection.close().await;
    }
}

fn cursor(value: &Value) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 8192 && !v.chars().any(char::is_control))
        .map(|v| Some(v.into()))
        .ok_or_else(|| ERROR.into())
}
fn identifier(value: &Value) -> Result<&str, String> {
    value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
        .ok_or_else(|| ERROR.into())
}
fn fingerprint(profile: &Path, path: &Path, native: &str) -> Result<(u64, String), String> {
    // Windows APIs disagree on the verbatim prefix. Normalize only that
    // spelling; component-by-component opens below still reject redirects.
    #[cfg(windows)]
    fn dos(path: &Path) -> PathBuf {
        let value = path.to_string_lossy();
        if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            PathBuf::from(format!(r"\\{unc}"))
        } else {
            PathBuf::from(value.strip_prefix(r"\\?\").unwrap_or(&value))
        }
    }
    #[cfg(windows)]
    let (profile, path) = (dos(profile), dos(path));
    #[cfg(windows)]
    let (profile, path) = (profile.as_path(), path.as_path());
    let relative = path.strip_prefix(profile).map_err(|_| ERROR)?;
    let parts: Vec<_> = relative.components().collect();
    if parts.len() < 2
        || !matches!(parts.first(),Some(Component::Normal(p)) if *p=="sessions" || *p=="archived_sessions")
        || parts.iter().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ERROR.into());
    }
    let mut checked = profile.to_owned();
    crate::storage_stage::open(&checked, true).map_err(|_| ERROR)?;
    for (i, part) in parts.iter().enumerate() {
        checked.push(part);
        crate::storage_stage::open(&checked, i + 1 < parts.len()).map_err(|_| ERROR)?;
    }
    let (file, meta) = crate::profile_snapshot::open(&checked, TOTAL).map_err(|_| ERROR)?;
    let identity = crate::storage_stage::identity(&file).map_err(|_| ERROR)?;
    let mut input = BufReader::with_capacity(65536, file);
    let mut first = Vec::new();
    input
        .by_ref()
        .take(LINE + 1)
        .read_until(b'\n', &mut first)
        .map_err(|_| ERROR)?;
    if first.len() as u64 > LINE || first.last() != Some(&b'\n') {
        return Err(ERROR.into());
    }
    let header: Value = serde_json::from_slice(&first).map_err(|_| ERROR)?;
    if header["type"] != "session_meta" || header["payload"]["id"] != native {
        return Err(ERROR.into());
    }
    let mut hash = Sha256::new();
    hash.update(&first);
    let mut bytes = first.len() as u64;
    let mut buffer = [0; 65536];
    loop {
        let n = input.read(&mut buffer).map_err(|_| ERROR)?;
        if n == 0 {
            break;
        }
        bytes += n as u64;
        if bytes > meta.len() {
            return Err(ERROR.into());
        }
        hash.update(&buffer[..n]);
    }
    let after = crate::storage_stage::open(&checked, false).map_err(|_| ERROR)?;
    if bytes != meta.len() || crate::storage_stage::identity(&after).map_err(|_| ERROR)? != identity
    {
        return Err(ERROR.into());
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

/// An immutable private capture, completed only if the entire native source
/// stayed unchanged. This is saved-history evidence, never proof of live idle.
pub async fn capture(
    config: &AgentConfig,
    cwd: &Path,
    native: &str,
    destination: &Path,
) -> Result<Value, String> {
    if !destination.is_absolute() {
        return Err(ERROR.into());
    }
    let parent = destination
        .parent()
        .ok_or(ERROR)?
        .canonicalize()
        .map_err(|_| ERROR)?;
    let profile = config.profile.canonicalize().map_err(|_| ERROR)?;
    if parent.starts_with(&profile) {
        return Err(ERROR.into());
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(destination).await.map_err(|_| ERROR)?;
    let mut reader = Reader::open(config, cwd).await?;
    let result=tokio::time::timeout(Duration::from_secs(120),async {
        let metadata=reader.metadata(native).await?;
        let path=PathBuf::from(metadata["path"].as_str().ok_or(ERROR)?);
        let p=profile.clone();let n=native.to_owned();let source=path.clone();
        let before=tokio::task::spawn_blocking(move||fingerprint(&p,&source,&n)).await.map_err(|_|ERROR)??;
        let mut digest=Sha256::new();let mut bytes=0u64;let mut turns=0usize;let mut items=0usize;
        let mut turn_cursor=None;let mut seen_turns=BTreeSet::new();let mut cursors=BTreeSet::new();
        loop {
            let page=reader.turns(native,turn_cursor.as_deref()).await?;
            let rows=page["data"].as_array().filter(|v|v.len()<=1).ok_or(ERROR)?;
            for turn in rows {
                let id=identifier(&turn["id"])?;
                if !seen_turns.insert(id.to_owned()) || turns>=100_000 {return Err(ERROR.into());}
                let mut item_cursor=None;let mut item_cursors=BTreeSet::new();let mut item_ids=BTreeSet::new();
                loop {
                    let page=reader.items(native,id,item_cursor.as_deref()).await?;
                    let rows=page["data"].as_array().filter(|v|v.len()<=8).ok_or(ERROR)?;
                    for row in rows {
                        let item=row.get("item").unwrap_or(row);
                        if !item_ids.insert(identifier(&item["id"])?.to_owned()) || items>=200_000 {return Err(ERROR.into());}
                        items+=1;
                    }
                    let row=serde_json::to_vec(&json!({"type":"items","turn":turn,"data":rows})).map_err(|_|ERROR)?;
                    bytes+=row.len() as u64+1;
                    if row.len() as u64>LINE || bytes>TOTAL {return Err(ERROR.into());}
                    digest.update(&row);digest.update(b"\n");file.write_all(&row).await.map_err(|_|ERROR)?;file.write_all(b"\n").await.map_err(|_|ERROR)?;
                    item_cursor=cursor(&page["nextCursor"])?;
                    if let Some(next)=&item_cursor {if rows.is_empty() || !item_cursors.insert(next.clone()) {return Err(ERROR.into());}}else{break;}
                }
                turns+=1;
            }
            turn_cursor=cursor(&page["nextCursor"])?;
            if let Some(next)=&turn_cursor {if rows.is_empty() || !cursors.insert(next.clone()) {return Err(ERROR.into());}}else{break;}
        }
        let after_metadata=reader.metadata(native).await?;
        if after_metadata["path"]!=metadata["path"] {return Err(ERROR.into());}
        let p=profile.clone();let n=native.to_owned();
        let after=tokio::task::spawn_blocking(move||fingerprint(&p,&path,&n)).await.map_err(|_|ERROR)??;
        if before!=after {return Err(ERROR.into());}
        let report=json!({"type":"complete","version":1,"native":native,"turns":turns,"items":items,"bytes":bytes,"sha256":format!("{:x}",digest.finalize()),"sourceBytes":before.0,"sourceSha256":before.1,"historyOnly":true,"ownerStateVerified":false});
        file.write_all(format!("{report}\n").as_bytes()).await.map_err(|_|ERROR)?;file.sync_all().await.map_err(|_|ERROR)?;
        Ok(report)
    }).await.map_err(|_|ERROR.to_owned()).and_then(|r|r);
    reader.close().await;
    result
}
