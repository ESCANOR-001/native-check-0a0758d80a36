//! Explicit activation of a verified, offline owned-session migration.
//! External/shared workers are never taken over. The legacy rollback checks
//! remain fail-closed once the activated generation gains new data.
use crate::{
    chat::Config, legacy_codex_headers::table, profile_handoff, profile_lock, storage_stage::open,
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Component, Path},
    time::{Duration, Instant},
};
const ERROR:&str="This migration is not ready to activate; original installation and staged history were retained";

fn binding(config: &Config) -> Result<String, String> {
    config.validate()?;
    if (config.codex.shared_server.is_some() && !config.desktop_endpoint())
        || config.claude.shared_server.is_some()
    {
        return Err("Shared owners require their existing connection; this owned-profile migration cannot take them over".into());
    }
    let root = config
        .native_profile_handoff
        .as_ref()
        .ok_or(ERROR)?
        .canonicalize()
        .map_err(|_| ERROR)?;
    let mut value = json!({"root":root,"codex":config.codex.profile.canonicalize().map_err(|_|ERROR)?,"claude":config.claude.profile.canonicalize().map_err(|_|ERROR)?,"workspace":config.workspace.canonicalize().map_err(|_|ERROR)?});
    if let Some(crate::agent::SharedServer::Desktop {
        desktop_endpoint,
        reader_executable,
        reader_workspace,
    }) = &config.codex.shared_server
    {
        value["desktopOwner"] = json!({"endpoint":desktop_endpoint.path,"reader":reader_executable.canonicalize().map_err(|_|ERROR)?,"workspace":reader_workspace.canonicalize().map_err(|_|ERROR)?});
    }
    Ok(crate::auth::digest(&value.to_string()))
}
fn transcript(profile: &Path, relative: &str, report: &str) -> Result<(), String> {
    let report: Value = serde_json::from_str(report).map_err(|_| ERROR)?;
    let bytes = report["bytes"]
        .as_u64()
        .filter(|n| *n <= 512 * 1024 * 1024)
        .ok_or(ERROR)?;
    let expected = report["sha256"]
        .as_str()
        .filter(|s| s.len() == 64)
        .ok_or(ERROR)?;
    let relative = Path::new(relative);
    if relative.as_os_str().len() > 4096
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(ERROR.into());
    }
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() {
        return Err(ERROR.into());
    }
    let mut path = profile.to_owned();
    open(&path, true).map_err(|_| ERROR)?;
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        open(&path, i + 1 < parts.len()).map_err(|_| ERROR)?;
    }
    let mut file = open(&path, false).map_err(|_| ERROR)?;
    if file.metadata().map_err(|_| ERROR)?.len() != bytes {
        return Err(ERROR.into());
    }
    let identity = crate::storage_stage::identity(&file).map_err(|_| ERROR)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read = 0u64;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let n = file.read(&mut buffer).map_err(|_| ERROR)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > bytes || Instant::now() >= deadline {
            return Err(ERROR.into());
        }
        hash.update(&buffer[..n]);
    }
    if read != bytes
        || format!("{:x}", hash.finalize()) != expected
        || crate::storage_stage::identity(&open(&path, false).map_err(|_| ERROR)?)
            .map_err(|_| ERROR)?
            != identity
    {
        return Err(ERROR.into());
    }
    Ok(())
}

impl Store {
    pub fn assert_migration_configuration(&self, config: Option<&Config>) -> Result<(), String> {
        let offline = match self.assert_migration_ready() {
            Ok(()) => return Ok(()),
            Err(reason) => reason,
        };
        if !table(&self.db, "legacy_activation")? {
            return Err(offline);
        }
        let saved:Option<(String,String,String)>=self.db.query_row("SELECT environment,configuration,source_hash FROM legacy_activation WHERE id=1 AND phase IN ('active','started','running')",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_|ERROR)?;
        let (environment, configuration, source_hash) = saved.ok_or(ERROR)?;
        if configuration != binding(config.ok_or(ERROR)?)? {
            return Err("Restore the migration's native profile configuration; no original profile was opened".into());
        }
        let same:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_connection_import i JOIN metadata m ON m.key='environment_id' WHERE i.id=1 AND i.phase='prepared' AND i.environment=?1 AND i.digest=?2 AND m.value=?1)",params![environment,source_hash],|r|r.get(0)).map_err(|_|ERROR)?;
        if !same {
            return Err(ERROR.into());
        }
        Ok(())
    }
}

/// Hold both profile generations unchanged for the complete rollback transaction.
/// Once runtime startup is claimed, executable rollback must retain current data.
pub(crate) fn rollback_lease(
    db: &rusqlite::Connection,
) -> Result<Option<profile_handoff::Session>, String> {
    if !table(db, "legacy_activation")? {
        return Ok(None);
    }
    let (environment,raw,configuration,phase):(String,String,String,String)=db.query_row("SELECT environment,config,configuration,phase FROM legacy_activation WHERE id=1 AND length(config)<=262144",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|_|ERROR)?;
    if phase != "active" {
        return Err("This migration has started handling chats. Data rollback was refused; roll back the executable while keeping its current state and native profiles. All history is retained.".into());
    }
    let config: Config = serde_json::from_str(&raw).map_err(|_| ERROR)?;
    if binding(&config)? != configuration {
        return Err(ERROR.into());
    }
    profile_handoff::inspect_prepared(
        config.native_profile_handoff.as_ref().ok_or(ERROR)?,
        &environment,
        &config.codex.profile,
        &config.claude.profile,
    )
    .map(Some)
}

/// Caller holds the state-directory lease. Verify before Store::open can repair
/// indexes or clear invitations. The durable started marker favors preserving
/// data after a crash, even if startup did not reach an agent launch.
pub fn before_start(directory: &Path, config: Option<&Config>) -> Result<(), String> {
    if !directory
        .join("native-connect.sqlite")
        .try_exists()
        .map_err(|_| ERROR)?
    {
        return Ok(());
    }
    let mut store = crate::legacy_stage::existing_store(directory)?;
    store.claim_migration_start(config)
}
impl Store {
    /// Called only after Chats owns the verified native generation lease. The
    /// importer deliberately exposes unknown status while offline; its settled
    /// owned sessions become ready once that ownership is actually acquired.
    pub(crate) fn complete_migration_activation(&mut self, config: &Config) -> Result<(), String> {
        self.assert_migration_configuration(Some(config))?;
        if !table(&self.db, "legacy_activation")? {
            return Ok(());
        }
        let phase: String = self
            .db
            .query_row("SELECT phase FROM legacy_activation WHERE id=1", [], |r| {
                r.get(0)
            })
            .map_err(|_| ERROR)?;
        if phase == "running" {
            return Ok(());
        }
        if phase != "started" {
            return Err(ERROR.into());
        }
        let tx = self.db.transaction().map_err(|_| ERROR)?;
        let rows: Vec<(String, String)> = tx
            .prepare("SELECT id,data FROM chats ORDER BY id LIMIT 4097")
            .map_err(|_| ERROR)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|_| ERROR)?
            .collect::<Result<_, _>>()
            .map_err(|_| ERROR)?;
        if rows.len() > 4096 {
            return Err(ERROR.into());
        }
        for (id, raw) in rows {
            let mut value: Value = serde_json::from_str(&raw).map_err(|_| ERROR)?;
            let proof = match value["agent"].as_str() {
                Some("codex") => "legacy_codex_native",
                Some("claude") => "legacy_claude_native",
                _ => return Err(ERROR.into()),
            };
            if !value["thread"]["session"]["activeTurnId"].is_null()
                || value["queue"].as_array().is_none_or(|q| !q.is_empty())
                || (!value["thread"]["latestTurn"].is_null()
                    && !matches!(
                        value["thread"]["latestTurn"]["state"].as_str(),
                        Some("completed" | "interrupted" | "error")
                    ))
            {
                return Err(ERROR.into());
            }
            let known: bool = tx
                .query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM {proof} WHERE chat=?1 AND phase='prepared')"
                    ),
                    [&id],
                    |r| r.get(0),
                )
                .map_err(|_| ERROR)?;
            if !known {
                return Err(ERROR.into());
            }
            let external = value["external_claude"] == true || value["external_codex"] == true;
            value["thread"]["session"]["status"] =
                json!(if external { "disconnected" } else { "ready" });
            value["thread"]["session"]["statusConfidence"] =
                json!(if external { "unknown" } else { "confirmed" });
            tx.execute(
                "UPDATE chats SET data=?1 WHERE id=?2",
                params![value.to_string(), id],
            )
            .map_err(|_| ERROR)?;
        }
        tx.execute(
            "UPDATE legacy_activation SET phase='running' WHERE id=1",
            [],
        )
        .map_err(|_| ERROR)?;
        tx.commit().map_err(|_| ERROR)?;
        Ok(())
    }
    pub(crate) fn claim_migration_start(&mut self, config: Option<&Config>) -> Result<(), String> {
        self.assert_migration_configuration(config)?;
        if !table(&self.db, "legacy_activation")? {
            return Ok(());
        }
        let phase: String = self
            .db
            .query_row("SELECT phase FROM legacy_activation WHERE id=1", [], |r| {
                r.get(0)
            })
            .map_err(|_| ERROR)?;
        if phase == "active" {
            self.rollback_legacy_imports(false)?;
            self.db
                .execute(
                    "UPDATE legacy_activation SET phase='started' WHERE id=1 AND phase='active'",
                    [],
                )
                .map_err(|_| ERROR)?;
        }
        Ok(())
    }
}

pub fn run(directory: &Path, config_path: &Path) -> Result<Value, String> {
    if !directory.is_absolute() {
        return Err(ERROR.into());
    }
    open(directory, true).map_err(|_| ERROR)?;
    let config = crate::launch_config::load(config_path)?;
    let raw = String::from_utf8(
        crate::profile_snapshot::bytes(config_path, 256 * 1024).map_err(|_| ERROR)?,
    )
    .map_err(|_| ERROR)?;
    let reread: Config = serde_json::from_str(&raw).map_err(|_| ERROR)?;
    let configuration = binding(&config)?;
    if binding(&reread)? != configuration {
        return Err(ERROR.into());
    }
    let _lease = profile_lock::acquire(directory)?;
    let mut store = crate::legacy_stage::existing_store(directory)?;
    if table(&store.db, "legacy_activation")? {
        store.assert_migration_configuration(Some(&config))?;
        let phase: String = store
            .db
            .query_row("SELECT phase FROM legacy_activation WHERE id=1", [], |r| {
                r.get(0)
            })
            .map_err(|_| ERROR)?;
        if phase == "active" {
            store.rollback_legacy_imports(false)?;
        }
        return Ok(
            json!({"phase":"owned-migration-activated","alreadyActive":true,"sourceUnchanged":true,"runtimeStarted":false,"migrationComplete":false}),
        );
    }
    store.rollback_legacy_imports(false)?;
    let (environment,source_hash):(String,String)=store.db.query_row("SELECT environment,digest FROM legacy_connection_import WHERE id=1 AND phase='prepared'",[],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|ERROR)?;
    let session = profile_handoff::inspect_prepared(
        config.native_profile_handoff.as_ref().ok_or(ERROR)?,
        &environment,
        &config.codex.profile,
        &config.claude.profile,
    )?;
    let (codex, claude) = session.profiles();
    let chats: Vec<(String, String, String)> = store
        .db
        .prepare("SELECT id,agent,data FROM chats ORDER BY id LIMIT 4097")
        .map_err(|_| ERROR)?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map_err(|_| ERROR)?
        .collect::<Result<_, _>>()
        .map_err(|_| ERROR)?;
    if chats.len() > 4096 {
        return Err(ERROR.into());
    }
    for (id, agent, raw) in &chats {
        let chat: Value = serde_json::from_str(raw).map_err(|_| ERROR)?;
        if ((chat["external_claude"] == true || chat["external_codex"] == true)
            && !session.retains_original_profiles())
            || (chat["external_codex"] == true && !config.desktop_endpoint())
            || chat["thread"]["session"]["activeTurnId"].is_string()
        {
            return Err(ERROR.into());
        }
        let (proof, profile) = if agent == "codex" {
            crate::legacy_codex_native::verify(&store.db, directory, id)?;
            ("legacy_codex_native", &codex)
        } else if agent == "claude" {
            crate::legacy_claude_native::verify(&store.db, directory, id)?;
            crate::legacy_claude_resume::verify(&store.db, id)?;
            let resumed = table(&store.db, "legacy_claude_resume")?
                && store.db.query_row("SELECT EXISTS(SELECT 1 FROM legacy_claude_resume WHERE chat=?1 AND phase='prepared')",[id],|r|r.get::<_,bool>(0)).map_err(|_|ERROR)?;
            // Older CLI-only chats have a native session but no shared link.
            // Their complete native export is sufficient for owned resumption;
            // do not invent a relay identity or cursor just to activate them.
            let linked = !chat["remote_identity"].is_null();
            if chat["external_claude"] == true && !linked {
                return Err(ERROR.into());
            }
            if (linked && (!resumed || !config.claude.remote_control)) || (!linked && resumed) {
                return Err(ERROR.into());
            }
            ("legacy_claude_native", &claude)
        } else {
            return Err(ERROR.into());
        };
        let (path,report):(String,String)=store.db.query_row(&format!("SELECT transcript,report FROM {proof} WHERE chat=?1 AND phase='prepared' AND length(report)<=16777216"),[id],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|ERROR)?;
        transcript(profile, &path, &report)?;
    }
    // The receipt commits only after every chat and both copied profiles were
    // verified. No CLI, relay, listener or original profile is touched here.
    let tx = store.db.transaction().map_err(|_| ERROR)?;
    tx.execute_batch("CREATE TABLE legacy_activation(id INTEGER PRIMARY KEY CHECK(id=1),environment TEXT NOT NULL,configuration TEXT NOT NULL,source_hash TEXT NOT NULL,phase TEXT NOT NULL,config TEXT NOT NULL);").map_err(|_|ERROR)?;
    tx.execute(
        "INSERT INTO legacy_activation VALUES(1,?1,?2,?3,'active',?4)",
        params![environment, configuration, source_hash, raw],
    )
    .map_err(|_| ERROR)?;
    tx.commit().map_err(|_| ERROR)?;
    Ok(
        json!({"phase":"owned-migration-activated","alreadyActive":false,"chats":chats.len(),"sourceUnchanged":true,"backupRetained":true,"runtimeStarted":false,"migrationComplete":false}),
    )
}
