//! Private, transactional local state. SQLite files must be on a local filesystem.
//!
//! A [`StateDatabase`] validates and migrates its file once, on first use, and
//! then serves operations from a small pool of open connections. Clones share
//! the pool; a second handle on the same path is a separate opener, so
//! cross-process safety comes from SQLite's WAL locking, not from this type.
//!
//! # Writers take turns
//!
//! SQLite lets one connection write at a time, and a connection that finds
//! the write lock taken can only poll for it, sleeping longer the longer it
//! has waited (its busy handler, up to 100 ms between attempts). SQLite
//! promises no order among waiters, and a writer that commits and asks again
//! at once is nearly always first back, so one busy writer can keep another
//! out until its timeout. `StateDatabase::write` therefore makes writers
//! take turns. Those on one handle queue in the order they asked, and the one
//! at the head locks the file `state.write.lock`, beside `state.sqlite3`, for
//! the length of its transaction. Other handles, in this process or another,
//! wait for that lock in the operating system (`flock`, or `LockFileEx` on
//! Windows), which wakes a waiter when it is released instead of leaving it to
//! poll. The wait is best effort in order: `flock` does not promise to serve
//! waiters first come, first served. The operating system releases the lock
//! when its holder exits or crashes. A write whose turn has not come within
//! five seconds, or whose caller gave up first, fails with
//! [`Error::StateBusy`] and never runs: nothing was written, and the same
//! call can be made again.
//!
//! A writer first tries for the lock where a blocking task runs. If another
//! handle or process holds it, the wait goes to a thread of its own per
//! handle, outside Tokio's blocking pool (see `lock_thread`).
//!
//! The lock file orders only writers that use it. One that does not (an
//! earlier release of the SDK, the `sqlite3` shell) is waited for through
//! SQLite's busy timeout, as before.
use crate::error::{Error, Result};
use rusqlite::{Connection, backup::Progress};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::Duration,
};

/// Connections kept open per database. Writers take turns (see the module
/// documentation), so a small pool bounds blocking threads without starving
/// readers or backups.
const POOL_SIZE: usize = 4;
/// How long a write waits for its turn before failing with
/// [`Error::StateBusy`], and how long a connection waits for a lock held
/// outside the turn order before SQLite fails it with `database is locked`.
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
    /// The turn to write. Writers on this handle queue on it in the order
    /// they asked (Tokio's mutex is fair).
    turn: Arc<tokio::sync::Mutex<()>>,
    /// The write lock file, opened on first use.
    lock_file: Arc<LockFile>,
    /// Asks the handle's lock thread to wait for the write lock file, once
    /// one has been started.
    locker: parking_lot::Mutex<Option<mpsc::Sender<LockRequest>>>,
}

/// A write whose turn did not come in time: it never ran.
fn write_turn_busy() -> Error {
    Error::StateBusy(
        "the write turn did not come in time: another writer holds the state file".into(),
    )
}
/// An opener that gave up waiting for another to finish preparing the file.
fn init_busy() -> Error {
    Error::StateBusy(
        "the initialization lock was not granted in time: another opener is still preparing the state file"
            .into(),
    )
}

/// The write lock file beside a database, opened on first use. It is shared
/// by the handle's writers, one at a time: whoever holds this handle's turn.
struct LockFile {
    path: PathBuf,
    file: parking_lot::Mutex<Option<Arc<std::fs::File>>>,
}
impl LockFile {
    fn open(&self) -> Result<Arc<std::fs::File>> {
        let mut file = self.file.lock();
        if let Some(file) = file.as_ref() {
            return Ok(file.clone());
        }
        let opened = Arc::new(crate::auth::token_store::open_lock_file(&self.path)?);
        *file = Some(opened.clone());
        Ok(opened)
    }
}

/// The write lock file, locked, and this handle's turn with it. Dropping it,
/// on any thread and however (a panic included), unlocks the file and then
/// passes the turn to the next writer on the handle.
struct HeldLock {
    file: Arc<std::fs::File>,
    owner: Arc<LockFile>,
    _turn: tokio::sync::OwnedMutexGuard<()>,
}
impl Drop for HeldLock {
    fn drop(&mut self) {
        // A file that cannot be unlocked is closed, which unlocks it, and
        // opened again for the next writer.
        if self.file.unlock().is_err() {
            *self.owner.file.lock() = None;
        }
    }
}

/// What a first, non-blocking try for the write lock file came to.
enum Attempt {
    Held(HeldLock),
    /// Another handle or process holds it.
    Busy {
        file: Arc<std::fs::File>,
        owner: Arc<LockFile>,
        turn: tokio::sync::OwnedMutexGuard<()>,
    },
}

/// A writer's wait for the write lock file, answered on `reply`. A request
/// whose `reply` has been dropped is abandoned: its caller timed out or went
/// away, and the lock thread skips it, or unlocks at once if it was already
/// waiting for the lock.
struct LockRequest {
    file: Arc<std::fs::File>,
    owner: Arc<LockFile>,
    turn: tokio::sync::OwnedMutexGuard<()>,
    reply: tokio::sync::oneshot::Sender<Result<HeldLock>>,
}

/// Wait for the write lock file on behalf of the handle's writers, one request
/// at a time and in the order they were made, for as long as the handle lives.
///
/// This is a thread of its own, not a task on Tokio's blocking pool, on
/// purpose. A thread blocked in `flock` or `LockFileEx` cannot be cancelled
/// without signals, which a library must not install, and another process can
/// hold the lock for as long as it likes. On the pool, such a wait would keep
/// a worker busy and make dropping the runtime wait for it; here it ends only
/// the thread's own wait, and when the lock is finally free the thread finds
/// the request abandoned and unlocks at once. Polling `try_lock` instead
/// would end the wait on time but lose the wake-up on release, and with it
/// the order that makes writers take turns. Do not move this into
/// `spawn_blocking`.
fn lock_thread(requests: mpsc::Receiver<LockRequest>) {
    for request in requests {
        let LockRequest {
            file,
            owner,
            turn,
            reply,
        } = request;
        if reply.is_closed() {
            continue;
        }
        let answer = match file.lock() {
            Ok(()) => Ok(HeldLock {
                file,
                owner,
                _turn: turn,
            }),
            Err(error) => Err(Error::Io(error)),
        };
        // Should nobody be waiting for the answer any more, the send fails
        // and drops the lock, which unlocks the file.
        let _ = reply.send(answer);
    }
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
        let path_for_lock = path.with_extension("write.lock");
        Self {
            inner: Arc::new(Inner {
                path,
                legacy,
                initialized: tokio::sync::OnceCell::new(),
                slots: Arc::new(tokio::sync::Semaphore::new(POOL_SIZE)),
                idle: parking_lot::Mutex::new(Vec::with_capacity(POOL_SIZE)),
                turn: Arc::new(tokio::sync::Mutex::new(())),
                lock_file: Arc::new(LockFile {
                    path: path_for_lock,
                    file: parking_lot::Mutex::new(None),
                }),
                locker: parking_lot::Mutex::new(None),
            }),
        }
    }
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
    /// Validate and migrate the file on the first call; a failed
    /// initialization is retried by the next call rather than poisoning the
    /// handle.
    async fn initialized(&self) -> Result<()> {
        self.inner
            .initialized
            .get_or_try_init(|| async {
                let inner = self.inner.clone();
                let connection = tokio::task::spawn_blocking(move || inner.initialize())
                    .await
                    .map_err(|_| Error::Storage("State worker failed".into()))??;
                self.inner.idle.lock().push(connection);
                Ok::<_, Error>(())
            })
            .await
            .copied()
    }
    /// Validate and migrate the file now, on the calling thread, so that an
    /// error in the state location surfaces to whoever builds on it and the
    /// first operation does not pay for creating the file.
    pub(crate) fn open(&self) -> Result<()> {
        if self.inner.initialized.initialized() {
            return Ok(());
        }
        let connection = self.inner.initialize()?;
        // Another caller may have opened it meanwhile: keep theirs.
        if self.inner.initialized.set(()).is_ok() {
            self.inner.idle.lock().push(connection);
        }
        Ok(())
    }
    /// Run `f` on a pooled connection from a blocking thread. The first call
    /// validates and migrates the file; a failed initialization is retried by
    /// the next call rather than poisoning the handle.
    pub async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.initialized().await?;
        let inner = self.inner.clone();
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
    /// [`run`](Self::run) for an operation that writes: `f` runs once it is
    /// this handle's turn to write and no other handle or process is writing
    /// in turn (see the module documentation). If the turn has not come
    /// within five seconds this fails with [`Error::StateBusy`] and `f`
    /// never runs, and neither does it if the caller stops waiting first (the
    /// future is dropped); once `f` has started it runs to the end.
    pub(crate) async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.write_within(BUSY_TIMEOUT, f).await
    }
    /// [`write`](Self::write), waiting at most `wait` for the turn.
    async fn write_within<T: Send + 'static>(
        &self,
        wait: Duration,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.initialized().await?;
        let expiry = tokio::time::Instant::now() + wait;
        let held = self.inner.lock_for_write(expiry).await?;
        // A connection is taken only once the lock is held, so a wait for
        // the lock holds no slot. Readers never wait for the lock, so this
        // wait cannot deadlock.
        let permit = tokio::time::timeout_at(expiry, self.inner.slots.clone().acquire_owned())
            .await
            .map_err(|_| write_turn_busy())?
            .map_err(|_| Error::Storage("State worker pool closed".into()))?;
        let inner = self.inner.clone();
        let (outcome, finished) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // The caller may have stopped waiting since the lock was granted;
            // a write it no longer waits for is dropped, and `held` with it.
            if outcome.is_closed() {
                return;
            }
            let result = inner.checkout().and_then(|mut connection| {
                let result = f(&mut connection);
                drop(held);
                inner.checkin(connection);
                result
            });
            let _ = outcome.send(result);
        });
        finished
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
            .write(|c| {
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
    /// Wait for this handle's turn, then for the write lock file, until
    /// `expiry`. Dropping the future abandons the request.
    async fn lock_for_write(&self, expiry: tokio::time::Instant) -> Result<HeldLock> {
        let turn = tokio::time::timeout_at(expiry, self.turn.clone().lock_owned())
            .await
            .map_err(|_| write_turn_busy())?;
        // The lock is free more often than not, and a try for it is quick:
        // make it where a blocking task would run, so that a paused clock
        // (`tokio::time::pause`) waits for it as it does for any other.
        let owner = self.lock_file.clone();
        let attempt = tokio::task::spawn_blocking(move || {
            let file = owner.open()?;
            match file.try_lock() {
                Ok(()) => Ok(Attempt::Held(HeldLock {
                    file,
                    owner,
                    _turn: turn,
                })),
                Err(std::fs::TryLockError::WouldBlock) => Ok(Attempt::Busy { file, owner, turn }),
                Err(std::fs::TryLockError::Error(error)) => Err(Error::Io(error)),
            }
        })
        .await
        .map_err(|_| Error::Storage("State worker failed".into()))??;
        let (file, owner, turn) = match attempt {
            Attempt::Held(held) => return Ok(held),
            Attempt::Busy { file, owner, turn } => (file, owner, turn),
        };
        let (reply, granted) = tokio::sync::oneshot::channel();
        let stopped = || Error::Storage("State lock thread stopped".into());
        {
            let mut locker = self.locker.lock();
            if locker.is_none() {
                // A thread of our own, never `spawn_blocking`: a thread blocked
                // in `flock` cannot be cancelled without signals, which a
                // library must not install, so this wait must be somewhere
                // runtime shutdown never waits for it (see `lock_thread`).
                let (requests, receiver) = mpsc::channel();
                std::thread::Builder::new()
                    .name("rustyschwab-state-lock".into())
                    .spawn(move || lock_thread(receiver))
                    .map_err(|error| Error::Storage(format!("State lock thread: {error}")))?;
                *locker = Some(requests);
            }
            let request = LockRequest {
                file,
                owner,
                turn,
                reply,
            };
            if locker.as_ref().is_some_and(|l| l.send(request).is_err()) {
                // The thread is gone: start another for the next write.
                *locker = None;
                return Err(stopped());
            }
        }
        tokio::time::timeout_at(expiry, granted)
            .await
            .map_err(|_| write_turn_busy())?
            .map_err(|_| stopped())?
    }
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
                    return Err(init_busy());
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
    let has_meta = table_exists(connection, "meta")?;
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
/// The pending migrations commit together, each with its history row, and
/// with the version bump, so a crash leaves either the version before them
/// or the newest, never a half-applied schema. A new file is created in one
/// commit rather than one per migration: each commit is a flush to disk.
fn migrate(connection: &mut Connection) -> Result<()> {
    let applied = if table_exists(connection, "meta")? {
        schema_version(connection)?.unwrap_or(0)
    } else {
        0
    };
    let pending: Vec<&Migration> = MIGRATIONS.iter().filter(|m| m.version > applied).collect();
    if pending.is_empty() {
        return Ok(());
    }
    let tx = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(sql)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, \
         name TEXT NOT NULL, applied_at INTEGER NOT NULL)",
    )
    .map_err(sql)?;
    for migration in pending {
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
    }
    tx.commit().map_err(sql)
}
fn table_exists(connection: &Connection, name: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [name],
            |r| r.get(0),
        )
        .map_err(sql)
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
    conn.execute_batch("BEGIN IMMEDIATE").map_err(sql)?;
    let appended = crate::audit_chain::append(conn, at, kind, subject, &payload)
        .and_then(|()| conn.execute_batch("COMMIT").map_err(sql));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn insert(c: &Connection, key: &str) -> Result<()> {
        c.execute(
            "INSERT INTO meta VALUES(?1,'') ON CONFLICT(key) DO NOTHING",
            [key],
        )
        .map(drop)
        .map_err(sql)
    }

    /// Hold SQLite's write lock for `hold`, then commit an insert of `key`.
    fn hold_and_insert(c: &mut Connection, key: &str, hold: Duration) -> Result<()> {
        let tx = c
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(sql)?;
        std::thread::sleep(hold);
        insert(&tx, key)?;
        tx.commit().map_err(sql)
    }

    /// Two handles on one file, as two processes would have: one writes
    /// back to back, and the other asks for a single write. It is served
    /// within a turn or two. Polling for SQLite's lock, the busy writer was
    /// almost always first back, so the other waited through many of its
    /// commits and could give up after five seconds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_writer_on_another_handle_gets_a_turn_beside_a_steady_one() {
        let dir = tempfile::tempdir().unwrap();
        let busy = StateDatabase::new(dir.path().join("steady.json"));
        let other = StateDatabase::new(busy.path().to_owned());
        busy.run(|_| Ok(())).await.unwrap();
        other.run(|_| Ok(())).await.unwrap();
        let committed = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let steady = {
            let (busy, committed, done) = (busy.clone(), committed.clone(), done.clone());
            tokio::spawn(async move {
                // Longer than the timeout in all, should the other write
                // have to wait for this to stop.
                for n in 0..300 {
                    if done.load(Ordering::Relaxed) {
                        break;
                    }
                    let committed = committed.clone();
                    busy.write(move |c| {
                        hold_and_insert(c, &format!("steady{n}"), Duration::from_millis(25))?;
                        committed.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    })
                    .await
                    .unwrap();
                }
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = committed.load(Ordering::Relaxed);
        other.write(|c| insert(c, "other")).await.unwrap();
        let during = committed.load(Ordering::Relaxed) - before;
        done.store(true, Ordering::Relaxed);
        steady.await.unwrap();
        assert!(during <= 3, "{during} writes went ahead of a waiting one");
    }

    /// Writers on one handle are served in the order they asked: none gets
    /// a second turn while another is waiting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn writers_on_one_handle_take_turns_in_the_order_they_asked() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("turns.json"));
        database.run(|_| Ok(())).await.unwrap();
        let order = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let tasks: Vec<_> = (0..4)
            .map(|task| {
                let (database, order) = (database.clone(), order.clone());
                tokio::spawn(async move {
                    for n in 0..5 {
                        let order = order.clone();
                        database
                            .write(move |c| {
                                order.lock().push(task);
                                hold_and_insert(
                                    c,
                                    &format!("t{task}n{n}"),
                                    Duration::from_millis(25),
                                )
                            })
                            .await
                            .unwrap();
                    }
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        let order = order.lock();
        assert_eq!(order.len(), 20);
        assert!(order.windows(2).all(|w| w[0] != w[1]), "{order:?}");
    }

    /// A write whose turn does not come fails as busy and never runs; the turn it was waiting for passes to the next writer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_that_gets_no_turn_fails_as_busy_and_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("held.json"));
        database.run(|_| Ok(())).await.unwrap();
        let held = hold_write_lock(&database);
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let refused = database
            .write_within(Duration::from_millis(200), move |c| {
                flag.store(true, Ordering::Relaxed);
                insert(c, "late")
            })
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, Error::StateBusy(m) if m.contains("write turn")),
            "{refused:?}"
        );
        held.unlock().unwrap();
        database.write(|c| insert(c, "next")).await.unwrap();
        assert!(!ran.load(Ordering::Relaxed));
        let late: bool = database.run(|c| meta::exists(c, "late")).await.unwrap();
        assert!(!late);
    }

    /// Another process in the middle of a write: the write lock file,
    /// locked from a handle of its own.
    fn hold_write_lock(database: &StateDatabase) -> std::fs::File {
        let held =
            crate::auth::token_store::open_lock_file(&database.path().with_extension("write.lock"))
                .unwrap();
        held.lock().unwrap();
        held
    }

    /// A caller that stops waiting (its own deadline, a cancelled task) takes
    /// its write with it: when the lock comes free, the write is not run for
    /// a caller that has gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_whose_caller_went_away_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("gone.json"));
        database.run(|_| Ok(())).await.unwrap();
        let held = hold_write_lock(&database);
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        // The caller's deadline is shorter than the write's own wait.
        let waited = tokio::time::timeout(
            Duration::from_millis(200),
            database.write(move |c| {
                flag.store(true, Ordering::Relaxed);
                insert(c, "gone")
            }),
        )
        .await;
        assert!(waited.is_err(), "the lock was held: {waited:?}");
        // And a task that is aborted rather than timed out.
        let flag = ran.clone();
        let handle = database.clone();
        let task = tokio::spawn(async move {
            handle
                .write(move |c| {
                    flag.store(true, Ordering::Relaxed);
                    insert(c, "aborted")
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        let _ = task.await;
        held.unlock().unwrap();
        // Requests are served in order, so once this write is done, both
        // earlier ones have been given up on.
        database.write(|c| insert(c, "next")).await.unwrap();
        assert!(!ran.load(Ordering::Relaxed));
        for key in ["gone", "aborted"] {
            let present = database.run(move |c| meta::exists(c, key)).await.unwrap();
            assert!(!present, "{key}");
        }
    }

    /// A writer waiting for a lock that another process keeps does not hold
    /// up the end of the runtime it waits in: the wait is on a thread of its
    /// own, not on the runtime's blocking pool.
    #[test]
    fn a_runtime_shuts_down_while_a_writer_waits_for_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("shutdown.json"));
        let (done, finished) = std::sync::mpsc::channel();
        let runner = {
            let database = database.clone();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(database.run(|_| Ok(()))).unwrap();
                let held = hold_write_lock(&database);
                runtime.spawn({
                    let database = database.clone();
                    async move { database.write(|c| insert(c, "waiting")).await }
                });
                // Long enough for the writer to be waiting in the operating
                // system's queue for the lock.
                std::thread::sleep(Duration::from_millis(300));
                drop(runtime);
                done.send(()).unwrap();
                held.unlock().unwrap();
            })
        };
        finished
            .recv_timeout(Duration::from_secs(3))
            .expect("dropping the runtime waited for the writer");
        runner.join().unwrap();
    }

    /// A write that finds the lock free is not cut short by a paused clock:
    /// tests that pause Tokio's time (`start_paused`) run the SDK's writes
    /// as they do any other blocking work, and the five seconds a write may
    /// wait for its turn do not pass while it runs.
    #[tokio::test(start_paused = true)]
    async fn a_write_completes_under_a_paused_clock() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("paused.json"));
        for n in 0..5 {
            database
                .write(move |c| {
                    std::thread::sleep(Duration::from_millis(50));
                    insert(c, &format!("paused{n}"))
                })
                .await
                .unwrap();
        }
    }

    /// Commits in `database`'s write-ahead log: its frames that close a
    /// transaction record the size of the database after it.
    fn wal_commits(database: &StateDatabase) -> usize {
        let wal = std::fs::read(database.path().with_extension("sqlite3-wal")).unwrap();
        let page_size = u32::from_be_bytes(wal[8..12].try_into().unwrap()) as usize;
        wal[32..]
            .chunks_exact(24 + page_size)
            .filter(|frame| frame[4..8] != [0; 4])
            .count()
    }

    /// Every flush to disk costs a commit, and on a slow disk a new file's
    /// six migrations took seconds: all of them are one commit.
    #[tokio::test]
    async fn a_new_state_file_is_created_in_one_commit() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("fresh.json"));
        database.run(|_| Ok(())).await.unwrap();
        assert_eq!(wal_commits(&database), 1);
        let versions: i64 = database
            .run(|c| {
                c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
                    .map_err(sql)
            })
            .await
            .unwrap();
        assert_eq!(versions, MIGRATIONS.len() as i64);
    }
}
