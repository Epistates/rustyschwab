//! Private, transactional local state. SQLite files must be on a local filesystem.
//!
//! A [`StateDatabase`] validates and migrates its file once, on first use, and
//! then serves operations from a small pool of open connections. Clones share
//! the pool; a second handle on the same path is a separate opener, so
//! cross-process safety comes from SQLite's WAL locking, not from this type.
use crate::error::{Error, Result};
use rusqlite::{Connection, backup::Progress};
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
    /// Data work SQL cannot do, run after `sql` in the same transaction.
    backfill: Option<fn(&Connection) -> Result<()>>,
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
        backfill: None,
    },
    // Version 1 files tracked this import with an ad hoc `tags_imported` flag.
    Migration {
        version: 2,
        name: "import_order_tags",
        sql: "INSERT OR IGNORE INTO used_order_tags SELECT account,json_extract(request,'$.tag') FROM submissions WHERE json_type(request,'$.tag')='text';
            DELETE FROM meta WHERE key='tags_imported';",
        backfill: None,
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
        backfill: None,
    },
    // Intents a principal asked for that need an operator's approval, so
    // the operator approves an order they can read, not a bare digest.
    Migration {
        version: 4,
        name: "pending_intents",
        sql: "CREATE TABLE IF NOT EXISTS intents(digest TEXT PRIMARY KEY, principal TEXT NOT NULL, tool TEXT NOT NULL, arguments TEXT NOT NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL, seen INTEGER NOT NULL DEFAULT 1);
            CREATE INDEX IF NOT EXISTS intents_recent ON intents(principal,last_seen);",
        backfill: None,
    },
    // Each audit row names the hash of the one before it and carries its
    // own, so an edited, deleted or reordered row is detectable (see
    // `audit_chain`). Existing rows are chained in sequence order here.
    // `audit_anchor` is where a deliberate deletion of the oldest rows
    // records the last deleted row, so verification can start after it.
    Migration {
        version: 5,
        name: "audit_hash_chain",
        sql: "ALTER TABLE audit ADD COLUMN prev_hash TEXT NOT NULL DEFAULT '';
            ALTER TABLE audit ADD COLUMN hash TEXT NOT NULL DEFAULT '';
            CREATE TABLE IF NOT EXISTS audit_anchor(id INTEGER PRIMARY KEY CHECK(id=1), sequence INTEGER NOT NULL, hash TEXT NOT NULL);",
        backfill: Some(crate::audit_chain::backfill),
    },
    // What a client did, call by call, for someone watching it live: a
    // bounded log beside the audit trail, not part of its hash chain (see
    // `OrderJournal::record_activity`).
    Migration {
        version: 6,
        name: "activity",
        sql: "CREATE TABLE IF NOT EXISTS activity(sequence INTEGER PRIMARY KEY AUTOINCREMENT, at INTEGER NOT NULL, subject TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL);",
        backfill: None,
    },
];
const SCHEMA_VERSION: u32 = MIGRATIONS[MIGRATIONS.len() - 1].version;

/// Temporary contention diagnostics, written as JSON lines to the file named
/// by `RUSTYSCHWAB_DIAG`.
#[doc(hidden)]
pub mod diag {
    use std::{
        cell::Cell,
        io::Write,
        sync::{
            OnceLock,
            atomic::{AtomicU64, Ordering},
        },
        time::Instant,
    };
    static START: OnceLock<Instant> = OnceLock::new();
    static OUT: OnceLock<Option<parking_lot::Mutex<std::fs::File>>> = OnceLock::new();
    static IDS: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        pub static POLLS: Cell<u32> = const { Cell::new(0) };
        pub static SLEPT_US: Cell<u64> = const { Cell::new(0) };
        pub static NOMINAL_MS: Cell<u64> = const { Cell::new(0) };
        pub static GAVE_UP: Cell<bool> = const { Cell::new(false) };
        pub static CTX: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    }
    pub fn next_id() -> u64 {
        IDS.fetch_add(1, Ordering::Relaxed)
    }
    pub fn now_us() -> u64 {
        START.get_or_init(Instant::now).elapsed().as_micros() as u64
    }
    fn out() -> &'static Option<parking_lot::Mutex<std::fs::File>> {
        OUT.get_or_init(|| {
            std::env::var_os("RUSTYSCHWAB_DIAG").map(|path| {
                parking_lot::Mutex::new(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .expect("diagnostics file"),
                )
            })
        })
    }
    pub fn emit(value: serde_json::Value) {
        if let Some(file) = out() {
            let _ = writeln!(file.lock(), "{value}");
        }
    }
    pub fn reset_waits() {
        POLLS.set(0);
        SLEPT_US.set(0);
        NOMINAL_MS.set(0);
        GAVE_UP.set(false);
    }
    /// SQLite's own `sqliteDefaultBusyCallback` (the handler `busy_timeout`
    /// installs), step for step, sleeping through the same VFS call, and
    /// counting what it does.
    pub fn default_busy(count: i32) -> bool {
        const DELAYS: [i64; 12] = [1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100];
        const TOTALS: [i64; 12] = [0, 1, 3, 8, 18, 33, 53, 78, 103, 128, 178, 228];
        let timeout = super::BUSY_TIMEOUT.as_millis() as i64;
        let count = i64::from(count);
        let (mut delay, prior) = if count < 12 {
            (DELAYS[count as usize], TOTALS[count as usize])
        } else {
            (100, 228 + 100 * (count - 11))
        };
        if prior + delay > timeout {
            delay = timeout - prior;
            if delay <= 0 {
                GAVE_UP.set(true);
                return false;
            }
        }
        let started = Instant::now();
        #[allow(unsafe_code)]
        // SAFETY: sqlite3_sleep takes a millisecond count and has no other
        // preconditions; it sleeps through the default VFS, as the default
        // busy handler does.
        unsafe {
            rusqlite::ffi::sqlite3_sleep(delay as i32);
        }
        POLLS.set(POLLS.get() + 1);
        SLEPT_US.set(SLEPT_US.get() + started.elapsed().as_micros() as u64);
        NOMINAL_MS.set(NOMINAL_MS.get() + delay as u64);
        true
    }
}

#[derive(Clone)]
pub struct StateDatabase {
    inner: Arc<Inner>,
}
struct Inner {
    id: u64,
    path: PathBuf,
    legacy: Option<PathBuf>,
    initialized: tokio::sync::OnceCell<()>,
    slots: Arc<tokio::sync::Semaphore>,
    idle: parking_lot::Mutex<Vec<(u64, Connection)>>,
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
                id: diag::next_id(),
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
        let t_call = diag::now_us();
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
        let t_init = diag::now_us();
        let permit = inner
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Storage("State worker pool closed".into()))?;
        let t_permit = diag::now_us();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let t_start = diag::now_us();
            let (conn_id, mut connection) = inner.checkout()?;
            diag::CTX.set((inner.id, conn_id));
            let result = f(&mut connection);
            let t_end = diag::now_us();
            diag::emit(serde_json::json!({
                "ev": "run", "handle": inner.id, "conn": conn_id, "t_call": t_call,
                "t_init": t_init, "t_permit": t_permit, "t_start": t_start, "t_end": t_end,
                "ok": result.is_ok(),
            }));
            inner.checkin((conn_id, connection));
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
            conn.backup("main", &destination, None::<fn(Progress)>)
                .map_err(sql)?;
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
            source
                .backup("main", &target_path, None::<fn(Progress)>)
                .map_err(sql)?;
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
    fn checkout(&self) -> Result<(u64, Connection)> {
        let pooled = self.idle.lock().pop();
        match pooled {
            Some(connection) => Ok(connection),
            None => {
                let id = diag::next_id();
                Ok((id, open_connection(&self.path, self.id, id)?))
            }
        }
    }
    /// A connection left mid-transaction is closed (rolling it back) rather
    /// than handed to the next caller.
    fn checkin(&self, connection: (u64, Connection)) {
        if connection.1.is_autocommit() {
            let mut idle = self.idle.lock();
            if idle.len() < POOL_SIZE {
                idle.push(connection);
            }
        }
    }
    /// Cross-process initialization is serialized by a lock file so two
    /// openers cannot race the schema migration.
    fn initialize(&self) -> Result<(u64, Connection)> {
        let t_start = diag::now_us();
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
        let t_locked = diag::now_us();
        let id = diag::next_id();
        let mut connection = open_connection(&self.path, self.id, id)?;
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
        diag::emit(serde_json::json!({
            "ev": "init", "handle": self.id, "conn": id, "t_start": t_start,
            "t_locked": t_locked, "t_end": diag::now_us(),
        }));
        Ok((id, connection))
    }
}
fn open_connection(path: &Path, handle: u64, id: u64) -> Result<Connection> {
    let mut connection = Connection::open(path).map_err(sql)?;
    connection.busy_handler(Some(diag::default_busy)).map_err(sql)?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;")
        .map_err(sql)?;
    // SQLite's default auto-checkpoint (`sqlite3WalDefaultHook`): a PASSIVE
    // checkpoint once the WAL holds 1000 frames, timed.
    connection
        .wal_hook(Some(move |wal: &mut rusqlite::hooks::Wal, pages: i32| {
            let t = diag::now_us();
            let checkpoint = (pages >= 1000).then(|| wal.checkpoint().is_ok());
            diag::emit(serde_json::json!({
                "ev": "wal", "handle": handle, "conn": id, "t": t, "pages": pages,
                "ckpt_ok": checkpoint, "ckpt_us": diag::now_us() - t,
            }));
            Ok(())
        }))
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
        if let Some(backfill) = migration.backfill {
            backfill(&tx)?;
        }
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

/// Append one event to the hash-chained audit table.
///
/// Reading the previous hash and inserting must not interleave with another
/// writer, in this process or another, or two rows would claim the same
/// predecessor. On a connection with no transaction open this takes the
/// write lock itself. Inside a transaction it requires that the caller
/// already holds it (`BEGIN IMMEDIATE`, or an earlier write): a deferred
/// transaction that has only read would race for the lock after reading the
/// head, so it is refused rather than trusted.
pub(crate) fn audit(
    conn: &Connection,
    kind: &str,
    subject: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    let at = chrono::Utc::now().timestamp_millis();
    let payload = serde_json::to_string(payload)?;
    if !conn.is_autocommit() {
        if conn.transaction_state(None::<&str>).map_err(sql)? != rusqlite::TransactionState::Write {
            return Err(Error::Storage(
                "Audit events must be written inside a write transaction".into(),
            ));
        }
        return crate::audit_chain::append(conn, at, kind, subject, &payload);
    }
    diag::reset_waits();
    let t0 = diag::now_us();
    let begun = conn.execute_batch("BEGIN IMMEDIATE");
    let t1 = diag::now_us();
    let (handle, conn_id) = diag::CTX.get();
    let report = |t2: u64, t3: u64, outcome: String| {
        diag::emit(serde_json::json!({
            "ev": "audit", "handle": handle, "conn": conn_id, "subject": subject,
            "t0": t0, "t1": t1, "t2": t2, "t3": t3, "polls": diag::POLLS.get(),
            "slept_us": diag::SLEPT_US.get(), "nominal_ms": diag::NOMINAL_MS.get(),
            "gave_up": diag::GAVE_UP.get(), "outcome": outcome,
        }));
    };
    if let Err(error) = begun {
        report(t1, t1, format!("begin: {error}"));
        return Err(sql(error));
    }
    let appended = crate::audit_chain::append(conn, at, kind, subject, &payload);
    let t2 = diag::now_us();
    let appended = appended.and_then(|()| conn.execute_batch("COMMIT").map_err(sql));
    let t3 = diag::now_us();
    report(
        t2,
        t3,
        appended
            .as_ref()
            .err()
            .map_or_else(|| "ok".to_owned(), ToString::to_string),
    );
    if appended.is_err() && !conn.is_autocommit() {
        // Best effort: the error being returned is the one that matters, and
        // a connection left open is dropped by the pool.
        let _ = conn.execute_batch("ROLLBACK");
    }
    appended
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
