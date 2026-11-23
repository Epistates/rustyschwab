//! Temporary: timer resolution, commit latency, and how write-lock
//! strategies behave with two handles and eight writers on one file.
use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn summary(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    let pick = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!(
        "n={} min={:.2} p50={:.2} p90={:.2} max={:.2}",
        v.len(),
        v[0],
        pick(0.5),
        pick(0.9),
        v[v.len() - 1]
    )
}

#[test]
fn probe_timers_and_commits() {
    let mut vfs = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        #[allow(unsafe_code)]
        // SAFETY: no preconditions.
        unsafe {
            rusqlite::ffi::sqlite3_sleep(1);
        }
        vfs.push(ms(t.elapsed()));
    }
    let mut std_sleep = Vec::new();
    for _ in 0..30 {
        let t = Instant::now();
        std::thread::sleep(Duration::from_millis(1));
        std_sleep.push(ms(t.elapsed()));
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let tokio_sleep = runtime.block_on(async {
        let mut v = Vec::new();
        for _ in 0..30 {
            let t = Instant::now();
            tokio::time::sleep(Duration::from_millis(1)).await;
            v.push(ms(t.elapsed()));
        }
        v
    });
    let dir = tempfile::tempdir().unwrap();
    let commits = |sync: &str| {
        let c = Connection::open(dir.path().join(format!("{sync}.sqlite3"))).unwrap();
        c.execute_batch(&format!(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous={sync}; CREATE TABLE t(x)"
        ))
        .unwrap();
        let mut v = Vec::new();
        for n in 0..30 {
            let t = Instant::now();
            c.execute("INSERT INTO t VALUES(?1)", [n]).unwrap();
            v.push(ms(t.elapsed()));
        }
        v
    };
    println!("DIAGPROBE sqlite3_sleep(1) ms: {}", summary(vfs));
    println!("DIAGPROBE std sleep(1ms) ms: {}", summary(std_sleep));
    println!("DIAGPROBE tokio sleep(1ms) ms: {}", summary(tokio_sleep));
    println!("DIAGPROBE commit FULL ms: {}", summary(commits("FULL")));
    println!("DIAGPROBE commit NORMAL ms: {}", summary(commits("NORMAL")));
}

#[derive(Clone, Copy, Debug)]
enum Busy {
    /// `busy_timeout(5 s)`: SQLite's backoff table.
    Default,
    /// Retry every millisecond until 5 s of wall clock have passed.
    Fixed,
}

#[derive(Clone, Copy, Debug)]
struct Strategy {
    name: &'static str,
    busy: Busy,
    gate: bool,
    lock_file: bool,
}

struct Handle {
    path: PathBuf,
    busy: Busy,
    pool: parking_lot::Mutex<Vec<Connection>>,
    permits: Arc<tokio::sync::Semaphore>,
    gate: Option<tokio::sync::Mutex<()>>,
    lock: Option<std::fs::File>,
}

fn open(path: &Path, busy: Busy) -> Connection {
    let mut c = Connection::open(path).unwrap();
    match busy {
        Busy::Default => c.busy_timeout(Duration::from_secs(5)).unwrap(),
        Busy::Fixed => {
            let mut since = Instant::now();
            c.busy_handler(Some(move |count: i32| {
                if count == 0 {
                    since = Instant::now();
                }
                if since.elapsed() >= Duration::from_secs(5) {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(1));
                true
            }))
            .unwrap();
        }
    }
    c.execute_batch("PRAGMA synchronous=FULL;").unwrap();
    c
}

async fn write(handle: Arc<Handle>, subject: String) -> Result<Duration, String> {
    let start = Instant::now();
    let _gate = match &handle.gate {
        Some(gate) => Some(gate.lock().await),
        None => None,
    };
    let permit = handle.permits.clone().acquire_owned().await.unwrap();
    let h = handle.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let pooled = h.pool.lock().pop();
        let conn = pooled.unwrap_or_else(|| open(&h.path, h.busy));
        if let Some(file) = &h.lock {
            file.lock().unwrap();
        }
        let result = (|| {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            let prev: i64 = conn.query_row("SELECT count(*) FROM t", [], |r| r.get(0))?;
            conn.execute(
                "INSERT INTO t(subject, prev) VALUES(?1, ?2)",
                rusqlite::params![subject, prev],
            )?;
            conn.execute_batch("COMMIT")
        })();
        if result.is_err() && !conn.is_autocommit() {
            let _ = conn.execute_batch("ROLLBACK");
        }
        if let Some(file) = &h.lock {
            file.unlock().unwrap();
        }
        h.pool.lock().push(conn);
        result.map_err(|e| e.to_string())
    })
    .await
    .unwrap()?;
    Ok(start.elapsed())
}

/// Background commits on another file, as sibling tests make.
fn noise(dir: &Path, stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    (0..2)
        .map(|n| {
            let path = dir.join(format!("noise{n}.sqlite3"));
            let stop = stop.clone();
            std::thread::spawn(move || {
                let c = Connection::open(path).unwrap();
                c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE t(x)")
                    .unwrap();
                while !stop.load(Ordering::Relaxed) {
                    c.execute("INSERT INTO t VALUES(1)", []).unwrap();
                }
            })
        })
        .collect()
}

async fn round(strategy: Strategy, with_noise: bool) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.sqlite3");
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, subject TEXT, prev INTEGER)",
        )
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let noisy = if with_noise {
        noise(dir.path(), stop.clone())
    } else {
        Vec::new()
    };
    let handles: Vec<Arc<Handle>> = (0..2)
        .map(|_| {
            Arc::new(Handle {
                path: path.clone(),
                busy: strategy.busy,
                pool: parking_lot::Mutex::new(Vec::new()),
                permits: Arc::new(tokio::sync::Semaphore::new(4)),
                gate: strategy.gate.then(|| tokio::sync::Mutex::new(())),
                lock: strategy.lock_file.then(|| {
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(dir.path().join("write.lock"))
                        .unwrap()
                }),
            })
        })
        .collect();
    let started = Instant::now();
    let tasks: Vec<_> = (0..8)
        .map(|task| {
            let handle = handles[task % 2].clone();
            tokio::spawn(async move {
                let mut waits = Vec::new();
                let mut errors = 0;
                for n in 0..20 {
                    match write(handle.clone(), format!("task{task}/{n}")).await {
                        Ok(wait) => waits.push(ms(wait)),
                        Err(_) => errors += 1,
                    }
                }
                (waits, errors)
            })
        })
        .collect();
    let mut waits = Vec::new();
    let mut errors = 0;
    for task in tasks {
        let (w, e) = task.await.unwrap();
        waits.extend(w);
        errors += e;
    }
    let total = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    for thread in noisy {
        thread.join().unwrap();
    }
    format!(
        "total={:.0}ms errors={errors} per-write {}",
        ms(total),
        if waits.is_empty() {
            "none".to_owned()
        } else {
            summary(waits)
        }
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strategies_under_contention() {
    let strategies = [
        Strategy {
            name: "default",
            busy: Busy::Default,
            gate: false,
            lock_file: false,
        },
        Strategy {
            name: "fixed-1ms",
            busy: Busy::Fixed,
            gate: false,
            lock_file: false,
        },
        Strategy {
            name: "gate+default",
            busy: Busy::Default,
            gate: true,
            lock_file: false,
        },
        Strategy {
            name: "gate+fixed-1ms",
            busy: Busy::Fixed,
            gate: true,
            lock_file: false,
        },
        Strategy {
            name: "gate+lockfile+fixed-1ms",
            busy: Busy::Fixed,
            gate: true,
            lock_file: true,
        },
    ];
    for with_noise in [false, true] {
        for strategy in strategies {
            for n in 0..3 {
                let line = round(strategy, with_noise).await;
                println!(
                    "DIAGSTRAT noise={with_noise} {:<24} round {n}: {line}",
                    strategy.name
                );
            }
        }
    }
}
