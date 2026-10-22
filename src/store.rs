use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path, time::Duration};

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub id: String,
    pub label: String,
    pub os: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub credential_digest: String,
    pub installation_digest: Option<String>,
}

pub struct Store {
    pub(crate) storage: crate::storage::Limits,
    pub(crate) db: Connection,
    pub directory: std::path::PathBuf,
}

impl Store {
    /// This experimental store owns a separate profile. It never discovers or
    /// rewrites the normal bridge, Desktop or CLI profiles.
    pub fn open(directory: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        if !directory.is_absolute() {
            return Err("an absolute isolated state directory is required".into());
        }
        fs::create_dir_all(directory)?;
        if fs::symlink_metadata(directory)?.file_type().is_symlink() {
            return Err("state directory must not be a symlink".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        }
        let path = directory.join("native-connect.sqlite");
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err("state database must be a regular file".into()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let db = Connection::open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             PRAGMA cache_size=-2048; PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS clients (
                id TEXT PRIMARY KEY, label TEXT NOT NULL, os TEXT NOT NULL,
                created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
                credential_digest TEXT NOT NULL UNIQUE, installation_digest TEXT
             );
             CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        let mut store = Self {
            db,
            directory: directory.to_path_buf(),
            storage: Default::default(),
        };
        store.init_storage()?;
        Ok(store)
    }

    pub fn environment_id(&self) -> rusqlite::Result<String> {
        self.db.execute(
            "INSERT OR IGNORE INTO metadata VALUES ('environment_id', ?1)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        self.db.query_row(
            "SELECT value FROM metadata WHERE key='environment_id'",
            [],
            |row| row.get(0),
        )
    }

    pub fn replace_client(&mut self, client: &Client) -> rusqlite::Result<Vec<String>> {
        let tx = self.db.transaction()?;
        let replaced = {
            let mut stmt = tx.prepare("SELECT id FROM clients WHERE installation_digest=?1")?;
            let rows = stmt.query_map([&client.installation_digest], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()?
        };
        tx.execute(
            "DELETE FROM clients WHERE installation_digest=?1",
            [&client.installation_digest],
        )?;
        tx.execute(
            "INSERT INTO clients VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                client.id,
                client.label,
                client.os,
                client.created_at,
                client.expires_at,
                client.credential_digest,
                client.installation_digest
            ],
        )?;
        tx.commit()?;
        Ok(replaced)
    }

    pub fn client(&self, digest: &str, now: i64) -> rusqlite::Result<Option<Client>> {
        self.db
            .query_row(
                "SELECT id,label,os,created_at,expires_at,credential_digest,installation_digest
             FROM clients WHERE credential_digest=?1 AND expires_at>?2",
                params![digest, now],
                |r| {
                    Ok(Client {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        os: r.get(2)?,
                        created_at: r.get(3)?,
                        expires_at: r.get(4)?,
                        credential_digest: r.get(5)?,
                        installation_digest: r.get(6)?,
                    })
                },
            )
            .optional()
    }

    pub fn authorized(&self, id: &str, now: i64) -> rusqlite::Result<bool> {
        self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM clients WHERE id=?1 AND expires_at>?2)",
            params![id, now],
            |r| r.get(0),
        )
    }

    pub fn revoke(&self, id: &str) -> rusqlite::Result<bool> {
        Ok(self.db.execute("DELETE FROM clients WHERE id=?1", [id])? > 0)
    }
}
