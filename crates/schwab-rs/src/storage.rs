//! Private, transactional local state. SQLite files must be on a local filesystem.
use crate::error::{Error, Result};
use rusqlite::{Connection, OptionalExtension};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Every table the schema creates; a restore refuses a backup missing any of them.
const COUNT_STATE_TABLES: &str = "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' \
     AND name IN ('meta','submissions','audit','consumed_approvals','used_order_tags',\
     'accounts','orders','executions','leases','quota','quota_config','approvals','reservations')";

#[derive(Clone, Debug)]
pub struct StateDatabase {
    path: PathBuf,
    legacy: Option<PathBuf>,
}
pub(crate) fn sql(error: rusqlite::Error) -> Error {
    Error::Storage(error.to_string())
}
impl StateDatabase {
    pub fn new(path: PathBuf) -> Self {
        if path.extension().is_some_and(|e| e == "sqlite3") {
            Self { path, legacy: None }
        } else {
            Self {
                path: path.with_extension("store").join("state.sqlite3"),
                legacy: Some(path),
            }
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        static WORKERS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
            std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(32)));
        let permit = WORKERS
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Storage("State worker pool closed".into()))?;
        let database = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit=permit;
            let parent = database.path.parent().ok_or_else(|| Error::Config("State path requires a directory".into()))?;
            crate::security::ensure_private_directory(parent)?;
            let lock = crate::auth::token_store::open_lock_file(&database.path.with_extension("init.lock"))?;
            let lock_started=std::time::Instant::now();
            loop {
                match lock.try_lock() {
                    Ok(())=>break,
                    Err(std::fs::TryLockError::WouldBlock) if lock_started.elapsed()<Duration::from_secs(5)=>std::thread::sleep(Duration::from_millis(10)),
                    Err(std::fs::TryLockError::WouldBlock)=>return Err(Error::Storage("State initialization lock timed out".into())),
                    Err(std::fs::TryLockError::Error(error))=>return Err(Error::Io(error)),
                }
            }
            if !database.path.exists() { crate::security::secure_file_write(&database.path, &[])?; }
            crate::security::verify_file_permissions(&database.path)?;
            let mut conn = Connection::open(&database.path).map_err(sql)?;
            conn.busy_timeout(Duration::from_secs(5)).map_err(sql)?;
            let tables: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r|r.get(0)).map_err(sql)?;
            let has_meta: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='meta')", [], |r|r.get(0)).map_err(sql)?;
            if tables > 0 && !has_meta { return Err(Error::Storage("Existing state is missing metadata; restore a complete backup".into())); }
            conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);").map_err(sql)?;
            let version: Option<String> = conn.query_row("SELECT value FROM meta WHERE key='schema'", [], |r| r.get(0)).optional().map_err(sql)?;
            if version.as_deref().is_some_and(|v| v != "1") { return Err(Error::Storage("Unsupported state schema version".into())); }
            if version.is_some() {
                let required: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name IN ('meta','submissions','audit','consumed_approvals','used_order_tags','accounts','orders','executions','leases','quota','quota_config','approvals','reservations')", [], |r|r.get(0)).map_err(sql)?;
                if required != 13 { return Err(Error::Storage("Existing state is missing required tables; restore a complete backup".into())); }
            } else {
                let metadata: i64 = conn.query_row("SELECT COUNT(*) FROM meta", [], |r|r.get(0)).map_err(sql)?;
                if tables > i64::from(has_meta) || metadata > 0 { return Err(Error::Storage("Existing state is missing its schema version".into())); }
            }
            conn.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS submissions(id TEXT PRIMARY KEY, account TEXT NOT NULL, operation TEXT NOT NULL, created_at INTEGER NOT NULL, request TEXT NOT NULL, outcome TEXT, dedup_key TEXT UNIQUE);
                CREATE INDEX IF NOT EXISTS submissions_account ON submissions(account,created_at);
                CREATE UNIQUE INDEX IF NOT EXISTS one_pending_account ON submissions(account) WHERE outcome IS NULL;
                CREATE TABLE IF NOT EXISTS audit(sequence INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, kind TEXT NOT NULL, subject TEXT NOT NULL, payload TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS consumed_approvals(key TEXT PRIMARY KEY);
                CREATE TABLE IF NOT EXISTS used_order_tags(account TEXT NOT NULL,tag TEXT NOT NULL,PRIMARY KEY(account,tag));
                CREATE TABLE IF NOT EXISTS accounts(account TEXT PRIMARY KEY, generation INTEGER NOT NULL, observed_at INTEGER NOT NULL, valid INTEGER NOT NULL, snapshot TEXT NOT NULL);
                CREATE TABLE IF NOT EXISTS orders(account TEXT NOT NULL, order_id TEXT NOT NULL, observed_at INTEGER NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(account,order_id));
                CREATE TABLE IF NOT EXISTS executions(account TEXT NOT NULL, execution_id TEXT NOT NULL, payload TEXT NOT NULL, PRIMARY KEY(account,execution_id));
                CREATE TABLE IF NOT EXISTS leases(resource TEXT PRIMARY KEY, owner TEXT NOT NULL, epoch INTEGER NOT NULL, expires_at INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS quota(key TEXT NOT NULL, at INTEGER NOT NULL);
                CREATE INDEX IF NOT EXISTS quota_window ON quota(key,at);
                CREATE TABLE IF NOT EXISTS quota_config(key TEXT PRIMARY KEY, per_second INTEGER NOT NULL, burst INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS approvals(digest TEXT PRIMARY KEY, principal TEXT NOT NULL, expires_at INTEGER NOT NULL, operator TEXT NOT NULL, consumed INTEGER NOT NULL DEFAULT 0);
                CREATE TABLE IF NOT EXISTS reservations(id TEXT PRIMARY KEY, account TEXT NOT NULL, quantity TEXT NOT NULL, notional TEXT NOT NULL, state TEXT NOT NULL);
                INSERT OR IGNORE INTO meta VALUES('schema','1'); COMMIT;").map_err(sql)?;
            let tags_imported: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='tags_imported')", [], |r| r.get(0)).map_err(sql)?;
            if !tags_imported {
                conn.execute_batch("BEGIN IMMEDIATE; INSERT OR IGNORE INTO used_order_tags SELECT account,json_extract(request,'$.tag') FROM submissions WHERE json_type(request,'$.tag')='text'; INSERT INTO meta VALUES('tags_imported','1'); COMMIT;").map_err(sql)?;
            }
            if let Some(legacy) = database.legacy.filter(|p| p.exists()) {
                let imported: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM meta WHERE key='legacy_imported')", [], |r| r.get(0)).map_err(sql)?;
                if !imported {
                    crate::security::verify_file_permissions(&legacy)?;
                    let records: std::collections::HashMap<String, crate::execution::SubmissionRecord> = serde_json::from_slice(&std::fs::read(legacy)?)?;
                    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(sql)?;
                    for record in records.values() {
                        if let Some(tag)=record.request.get("tag").and_then(serde_json::Value::as_str){tx.execute("INSERT INTO used_order_tags VALUES(?1,?2)",rusqlite::params![record.account,tag]).map_err(sql)?;}
                        tx.execute("INSERT INTO submissions VALUES(?1,?2,?3,?4,?5,?6,?7)", rusqlite::params![record.attempt_id,record.account,record.operation,record.created_at,serde_json::to_string(&record.request)?, record.outcome.as_ref().map(serde_json::to_string).transpose()?,record.dedup_key]).map_err(sql)?;
                        if let Some(key) = &record.dedup_key { tx.execute("INSERT OR IGNORE INTO consumed_approvals VALUES(?1)",[key]).map_err(sql)?; }
                    }
                    tx.execute("INSERT INTO meta VALUES('legacy_imported','1')",[]).map_err(sql)?;
                    tx.commit().map_err(sql)?;
                }
            }
            drop(lock);
            f(&mut conn)
        }).await.map_err(|_| Error::Storage("State worker failed".into()))?
    }
    /// Online SQLite backup. Destination must be a new file in a private directory.
    pub async fn backup(&self, destination: PathBuf) -> Result<()> {
        self.run(move |conn| {
            if destination.exists() {
                return Err(Error::InvalidParameter(
                    "Backup destination already exists".into(),
                ));
            }
            let parent = destination
                .parent()
                .ok_or_else(|| Error::InvalidParameter("Backup directory required".into()))?;
            crate::security::ensure_private_directory(parent)?;
            let reserved = reserve_backup_file(&destination)?;
            conn.backup("main", &destination, None).map_err(sql)?;
            crate::security::verify_file_permissions(&destination)?;
            reserved.sync_all()?;
            Ok(())
        })
        .await
    }
    /// Restore to a NEW state location; never overwrite a running database.
    pub async fn restore(backup: PathBuf, destination: PathBuf) -> Result<Self> {
        let target = Self::new(destination);
        let target_path = target.path.clone();
        // A restore must never create or initialize its source. Validate the
        // read-only backup before creating any destination state.
        tokio::task::spawn_blocking(move || {
            crate::security::verify_file_permissions(&backup)?;
            let source =
                Connection::open_with_flags(&backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(sql)?;
            let check: String = source
                .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                .map_err(sql)?;
            if check != "ok" {
                return Err(Error::Storage("Backup integrity check failed".into()));
            }
            let schema: String = source
                .query_row("SELECT value FROM meta WHERE key='schema'", [], |r| {
                    r.get(0)
                })
                .map_err(sql)?;
            if schema != "1" {
                return Err(Error::Storage("Unsupported backup schema version".into()));
            }
            let tables: i64 = source
                .query_row(COUNT_STATE_TABLES, [], |r| r.get(0))
                .map_err(sql)?;
            if tables != 13 {
                return Err(Error::Storage(
                    "Backup is missing required state tables".into(),
                ));
            }
            if target_path.exists() {
                return Err(Error::InvalidParameter(
                    "Restore destination already exists".into(),
                ));
            }
            let parent = target_path
                .parent()
                .ok_or_else(|| Error::InvalidParameter("Restore directory required".into()))?;
            crate::security::ensure_private_directory(parent)?;
            let reserved = reserve_backup_file(&target_path)?;
            source.backup("main", &target_path, None).map_err(sql)?;
            reserved.sync_all()?;
            Ok::<_, Error>(())
        })
        .await
        .map_err(|_| Error::Storage("Restore worker failed".into()))??;
        target
            .run(|c| {
                let check: String = c
                    .query_row("PRAGMA integrity_check", [], |r| r.get(0))
                    .map_err(sql)?;
                if check != "ok" {
                    return Err(Error::Storage("Backup integrity check failed".into()));
                }
                Ok(())
            })
            .await?;
        Ok(target)
    }
}
pub(crate) fn audit(
    conn: &Connection,
    kind: &str,
    subject: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    conn.execute(
        "INSERT INTO audit(at,kind,subject,payload) VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            chrono::Utc::now().timestamp_millis(),
            kind,
            subject,
            serde_json::to_string(payload)?
        ],
    )
    .map_err(sql)?;
    Ok(())
}

// Atomically reserve a new backup path with private permissions at creation:
// an explicit protected DACL on Windows and mode 0600 on Unix.
fn reserve_backup_file(path: &Path) -> Result<std::fs::File> {
    #[cfg(windows)]
    {
        crate::security_windows::secure_create_new(path)
    }
    #[cfg(not(windows))]
    {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        crate::security::verify_file_permissions(path)?;
        Ok(file)
    }
}
