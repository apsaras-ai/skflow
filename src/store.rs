use anyhow::{bail, Context, Result};
use fs2::FileExt;
use rusqlite::{params, Connection, OpenFlags, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

/// An OS-owned lock covers read-modify-write and advancement, including scripts.
/// SQLite transactions stay short; a dead process releases this lock automatically.
pub struct RunLock(File);

impl RunLock {
    pub fn acquire(db: &Path) -> Result<Self> {
        let mut name = db.as_os_str().to_os_string();
        name.push("-lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(PathBuf::from(name))
            .context("open run lock")?;
        file.lock_exclusive().context("lock run")?;
        Ok(Self(file))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub fn absolute_db(path: &Path, create_parent: bool) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let parent = path.parent().context("database has no parent directory")?;
    if create_parent {
        fs::create_dir_all(parent).context("create database directory")?;
    }
    let parent = parent
        .canonicalize()
        .context("database directory does not exist; run skflow start first")?;
    let path = parent.join(
        path.file_name()
            .context("database path needs a file name")?,
    );
    // Resolve an existing file symlink too so callers share the same advisory lock.
    Ok(if path.exists() {
        path.canonicalize()?
    } else {
        path
    })
}

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn create<T: Serialize>(path: &Path, value: &T) -> Result<Self> {
        if path.exists() {
            bail!(
                "database already exists: {}; choose a new --db or use next/rewind",
                path.display()
            );
        }
        let document = serde_json::to_string(value)?;
        let parent = path.parent().context("database directory missing")?;
        // Build the initial database off to the side. A crash before publishing
        // must not leave the requested path occupied by a half-created run.
        let temporary = tempfile::Builder::new()
            .prefix(".skflow-init-")
            .tempfile_in(parent)?;
        let mut connection =
            Connection::open(temporary.path()).context("create SQLite database")?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch("PRAGMA user_version=1; CREATE TABLE run (id INTEGER PRIMARY KEY CHECK(id=1), document TEXT NOT NULL);")?;
        transaction.execute(
            "INSERT INTO run(id, document) VALUES(1, ?1)",
            params![document],
        )?;
        transaction.commit()?;
        drop(connection);
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(path)
            .map_err(|e| e.error)
            .context("publish initialized database")?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Self::open(path, false)
    }

    pub fn open(path: &Path, read_only: bool) -> Result<Self> {
        if !path.is_file() {
            bail!("no run at {}; run skflow start first", path.display());
        }
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let connection =
            Connection::open_with_flags(path, flags).context("open SQLite database")?;
        connection.busy_timeout(Duration::from_secs(30))?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != 1 {
            bail!("unsupported database format {version}; expected 1");
        }
        if !read_only {
            connection.execute_batch("PRAGMA synchronous=FULL;")?;
        }
        Ok(Self { connection })
    }

    pub fn load<T: DeserializeOwned>(&self) -> Result<T> {
        let document: String = self
            .connection
            .query_row("SELECT document FROM run WHERE id=1", [], |r| r.get(0))
            .context("read persisted run")?;
        serde_json::from_str(&document).context("decode persisted run")
    }

    pub fn save<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let document = serde_json::to_string(value)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed =
            transaction.execute("UPDATE run SET document=?1 WHERE id=1", params![document])?;
        if changed != 1 {
            bail!("run record is missing");
        }
        transaction.commit().context("commit run state")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn initialization_only_publishes_complete_runs_and_never_overwrites_one() {
        struct Invalid;
        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(
                &self,
                _: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("injected serialization failure"))
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run.db");
        assert!(Store::create(&path, &Invalid).is_err());
        assert!(!path.exists());
        let store = Store::create(&path, &json!({"run":"original"})).unwrap();
        assert_eq!(store.load::<Value>().unwrap(), json!({"run":"original"}));
        assert!(Store::create(&path, &json!({"run":"replacement"})).is_err());
        assert_eq!(
            Store::open(&path, true).unwrap().load::<Value>().unwrap(),
            json!({"run":"original"})
        );
    }
}
