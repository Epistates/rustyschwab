//! Private, transactional local state. SQLite files must be on a local filesystem.
//!
//! A [`StateDatabase`] validates and migrates its file once, on first use, and
//! then serves operations from a small pool of open connections. Clones share
//! the pool; a second handle on the same path is a separate opener, so
//! cross-process safety comes from SQLite's WAL locking, not from this type.
use crate::error::{Error, Result};
use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// Connections kept open per database. Writers serialize on SQLite's lock, so
/// a small pool bounds blocking threads without starving readers or backups.
const POOL_SIZE: usize = 4;
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const INIT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Tables created by the baseline migration. Every schema version has them,
/// and a restore refuses a backup missing any of them.
const BASELINE_TABLES: [&str; 13] = [
    "meta",
    "submissions",
    "audit",
    "consumed_approvals",
    "used_order_tags",
    "accounts",
    "orders",
    "executions",
    "leases",
    "quota",
    "quota_config",
    "approvals",
    "reservations",
];

struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

/// Applied in order inside one transaction each; `meta.schema` records the
/// highest applied version and `schema_migrations` keeps the history.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "baseline",
        sql: "CREATE TABLE IF NOT EXISTS submissions(id TEXT PRIMARY KEY, account TEXT NOT NULL, operation TEXT NOT NULL, created_at INTEGER NOT NULL, request TEXT NOT NULL, outcome TEXT, dedup_key TEXT UNIQUE);
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
            CREATE TABLE IF NOT EXISTS reservations(id TEXT PRIMARY KEY, account TEXT NOT NULL, quantity TEXT NOT NULL, notional TEXT NOT NULL, state TEXT NOT NULL);",
    },
    // Version 1 files tracked this import with an ad hoc `tags_imported` flag.
    Migration {
        version: 2,
        name: "import_order_tags",
        sql: "INSERT OR IGNORE INTO used_order_tags SELECT account,json_extract(request,'$.tag') FROM submissions WHERE json_type(request,'$.tag')='text';
            DELETE FROM meta WHERE key='tags_imported';",
    },
    // `armed` marks an intent whose request may have left this host; rows
    // written before it existed are assumed armed. Reservations learn what
    // they would buy or sell, so sells are judged per symbol and a
    // replacement does not count the order it replaces. The indexes keep
    // the risk check and reconciliation proportional to working orders
    // rather than to history, and every tag Schwab is known to hold is
    // spent.
    Migration {
        version: 3,
        name: "armed_intents_reservation_scope",
        sql: "ALTER TABLE submissions ADD COLUMN armed INTEGER NOT NULL DEFAULT 1;
            ALTER TABLE reservations ADD COLUMN symbol TEXT;
            ALTER TABLE reservations ADD COLUMN side TEXT;
            ALTER TABLE reservations ADD COLUMN replaces TEXT;
            UPDATE reservations SET
              symbol=(SELECT json_extract(s.request,'$.orderLegCollection[0].instrument.symbol') FROM submissions s WHERE s.id=reservations.id),
              side=(SELECT json_extract(s.request,'$.orderLegCollection[0].instruction') FROM submissions s WHERE s.id=reservations.id);
            CREATE INDEX IF NOT EXISTS reservations_held ON reservations(account) WHERE state='held';
            CREATE INDEX IF NOT EXISTS submissions_order ON submissions(account,json_extract(outcome,'$.order_id'));
            CREATE INDEX IF NOT EXISTS orders_status ON orders(account,json_extract(payload,'$.status'));
            CREATE INDEX IF NOT EXISTS orders_tag ON orders(account,json_extract(payload,'$.tag'));
            INSERT OR IGNORE INTO used_order_tags SELECT account,json_extract(payload,'$.tag') FROM orders WHERE json_type(payload,'$.tag')='text';",
    },
];
const SCHEMA_VERSION: u32 = MIGRATIONS[MIGRATIONS.len() - 1].version;

#[derive(Clone)]
pub struct StateDatabase {
    inner: Arc<Inner>,
}
struct Inner {
    path: PathBuf,
    legacy: Option<PathBuf>,
    initialized: tokio::sync::OnceCell<()>,
    slots: Arc<tokio::sync::Semaphore>,
    idle: parking_lot::Mutex<Vec<Connection>>,
}
impl std::fmt::Debug for StateDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDatabase")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}
pub(crate) fn sql(error: rusqlite::Error) -> Error {
    Error::Storage(error.to_string())
}
impl StateDatabase {
    pub fn new(path: PathBuf) -> Self {
        let (path, legacy) = if path.extension().is_some_and(|e| e == "sqlite3") {
            (path, None)
        } else {
            (
                path.with_extension("store").join("state.sqlite3"),
                Some(path),
            )
        };
        Self {
            inner: Arc::new(Inner {
                path,
                legacy,
                initialized: tokio::sync::OnceCell::new(),
                slots: Arc::new(tokio::sync::Semaphore::new(POOL_SIZE)),
                idle: parking_lot::Mutex::new(Vec::with_capacity(POOL_SIZE)),
            }),
        }
    }
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    /// Run `f` on a pooled connection from a blocking thread. The first call
    /// validates and migrates the file; a failed initialization is retried by
    /// the next call rather than poisoning the handle.
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let inner = self.inner.clone();
        inner
            .initialized
            .get_or_try_init(|| async {
                let inner = self.inner.clone();
                let connection = tokio::task::spawn_blocking(move || inner.initialize())
                    .await
                    .map_err(|_| Error::Storage("State worker failed".into()))??;
                self.inner.idle.lock().push(connection);
                Ok::<_, Error>(())
            })
            .await?;
        let permit = inner
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Storage("State worker pool closed".into()))?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut connection = inner.checkout()?;
            let result = f(&mut connection);
            inner.checkin(connection);
            result
        })
        .await
        .map_err(|_| Error::Storage("State worker failed".into()))?
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
        let target_path = target.inner.path.clone();
        // A restore must never create or initialize its source. Validate the
        // read-only backup before creating any destination state.
        tokio::task::spawn_blocking(move || {
            crate::security::verify_file_permissions(&backup)?;
            let source =
                Connection::open_with_flags(&backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(sql)?;
            integrity_check(&source)?;
            if !schema_version(&source)?
                .is_some_and(|version| (1..=SCHEMA_VERSION).contains(&version))
            {
                return Err(Error::Storage("Unsupported backup schema version".into()));
            }
            if !baseline_tables_present(&source)? {
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
        // A backup predates whatever reached the broker since it was taken:
        // no account baseline in it may be trusted until reconciliation
        // reads the broker again (and learns the order tags it now holds).
        target
            .run(|c| {
                integrity_check(c)?;
                let tx = c
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(sql)?;
                let accounts = tx.execute("UPDATE accounts SET valid=0", []).map_err(sql)?;
                audit(
                    &tx,
                    "state_restored",
                    "state",
                    &serde_json::json!({"invalidated_accounts": accounts}),
                )?;
                tx.commit().map_err(sql)
            })
            .await?;
        Ok(target)
    }
}
impl Inner {
    fn checkout(&self) -> Result<Connection> {
        match self.idle.lock().pop() {
            Some(connection) => Ok(connection),
            None => open_connection(&self.path),
        }
    }
    /// A connection left mid-transaction is closed (rolling it back) rather
    /// than handed to the next caller.
    fn checkin(&self, connection: Connection) {
        if connection.is_autocommit() {
            let mut idle = self.idle.lock();
            if idle.len() < POOL_SIZE {
                idle.push(connection);
            }
        }
    }
    /// Cross-process initialization is serialized by a lock file so two
    /// openers cannot race the schema migration.
    fn initialize(&self) -> Result<Connection> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::Config("State path requires a directory".into()))?;
        crate::security::ensure_private_directory(parent)?;
        let lock =
            crate::auth::token_store::open_lock_file(&self.path.with_extension("init.lock"))?;
        let started = std::time::Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < INIT_LOCK_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(Error::Storage("State initialization lock timed out".into()));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(Error::Io(error)),
            }
        }
        if !self.path.exists() {
            crate::security::secure_file_write(&self.path, &[])?;
        }
        crate::security::verify_file_permissions(&self.path)?;
        let mut connection = open_connection(&self.path)?;
        connection
            .execute_batch("PRAGMA journal_mode=WAL")
            .map_err(sql)?;
        validate_existing(&connection)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            )
            .map_err(sql)?;
        migrate(&mut connection)?;
        if let Some(legacy) = self.legacy.as_deref().filter(|p| p.exists()) {
            import_legacy(&mut connection, legacy)?;
        }
        drop(lock);
        Ok(connection)
    }
}
fn open_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path).map_err(sql)?;
    connection.busy_timeout(BUSY_TIMEOUT).map_err(sql)?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(sql)?;
    Ok(connection)
}
fn integrity_check(connection: &Connection) -> Result<()> {
    let check: String = connection
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(sql)?;
    if check != "ok" {
        return Err(Error::Storage("Backup integrity check failed".into()));
    }
    Ok(())
}
fn baseline_tables_present(connection: &Connection) -> Result<bool> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type='table'")
        .map_err(sql)?;
    let names = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(sql)?
        .collect::<std::result::Result<std::collections::HashSet<_>, _>>()
        .map_err(sql)?;
    Ok(BASELINE_TABLES.iter().all(|table| names.contains(*table)))
}
/// Refuse files that are not ours or that lost their metadata; a fresh file
/// and a complete versioned file both pass.
fn validate_existing(connection: &Connection) -> Result<()> {
    let tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .map_err(sql)?;
    let has_meta: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='meta')",
            [],
            |r| r.get(0),
        )
        .map_err(sql)?;
    if tables > 0 && !has_meta {
        return Err(Error::Storage(
            "Existing state is missing metadata; restore a complete backup".into(),
        ));
    }
    if !has_meta {
        return Ok(());
    }
    match schema_version(connection)? {
        Some(version) if version > SCHEMA_VERSION => {
            Err(Error::Storage("Unsupported state schema version".into()))
        }
        Some(_) if !baseline_tables_present(connection)? => Err(Error::Storage(
            "Existing state is missing required tables; restore a complete backup".into(),
        )),
        Some(_) => Ok(()),
        None => {
            let metadata: i64 = connection
                .query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))
                .map_err(sql)?;
            if tables > 1 || metadata > 0 {
                return Err(Error::Storage(
                    "Existing state is missing its schema version".into(),
                ));
            }
            Ok(())
        }
    }
}
fn schema_version(connection: &Connection) -> Result<Option<u32>> {
    meta::get(connection, "schema")?
        .map(|v| {
            v.parse()
                .map_err(|_| Error::Storage("Unsupported state schema version".into()))
        })
        .transpose()
}
/// Each migration commits together with its history row and the version
/// bump, so a crash leaves either the previous version or the next, never a
/// half-applied schema.
fn migrate(connection: &mut Connection) -> Result<()> {
    let applied = schema_version(connection)?.unwrap_or(0);
    for migration in MIGRATIONS.iter().filter(|m| m.version > applied) {
        let tx = connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, \
             name TEXT NOT NULL, applied_at INTEGER NOT NULL)",
        )
        .map_err(sql)?;
        tx.execute_batch(migration.sql).map_err(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations VALUES(?1,?2,?3)",
            rusqlite::params![
                migration.version,
                migration.name,
                chrono::Utc::now().timestamp_millis()
            ],
        )
        .map_err(sql)?;
        meta::put(&tx, "schema", &migration.version.to_string())?;
        tx.commit().map_err(sql)?;
    }
    Ok(())
}
/// One-time import of the pre-SQLite JSON journal, recorded in `meta` so a
/// retained legacy file is never replayed.
fn import_legacy(connection: &mut Connection, legacy: &Path) -> Result<()> {
    if meta::exists(connection, "legacy_imported")? {
        return Ok(());
    }
    crate::security::verify_file_permissions(legacy)?;
    let records: std::collections::HashMap<String, crate::execution::SubmissionRecord> =
        serde_json::from_slice(&std::fs::read(legacy)?)?;
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sql)?;
    for record in records.values() {
        if let Some(tag) = record
            .request
            .get("tag")
            .and_then(serde_json::Value::as_str)
        {
            tx.execute(
                "INSERT INTO used_order_tags VALUES(?1,?2)",
                rusqlite::params![record.account, tag],
            )
            .map_err(sql)?;
        }
        tx.execute(
            "INSERT INTO submissions(id,account,operation,created_at,request,outcome,dedup_key) \
             VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![
                record.attempt_id,
                record.account,
                record.operation,
                record.created_at,
                serde_json::to_string(&record.request)?,
                record
                    .outcome
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?,
                record.dedup_key
            ],
        )
        .map_err(sql)?;
        if let Some(key) = &record.dedup_key {
            tx.execute("INSERT OR IGNORE INTO consumed_approvals VALUES(?1)", [key])
                .map_err(sql)?;
        }
    }
    meta::put(&tx, "legacy_imported", "1")?;
    tx.commit().map_err(sql)?;
    Ok(())
}
/// The `meta` key/value table: schema version, execution mode, operator halt,
/// paper run state and delivery cursors all live here. Values are opaque
/// strings; the `_json` variants encode with serde for structured values.
pub(crate) mod meta {
    use super::sql;
    use crate::error::Result;
    use rusqlite::{Connection, OptionalExtension};
    use serde::{Serialize, de::DeserializeOwned};

    pub(crate) fn get(c: &Connection, key: &str) -> Result<Option<String>> {
        c.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()
            .map_err(sql)
    }
    pub(crate) fn exists(c: &Connection, key: &str) -> Result<bool> {
        c.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key=?1)",
            [key],
            |r| r.get(0),
        )
        .map_err(sql)
    }
    /// Insert or overwrite.
    pub(crate) fn put(c: &Connection, key: &str, value: &str) -> Result<()> {
        c.execute(
            "INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value],
        )
        .map_err(sql)?;
        Ok(())
    }
    /// Insert only when the key is absent; an existing value is kept.
    pub(crate) fn insert_if_new(c: &Connection, key: &str, value: &str) -> Result<()> {
        c.execute("INSERT OR IGNORE INTO meta VALUES(?1,?2)", [key, value])
            .map_err(sql)?;
        Ok(())
    }
    pub(crate) fn delete(c: &Connection, key: &str) -> Result<()> {
        c.execute("DELETE FROM meta WHERE key=?1", [key])
            .map_err(sql)?;
        Ok(())
    }
    pub(crate) fn get_json<T: DeserializeOwned>(c: &Connection, key: &str) -> Result<Option<T>> {
        get(c, key)?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(Into::into)
    }
    pub(crate) fn put_json(c: &Connection, key: &str, value: &impl Serialize) -> Result<()> {
        put(c, key, &serde_json::to_string(value)?)
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
