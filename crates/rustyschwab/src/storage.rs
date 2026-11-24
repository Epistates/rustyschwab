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
//!
//! # Opening
//!
//! A new file is built whole in a private temporary file beside it
//! (`state.sqlite3.<random>.tmp`): created as private as the state file will
//! be, filled with the schema in one transaction without a journal or a
//! flush, switched to WAL mode, closed, flushed once, and moved into place
//! with a rename that fails if the name is taken. A crash at any point leaves
//! no state file or a complete one, never a partial one, and two openers that
//! create it at the same moment need no lock: the one whose move fails
//! discards its copy and opens the other's. A crash while building leaves its
//! temporary file behind; the next opener removes those that are more than an
//! hour old.
//!
//! An existing file that is behind (it needs a migration, or the import of a
//! legacy journal) is prepared under a second lock file, `state.init.lock`, so
//! that two openers, in this process or another, cannot race the migration. A
//! file that is already current is opened without it. An opener that finds the
//! lock taken waits for it like a writer waits for its turn: in the operating
//! system, on the handle's lock thread, for at most five seconds from when the
//! wait starts, and the wait ends sooner if its caller drops the future (a
//! caller's deadline, `tokio::time::timeout` around the call, does that). The
//! lock thread gives the lock back as soon as it is granted to a request that
//! was dropped. When the five seconds run out the call fails with
//! [`Error::StateBusy`] and nothing has been written. The wait for the turn to
//! write starts only once opening is done, with its own five seconds. The
//! constructor (`StateDatabase::open`, which `SchwabClient::new` calls) has no
//! future to drop: it polls for the lock on the calling thread for at most
//! five seconds.
//!
//! These waits are for locks that may not be released, such as one another
//! process holds. An opener that finds another on its own handle already
//! building the file waits for that work to finish, as it would for any work
//! under way.
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
/// The longest an opener waits for the init lock, counted from when it starts
/// waiting: for the synchronous constructor (`StateDatabase::open`, which
/// `SchwabClient::new` calls, and which has no future to drop) and for the
/// async path (`run`, `write`), where the caller can end the wait sooner by
/// dropping the future. Past it they fail with [`Error::StateBusy`].
const INIT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// How old a temporary file left by a crash while building a new state file
/// must be before an opener removes it. A build takes a second or two, even on
/// a slow disk, so nothing that old has an owner.
const STALE_BUILD_AGE: Duration = Duration::from_secs(60 * 60);

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

/// A state file opened without the init lock.
enum Opened {
    /// Current: nothing to migrate and nothing to import.
    Ready(Connection),
    /// Behind, or not what it should be: prepare it under the init lock, which
    /// also decides whether it is refused.
    Behind(Connection),
}

/// A point at which a test process, run as a child, stops as a crash would:
/// the process aborts, with no destructor run and nothing flushed. Named by
/// `RUSTYSCHWAB_TEST_CRASH_AT`. Does nothing outside the unit tests.
#[cfg(test)]
pub(crate) fn failpoint(name: &str) {
    if std::env::var("RUSTYSCHWAB_TEST_CRASH_AT").is_ok_and(|at| at == name) {
        std::process::abort();
    }
}
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn failpoint(_: &str) {}

#[derive(Clone)]
pub struct StateDatabase {
    inner: Arc<Inner>,
}
struct Inner {
    path: PathBuf,
    legacy: Option<PathBuf>,
    initialized: tokio::sync::OnceCell<()>,
    /// Openers on this handle take turns at creating or opening the file, so
    /// that the ones behind the first find it ready instead of each building
    /// one.
    opening: tokio::sync::Mutex<()>,
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

/// A lock file, locked, and the turn that goes with it. Dropping it, on any
/// thread and however (a panic included), unlocks the file and then passes the
/// turn on.
///
/// The write lock file is shared by the handle's writers, who hold it in
/// turn: `owner` is its slot, and `turn` the handle's turn to write. An
/// opener's init lock file is opened for the one use and has neither.
struct HeldLock {
    file: Arc<std::fs::File>,
    owner: Option<Arc<LockFile>>,
    _turn: Option<tokio::sync::OwnedMutexGuard<()>>,
}
impl Drop for HeldLock {
    fn drop(&mut self) {
        // A file that cannot be unlocked is closed, which unlocks it, and
        // opened again for the next writer.
        if self.file.unlock().is_err()
            && let Some(owner) = &self.owner
        {
            *owner.file.lock() = None;
        }
    }
}

/// A lock file that another handle or process holds, and the turn that goes
/// with it: what a writer or an opener waits with.
struct Waiting {
    file: Arc<std::fs::File>,
    owner: Option<Arc<LockFile>>,
    turn: Option<tokio::sync::OwnedMutexGuard<()>>,
}

/// What a first, non-blocking try for a lock file came to.
enum Attempt {
    Held(HeldLock),
    Busy(Waiting),
}

/// Try once to lock `file`, without waiting.
fn try_lock_file(
    file: Arc<std::fs::File>,
    owner: Option<Arc<LockFile>>,
    turn: Option<tokio::sync::OwnedMutexGuard<()>>,
) -> Result<Attempt> {
    match file.try_lock() {
        Ok(()) => Ok(Attempt::Held(HeldLock {
            file,
            owner,
            _turn: turn,
        })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(Attempt::Busy(Waiting { file, owner, turn })),
        Err(std::fs::TryLockError::Error(error)) => Err(Error::Io(error)),
    }
}

/// A wait for a lock file, answered on `reply`. A request
/// whose `reply` has been dropped is abandoned: its caller timed out or went
/// away, and the lock thread skips it, or unlocks at once if it was already
/// waiting for the lock.
struct LockRequest {
    waiting: Waiting,
    reply: tokio::sync::oneshot::Sender<Result<HeldLock>>,
}

/// Wait for a lock file on behalf of the handle's writers and its opener, one request
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
            waiting: Waiting { file, owner, turn },
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
                opening: tokio::sync::Mutex::new(()),
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
    /// handle. Waiting for the init lock ends after [`INIT_LOCK_TIMEOUT`],
    /// with [`Error::StateBusy`], or sooner if the future is dropped; the
    /// work of preparing the file, once this call holds the lock, runs to its
    /// end.
    async fn initialized(&self) -> Result<()> {
        if self.inner.initialized.initialized() {
            return Ok(());
        }
        // A file that is current is opened without the init lock. The work of
        // creating a file runs on a blocking task to its end, even if this
        // future is dropped meanwhile. Openers on this handle take turns at
        // it, and one behind another waits for that work as it waits for any
        // other that is running: the deadline is for waits that may not end,
        // such as a lock another process holds, and does not cut short work
        // that is under way.
        let opened = {
            let _working = self.inner.opening.lock().await;
            if self.inner.initialized.initialized() {
                return Ok(());
            }
            let inner = self.inner.clone();
            tokio::task::spawn_blocking(move || inner.open_unlocked())
                .await
                .map_err(|_| Error::Storage("State worker failed".into()))??
        };
        let connection = match opened {
            Opened::Ready(connection) => connection,
            Opened::Behind(connection) => {
                let held = self.inner.lock_init().await?;
                // Another opener on this handle may have prepared it while
                // this one waited for the lock.
                if self.inner.initialized.initialized() {
                    return Ok(());
                }
                let inner = self.inner.clone();
                tokio::task::spawn_blocking(move || inner.prepare(connection, held))
                    .await
                    .map_err(|_| Error::Storage("State worker failed".into()))??
            }
        };
        // The sync constructor may have opened it meanwhile: keep theirs.
        if self.inner.initialized.set(()).is_ok() {
            self.inner.idle.lock().push(connection);
        }
        Ok(())
    }
    /// Validate and migrate the file now, on the calling thread, so that an
    /// error in the state location surfaces to whoever builds on it and the
    /// first operation does not pay for creating the file. It has no
    /// deadline to follow, so it waits for another opener at most
    /// [`INIT_LOCK_TIMEOUT`] (five seconds) and then fails with
    /// [`Error::StateBusy`].
    pub(crate) fn open(&self) -> Result<()> {
        self.open_within(INIT_LOCK_TIMEOUT)
    }
    /// [`open`](Self::open), waiting at most `wait` for another opener.
    fn open_within(&self, wait: Duration) -> Result<()> {
        if self.inner.initialized.initialized() {
            return Ok(());
        }
        let connection = match self.inner.open_unlocked()? {
            Opened::Ready(connection) => connection,
            Opened::Behind(connection) => {
                let held = self.inner.lock_init_blocking(wait)?;
                self.inner.prepare(connection, held)?
            }
        };
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
    /// within five seconds, counted from when the file is open, this fails
    /// with [`Error::StateBusy`] and `f` never runs, and neither does it if the caller stops waiting first (the
    /// future is dropped); once `f` has started it runs to the end.
    pub(crate) async fn write<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        self.write_within(BUSY_TIMEOUT, f).await
    }
    /// [`write`](Self::write), waiting at most `wait` for the turn once the
    /// file is open.
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
            try_lock_file(file, Some(owner), Some(turn))
        })
        .await
        .map_err(|_| Error::Storage("State worker failed".into()))??;
        match attempt {
            Attempt::Held(held) => Ok(held),
            Attempt::Busy(waiting) => self.wait_for_lock(waiting, expiry, write_turn_busy).await,
        }
    }
    /// Wait on the handle's lock thread for a lock that is held elsewhere,
    /// until `expiry`, when the caller fails with `busy`. Dropping the
    /// future abandons the request.
    async fn wait_for_lock(
        &self,
        waiting: Waiting,
        expiry: tokio::time::Instant,
        busy: fn() -> Error,
    ) -> Result<HeldLock> {
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
            let request = LockRequest { waiting, reply };
            if locker.as_ref().is_some_and(|l| l.send(request).is_err()) {
                // The thread is gone: start another for the next write.
                *locker = None;
                return Err(stopped());
            }
        }
        tokio::time::timeout_at(expiry, granted)
            .await
            .map_err(|_| busy())?
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
    /// The init lock file, opened for this one use: a file shared by the
    /// handle's openers would be locked by whichever asked first, and then
    /// by the others too. Creates the state directory if need be.
    fn open_init_lock(path: &Path) -> Result<std::fs::File> {
        let parent = path
            .parent()
            .ok_or_else(|| Error::Config("State path requires a directory".into()))?;
        crate::security::ensure_private_directory(parent)?;
        Ok(crate::auth::token_store::open_lock_file(
            &path.with_extension("init.lock"),
        )?)
    }
    /// Take the init lock, trying where a blocking task runs. If another
    /// opener holds it, wait on the handle's lock thread for at most
    /// [`INIT_LOCK_TIMEOUT`]. Dropping the future abandons the request.
    async fn lock_init(&self) -> Result<HeldLock> {
        let path = self.path.clone();
        let attempt = tokio::task::spawn_blocking(move || {
            try_lock_file(Arc::new(Self::open_init_lock(&path)?), None, None)
        })
        .await
        .map_err(|_| Error::Storage("State worker failed".into()))??;
        match attempt {
            Attempt::Held(held) => Ok(held),
            Attempt::Busy(waiting) => {
                let expiry = tokio::time::Instant::now() + INIT_LOCK_TIMEOUT;
                self.wait_for_lock(waiting, expiry, init_busy).await
            }
        }
    }
    /// Take the init lock on the calling thread, polling for it for at most
    /// `wait`: a synchronous caller has no future to drop and no deadline to
    /// follow.
    fn lock_init_blocking(&self, wait: Duration) -> Result<HeldLock> {
        let file = Arc::new(Self::open_init_lock(&self.path)?);
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(HeldLock {
                        file,
                        owner: None,
                        _turn: None,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) if started.elapsed() < wait => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(init_busy()),
                Err(std::fs::TryLockError::Error(error)) => return Err(Error::Io(error)),
            }
        }
    }
    /// Open the state file, building it first if it does not exist, and say
    /// whether it is current. Takes no lock: a new file is moved into place
    /// whole or not at all (see the module documentation).
    fn open_unlocked(&self) -> Result<Opened> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| Error::Config("State path requires a directory".into()))?;
        crate::security::ensure_private_directory(parent)?;
        if !self.path.exists() {
            self.create_new_file(parent)?;
        }
        crate::security::verify_file_permissions(&self.path)?;
        let connection = open_connection(&self.path)?;
        remove_stale_builds(&self.path, STALE_BUILD_AGE);
        Ok(if self.is_current(&connection) {
            Opened::Ready(connection)
        } else {
            Opened::Behind(connection)
        })
    }
    /// Whether `connection`'s file is complete, in WAL mode and at the newest
    /// schema, with nothing left to import. Anything else, including a file
    /// that cannot be read, is for the locked path to judge: another opener
    /// of an older release may be part way through creating it.
    fn is_current(&self, connection: &Connection) -> bool {
        let current = || -> Result<bool> {
            if !table_exists(connection, "meta")?
                || schema_version(connection)? != Some(SCHEMA_VERSION)
                || !baseline_tables_present(connection)?
            {
                return Ok(false);
            }
            let mode: String = connection
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .map_err(sql)?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Ok(false);
            }
            let imported = meta::exists(connection, "legacy_imported")?;
            Ok(imported || !self.legacy.as_deref().is_some_and(Path::exists))
        };
        current().unwrap_or(false)
    }
    /// Build a new state file and move it into place, unless another opener
    /// moves theirs first, in which case theirs stands and this one's copy is
    /// discarded.
    fn create_new_file(&self, parent: &Path) -> Result<()> {
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        let temp = self.path.with_file_name(name);
        let created = (|| {
            // As private as the state file, from the moment it exists.
            let file = reserve_backup_file(&temp)?;
            failpoint("reserved");
            build_database(&temp, self.legacy.as_deref())?;
            failpoint("built");
            // The one flush: everything in the file is on disk before it has
            // the state file's name.
            file.sync_all()?;
            failpoint("flushed");
            match crate::security::rename_noclobber(&temp, &self.path) {
                Ok(()) => {}
                // Lost the race: the winner's file is complete.
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
                Err(error) => return Err(error.into()),
            }
            failpoint("moved");
            // The new name is on disk before any write to the file is.
            crate::security::sync_directory(parent)?;
            #[cfg(windows)]
            file.sync_all()?;
            failpoint("published");
            Ok(())
        })();
        discard_build(&temp);
        created
    }
    /// Prepare an existing file that is behind, under the init lock, which is
    /// released when this returns, so that two openers cannot race the
    /// migration.
    fn prepare(&self, mut connection: Connection, held: HeldLock) -> Result<Connection> {
        connection
            .execute_batch("PRAGMA journal_mode=WAL")
            .map_err(sql)?;
        validate_existing(&connection)?;
        migrate(&mut connection)?;
        if let Some(legacy) = self.legacy.as_deref().filter(|p| p.exists()) {
            import_legacy(&mut connection, legacy)?;
        }
        drop(held);
        Ok(connection)
    }
}
/// The file a database's SQLite sidecar of `suffix` (`-wal`, `-shm`,
/// `-journal`) is named after `path`.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}
/// Build the complete database at `path`, an empty file made for it, and leave
/// it closed, in WAL mode and with no file beside it.
///
/// Nothing here has to survive a crash: the file is not the state file until
/// it is moved, and a crash leaves it a stray to remove. So the schema goes in
/// with no rollback journal on disk and no flush, and one flush by the caller
/// makes it durable. The file is switched to WAL mode last, which SQLite
/// records in the file's header, so that the first connection to open the
/// state file finds it ready and does not change its mode, and a closed
/// database has no write-ahead log or shared-memory file.
fn build_database(path: &Path, legacy: Option<&Path>) -> Result<()> {
    let mut connection = Connection::open(path).map_err(sql)?;
    connection
        .execute_batch("PRAGMA synchronous=OFF; PRAGMA journal_mode=MEMORY")
        .map_err(sql)?;
    migrate(&mut connection)?;
    if let Some(legacy) = legacy.filter(|p| p.exists()) {
        import_legacy(&mut connection, legacy)?;
    }
    let mode: String = connection
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .map_err(sql)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::Storage(
            "A new state file would not switch to WAL mode".into(),
        ));
    }
    // The last connection to close checkpoints the log, which has nothing in
    // it, and deletes it and the shared-memory file.
    connection.close().map_err(|(_, error)| sql(error))?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let beside = sidecar(path, suffix);
        let Ok(metadata) = std::fs::metadata(&beside) else {
            continue;
        };
        // Moving the database without a log that holds anything would lose
        // that, and a journal that exists is a hot one: neither is ours to
        // delete. An empty log and the shared-memory index hold nothing.
        if metadata.len() != 0 && suffix != "-shm" {
            return Err(Error::Storage(format!(
                "A new state file was left with a {suffix} file"
            )));
        }
        std::fs::remove_file(&beside)?;
    }
    Ok(())
}
/// Remove a build that was not moved into place, and whatever SQLite left
/// beside it. Best effort: a stray is removed later (see
/// [`remove_stale_builds`]).
fn discard_build(path: &Path) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(sidecar(path, suffix));
    }
}
/// Remove the temporary files that a crash while building the state file at
/// `path` left beside it, and their SQLite files, once they are `older_than`.
/// A build that is still running is younger than that. Best effort.
fn remove_stale_builds(path: &Path, older_than: Duration) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(rest) = file_name
            .to_str()
            .and_then(|f| f.strip_prefix(name))
            .and_then(|f| f.strip_prefix('.'))
        else {
            continue;
        };
        let build = ["", "-wal", "-shm", "-journal"].iter().any(|suffix| {
            rest.strip_suffix(suffix)
                .and_then(|r| r.strip_suffix(".tmp"))
                .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        });
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= older_than);
        if build && old {
            let _ = std::fs::remove_file(entry.path());
        }
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

    /// A handle on a state file that an earlier release left one migration
    /// behind, so that opening it must take the init lock.
    fn one_release_behind(dir: &Path, name: &str) -> StateDatabase {
        let first = StateDatabase::new(dir.join(name));
        first.open().unwrap();
        Connection::open(first.path())
            .unwrap()
            .execute_batch(
                "DROP TABLE activity;
                 DELETE FROM schema_migrations WHERE version=6;
                 UPDATE meta SET value='5' WHERE key='schema'",
            )
            .unwrap();
        StateDatabase::new(first.path().to_owned())
    }

    /// Another opener in the middle of preparing the file: the init lock,
    /// locked from a handle of its own.
    fn hold_init_lock(database: &StateDatabase) -> std::fs::File {
        let held = Inner::open_init_lock(database.path()).unwrap();
        held.lock().unwrap();
        held
    }

    /// Unlock `held` after `delay`, from a thread of its own.
    fn release_after(held: std::fs::File, delay: Duration) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            held.unlock().unwrap();
        })
    }

    /// Once the init lock is free again, nothing holds it: the request of a
    /// caller that went away was granted the lock, found nobody waiting and
    /// gave it back, so another can take it, and the handle's lock thread is
    /// free for the next request.
    async fn assert_lock_given_back(database: &StateDatabase) {
        let probe = Inner::open_init_lock(database.path()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match probe.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("the lock was never given back: {error:?}"),
            }
        }
        probe.unlock().unwrap();
        database.write(|c| insert(c, "next")).await.unwrap();
    }

    /// An async caller sets its own deadline by dropping the future (here,
    /// `tokio::time::timeout`): while another opener keeps the file, the call
    /// ends at that deadline and not at the five seconds of the wait itself,
    /// the write never runs, and no lock or waiting thread is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_caller_deadline_ends_the_wait_for_a_busy_opener() {
        let dir = tempfile::tempdir().unwrap();
        let database = one_release_behind(dir.path(), "deadline.json");
        let held = hold_init_lock(&database);
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let started = std::time::Instant::now();
        let waited = tokio::time::timeout(
            Duration::from_millis(300),
            database.write(move |c| {
                flag.store(true, Ordering::Relaxed);
                insert(c, "early")
            }),
        )
        .await;
        let elapsed = started.elapsed();
        assert!(waited.is_err(), "the lock was held: {waited:?}");
        assert!(elapsed >= Duration::from_millis(290), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        held.unlock().unwrap();
        assert_lock_given_back(&database).await;
        assert!(!ran.load(Ordering::Relaxed));
        assert!(!database.run(|c| meta::exists(c, "early")).await.unwrap());
    }

    /// A write waits for a busy opener and goes ahead once it lets go.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_waits_for_a_busy_opener_that_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let database = one_release_behind(dir.path(), "patient.json");
        let released = release_after(hold_init_lock(&database), Duration::from_millis(300));
        let started = std::time::Instant::now();
        // A caller's deadline that outlasts the other opener.
        tokio::time::timeout(
            Duration::from_secs(30),
            database.write(|c| insert(c, "patient")),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(290));
        released.join().unwrap();
        assert!(database.run(|c| meta::exists(c, "patient")).await.unwrap());
    }

    /// The turn to write has its own wait, counted from when the file is
    /// open: a write that had to wait for another opener still gets all of
    /// it. Here opening takes 1 s of a 2 s wait and the turn comes at 2.5 s,
    /// which is after the wait would have run out had it started with the
    /// call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_slow_opening_does_not_shorten_the_wait_for_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let database = one_release_behind(dir.path(), "slow.json");
        let opener = release_after(hold_init_lock(&database), Duration::from_secs(1));
        let writer = release_after(hold_write_lock(&database), Duration::from_millis(2500));
        let started = std::time::Instant::now();
        database
            .write_within(Duration::from_secs(2), |c| insert(c, "slow"))
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(2400));
        opener.join().unwrap();
        writer.join().unwrap();
        assert!(database.run(|c| meta::exists(c, "slow")).await.unwrap());
    }

    /// Openers on one handle that wait for the init lock each follow their own
    /// caller: one that stops waiting does not hold up the one in front.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn openers_on_one_handle_each_end_their_wait_with_their_caller() {
        let dir = tempfile::tempdir().unwrap();
        let database = one_release_behind(dir.path(), "queue.json");
        let held = hold_init_lock(&database);
        let patient = {
            let database = database.clone();
            tokio::spawn(async move { database.write(|c| insert(c, "patient")).await })
        };
        // The patient opener is waiting for the lock by now; the impatient
        // one is behind it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = std::time::Instant::now();
        let waited = tokio::time::timeout(
            Duration::from_millis(300),
            database.write(|c| insert(c, "impatient")),
        )
        .await;
        assert!(waited.is_err(), "{waited:?}");
        assert!(started.elapsed() < Duration::from_secs(3));
        held.unlock().unwrap();
        patient.await.unwrap().unwrap();
        assert!(
            !database
                .run(|c| meta::exists(c, "impatient"))
                .await
                .unwrap()
        );
    }

    /// The constructor has no future's deadline to follow, so it keeps a
    /// bound of its own: `open_within` takes it as an argument.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_constructor_waits_for_another_opener_within_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let database = one_release_behind(dir.path(), "constructor.json");
        let held = hold_init_lock(&database);
        let started = std::time::Instant::now();
        let refused = database
            .open_within(Duration::from_millis(300))
            .unwrap_err();
        assert!(
            matches!(&refused, Error::StateBusy(m) if m.contains("initialization")),
            "{refused:?}"
        );
        assert!(started.elapsed() >= Duration::from_millis(290));
        assert!(started.elapsed() < Duration::from_secs(3));
        held.unlock().unwrap();
        let released = release_after(hold_init_lock(&database), Duration::from_millis(300));
        database.open_within(Duration::from_secs(20)).unwrap();
        released.join().unwrap();
    }

    /// The bounds that `open` and `run` have, with no deadline of their own,
    /// are the five seconds the opener has always had: no wait gets longer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn open_and_run_give_up_on_a_busy_opener_after_five_seconds() {
        assert_eq!(INIT_LOCK_TIMEOUT, Duration::from_secs(5));
        let dir = tempfile::tempdir().unwrap();
        let database = one_release_behind(dir.path(), "bound.json");
        let held = hold_init_lock(&database);
        let constructor = {
            let database = database.clone();
            tokio::task::spawn_blocking(move || {
                let started = std::time::Instant::now();
                (database.open(), started.elapsed())
            })
        };
        let started = std::time::Instant::now();
        let refused = database.run(|_| Ok(())).await.unwrap_err();
        let waited = started.elapsed();
        assert!(matches!(refused, Error::StateBusy(_)), "{refused:?}");
        assert!(waited >= Duration::from_millis(4900), "{waited:?}");
        assert!(waited < Duration::from_secs(8), "{waited:?}");
        let (opened, waited) = constructor.await.unwrap();
        assert!(matches!(opened, Err(Error::StateBusy(_))), "{opened:?}");
        assert!(waited >= Duration::from_millis(4900), "{waited:?}");
        assert!(waited < Duration::from_secs(8), "{waited:?}");
        held.unlock().unwrap();
    }

    /// A caller that stops waiting for the file takes its request with it: no
    /// thread keeps waiting for the lock once it is free, and nothing holds
    /// it afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_opener_whose_caller_went_away_leaves_no_lock_held() {
        let dir = tempfile::tempdir().unwrap();
        let database = one_release_behind(dir.path(), "abandoned.json");
        let held = hold_init_lock(&database);
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        // Dropped by its own timeout.
        let waited = tokio::time::timeout(
            Duration::from_millis(200),
            database.write(move |c| {
                flag.store(true, Ordering::Relaxed);
                insert(c, "timed-out")
            }),
        )
        .await;
        assert!(waited.is_err(), "the lock was held: {waited:?}");
        // Aborted.
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
        assert_lock_given_back(&database).await;
        assert!(!ran.load(Ordering::Relaxed));
        for key in ["timed-out", "aborted"] {
            let present = database.run(move |c| meta::exists(c, key)).await.unwrap();
            assert!(!present, "{key}");
        }
    }

    /// `path` is a complete state file, closed: in WAL mode by its header,
    /// intact, at the newest schema, with every baseline table.
    fn assert_complete(path: &Path) {
        let header = std::fs::read(path).unwrap();
        assert_eq!(header[18..20], [2, 2], "not in WAL mode: {path:?}");
        let connection = Connection::open(path).unwrap();
        integrity_check(&connection).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), Some(SCHEMA_VERSION));
        assert!(baseline_tables_present(&connection).unwrap());
        let applied: i64 = connection
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(applied, MIGRATIONS.len() as i64);
    }

    /// Files in `dir` that a build leaves, or SQLite leaves beside the state
    /// file or a build.
    fn leftovers(dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp") || n.ends_with("-journal"))
            .collect()
    }

    /// Every flush to disk costs, and on a slow disk a new file's migrations
    /// took seconds: the file is built whole, in one transaction, in a private
    /// file, and moved into place with one flush. It is closed, with no log
    /// beside it, and its first opener finds it ready and writes nothing.
    #[tokio::test]
    async fn a_new_state_file_is_built_whole_and_opens_without_a_write() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("fresh.json"));
        let state = database.path().to_owned();
        let parent = state.parent().unwrap();
        crate::security::ensure_private_directory(parent).unwrap();
        database.inner.create_new_file(parent).unwrap();
        assert_complete(&state);
        crate::security::verify_file_permissions(&state).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["state.sqlite3"]);
        // Opening it needs no init lock and changes nothing in the file.
        let held = hold_init_lock(&database);
        let before = std::fs::read(&state).unwrap();
        database.run(|_| Ok(())).await.unwrap();
        assert_eq!(std::fs::read(&state).unwrap(), before);
        let log = std::fs::metadata(state.with_extension("sqlite3-wal"));
        assert!(
            log.is_ok_and(|log| log.len() == 0) || !state.with_extension("sqlite3-wal").exists()
        );
        held.unlock().unwrap();
    }

    /// A new file is created whole, or not at all: the opener that loses the
    /// race leaves the winner's file as it was and removes its own.
    #[test]
    fn an_opener_that_loses_the_race_to_create_leaves_the_winners_file() {
        let dir = tempfile::tempdir().unwrap();
        let database = StateDatabase::new(dir.path().join("race.json"));
        let state = database.path().to_owned();
        let parent = state.parent().unwrap();
        crate::security::ensure_private_directory(parent).unwrap();
        database.inner.create_new_file(parent).unwrap();
        let before = std::fs::read(&state).unwrap();
        database.inner.create_new_file(parent).unwrap();
        assert_eq!(std::fs::read(&state).unwrap(), before);
        assert!(leftovers(parent).is_empty(), "{:?}", leftovers(parent));
    }

    /// Openers that create one file at the same moment, in separate handles
    /// as separate processes would have, all end with the one complete file.
    #[test]
    fn openers_racing_to_create_a_state_file_share_one_complete_file() {
        for round in 0..10 {
            let dir = tempfile::tempdir().unwrap();
            let state = dir.path().join("state").join("state.sqlite3");
            let start = Arc::new(std::sync::Barrier::new(8));
            let openers: Vec<_> = (0..8)
                .map(|_| {
                    let (state, start) = (state.clone(), start.clone());
                    std::thread::spawn(move || {
                        let database = StateDatabase::new(state);
                        start.wait();
                        database.open().map(|()| database)
                    })
                })
                .collect();
            let databases: Vec<_> = openers
                .into_iter()
                .map(|o| {
                    o.join()
                        .unwrap()
                        .unwrap_or_else(|e| panic!("round {round}: {e}"))
                })
                .collect();
            drop(databases);
            assert_complete(&state);
            let parent = state.parent().unwrap();
            assert!(leftovers(parent).is_empty(), "{:?}", leftovers(parent));
        }
    }

    /// A new file is not prepared under the init lock, and neither is a
    /// current one: another opener holding it does not keep them waiting.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_a_file_that_is_behind_waits_for_the_init_lock() {
        let dir = tempfile::tempdir().unwrap();
        let new = StateDatabase::new(dir.path().join("new.json"));
        let held = hold_init_lock(&new);
        new.write_within(Duration::from_millis(500), |c| insert(c, "a"))
            .await
            .unwrap();
        held.unlock().unwrap();
        let current = StateDatabase::new(new.path().to_owned());
        let held = hold_init_lock(&current);
        current
            .write_within(Duration::from_millis(500), |c| insert(c, "b"))
            .await
            .unwrap();
        held.unlock().unwrap();
        let behind = one_release_behind(dir.path(), "behind.json");
        let held = hold_init_lock(&behind);
        let waited =
            tokio::time::timeout(Duration::from_millis(300), behind.write(|c| insert(c, "c")))
                .await;
        assert!(waited.is_err(), "{waited:?}");
        held.unlock().unwrap();
    }

    /// The temporary files a crashed build leaves are removed once they are
    /// old enough to have no owner, together with SQLite's files beside them,
    /// and nothing else is.
    #[test]
    fn stale_builds_are_removed_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state.sqlite3");
        let id = uuid::Uuid::new_v4().simple().to_string();
        let build = format!("state.sqlite3.{id}.tmp");
        let ours = [
            build.clone(),
            format!("{build}-wal"),
            format!("{build}-shm"),
            format!("{build}-journal"),
        ];
        let theirs = [
            "state.sqlite3".to_owned(),
            "state.sqlite3-wal".to_owned(),
            "state.sqlite3-shm".to_owned(),
            "state.write.lock".to_owned(),
            "state.init.lock".to_owned(),
            "state.sqlite3.backup".to_owned(),
            format!("other.sqlite3.{id}.tmp"),
            "state.sqlite3.notahexid.tmp".to_owned(),
        ];
        for name in ours.iter().chain(&theirs) {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        // Too young to be anybody's stray: a build may be running.
        remove_stale_builds(&state, STALE_BUILD_AGE);
        assert!(ours.iter().all(|n| dir.path().join(n).exists()));
        remove_stale_builds(&state, Duration::ZERO);
        assert!(ours.iter().all(|n| !dir.path().join(n).exists()));
        assert!(theirs.iter().all(|n| dir.path().join(n).exists()));
    }

    /// A child process for the crash tests: it opens a new state file in the
    /// directory it is given, and the parent stops it. It does nothing when
    /// run as one of the unit tests.
    #[test]
    fn crash_test_child() {
        if let Some(dir) = std::env::var_os("RUSTYSCHWAB_TEST_CRASH_DIR") {
            StateDatabase::new(PathBuf::from(dir).join("state.sqlite3"))
                .open()
                .unwrap();
        }
    }

    fn child(dir: &Path) -> std::process::Command {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "storage::tests::crash_test_child",
                "--test-threads=1",
            ])
            .env("RUSTYSCHWAB_TEST_CRASH_DIR", dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        child
    }

    /// What a state location holds after a crash: no state file, or a
    /// complete one, and a later opener gets a working file either way.
    fn assert_recovers(dir: &Path) {
        let state = dir.join("state.sqlite3");
        if state.exists() {
            assert_complete(&state);
        }
        let database = StateDatabase::new(state.clone());
        database.open().unwrap();
        assert_complete(&state);
        remove_stale_builds(&state, Duration::ZERO);
        assert!(leftovers(dir).is_empty(), "{:?}", leftovers(dir));
    }

    /// A process that dies while it builds the state file leaves no state
    /// file: only the temporary one, which the next opener removes once it is
    /// old. One that dies after the move leaves the complete file.
    #[test]
    fn a_crash_before_the_move_leaves_no_state_file() {
        for point in ["reserved", "built", "flushed"] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("state");
            let status = child(&dir)
                .env("RUSTYSCHWAB_TEST_CRASH_AT", point)
                .status()
                .unwrap();
            assert!(!status.success(), "{point}: the child did not crash");
            assert!(!dir.join("state.sqlite3").exists(), "{point}");
            assert_eq!(leftovers(&dir).len(), 1, "{point}");
            assert_recovers(&dir);
        }
    }

    #[test]
    fn a_crash_after_the_move_leaves_the_complete_file() {
        for point in ["moved", "published"] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("state");
            let status = child(&dir)
                .env("RUSTYSCHWAB_TEST_CRASH_AT", point)
                .status()
                .unwrap();
            assert!(!status.success(), "{point}: the child did not crash");
            assert!(leftovers(&dir).is_empty(), "{point}");
            assert_complete(&dir.join("state.sqlite3"));
            assert_recovers(&dir);
        }
    }

    /// Killed at any instant, not only the ones named: the same holds.
    #[test]
    fn a_process_killed_at_any_instant_leaves_no_file_or_a_complete_one() {
        for delay in (0..160).step_by(5) {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("state");
            let mut child = child(&dir).spawn().unwrap();
            std::thread::sleep(Duration::from_millis(delay));
            let _ = child.kill();
            let _ = child.wait();
            assert_recovers(&dir);
        }
    }
}
