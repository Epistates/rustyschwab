"""Temporary: run the timing-sensitive tests in loops and summarise the
JSON-lines diagnostics the instrumented build writes (RUSTYSCHWAB_DIAG)."""

import json
import os
import pathlib
import re
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "diag-out"
OUT.mkdir(exist_ok=True)
FLAGS = ["--workspace", "--all-features", "--locked"]


def cargo_test(test, args=(), diag=None):
    cmd = ["cargo", "test", *FLAGS, "--test", test, "--", *args]
    env = dict(os.environ)
    if diag is not None:
        env["RUSTYSCHWAB_DIAG"] = str(diag)
        if diag.exists():
            diag.unlink()
    started = time.monotonic()
    proc = subprocess.run(
        cmd, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", cwd=ROOT
    )
    return proc.returncode, time.monotonic() - started, proc.stdout + proc.stderr


def load(path):
    if not path.exists():
        return []
    events = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if line:
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return events


def pct(values, q):
    values = sorted(values)
    return values[min(len(values) - 1, round((len(values) - 1) * q))]


def stats(values):
    values = list(values)
    if not values:
        return "n=0"
    return (
        f"n={len(values)} p50={pct(values, 0.5):.1f} p90={pct(values, 0.9):.1f} "
        f"p99={pct(values, 0.99):.1f} max={max(values):.1f}"
    )


def failures(output):
    return [l for l in output.splitlines() if "FAILED" in l or "panicked" in l or "Err` value" in l][:12]


def analyze_audit(events):
    tasks = [e for e in events if e["ev"] == "audit" and re.fullmatch(r"task\d", e["subject"])]
    if not tasks:
        return ["no concurrent-test audit events"]
    handles = sorted({e["handle"] for e in tasks})
    ok = [e for e in tasks if e["outcome"] == "ok"]
    bad = [e for e in tasks if e["outcome"] != "ok"]
    t_start = min(e["t0"] for e in tasks)
    # The WAL hook runs right after the commit releases the write lock, so
    # its timestamp is the release; `t3` also covers the hook itself.
    hooks = {}
    for e in events:
        if e["ev"] == "wal":
            hooks.setdefault((e["handle"], e["conn"]), []).append(e["t"])
    for e in ok:
        released = [t for t in hooks.get((e["handle"], e["conn"]), []) if e["t2"] <= t <= e["t3"]]
        e["t3"] = min(released) if released else e["t3"]
    lines = [
        f"events={len(tasks)} ok={len(ok)} failed={len(bad)} handles={handles} "
        f"connections={len({(e['handle'], e['conn']) for e in tasks})}"
    ]
    wait = [(e["t1"] - e["t0"]) / 1000 for e in tasks]
    commit = [(e["t3"] - e["t2"]) / 1000 for e in ok]
    hold = [(e["t3"] - e["t1"]) / 1000 for e in ok]
    span = (max(e["t3"] for e in tasks) - t_start) / 1000
    lines.append(f"begin (lock) wait ms: {stats(wait)}")
    lines.append(f"commit ms: {stats(commit)}")
    lines.append(f"lock hold ms: {stats(hold)}; span {span:.0f} ms; lock held {sum(hold) / span * 100:.0f}% of span")
    polls = [e["polls"] for e in tasks]
    ratio = [e["slept_us"] / 1000 / e["nominal_ms"] for e in tasks if e["nominal_ms"] > 0]
    lines.append(f"busy-handler polls per wait: {stats(polls)}; actual/nominal sleep: {stats(ratio)}")
    acquired = sorted(ok, key=lambda e: e["t1"])
    same_task = same_handle = 0
    gaps = []
    runs = []
    run = 1
    for a, b in zip(acquired, acquired[1:]):
        gaps.append((b["t1"] - a["t3"]) / 1000)
        if a["subject"] == b["subject"]:
            same_task += 1
            run += 1
        else:
            runs.append(run)
            run = 1
        if a["handle"] == b["handle"]:
            same_handle += 1
    runs.append(run)
    lines.append(
        f"hand-offs={len(acquired) - 1}: next holder is the same task {same_task}, "
        f"same handle {same_handle}; release-to-acquire gap ms {stats(gaps)}; "
        f"consecutive acquisitions by one task {stats(runs)}"
    )
    per_task = {}
    for e in acquired:
        per_task.setdefault(e["subject"], []).append(e)
    lines.append(
        "per task first-acquire/last-commit/longest-wait ms: "
        + ", ".join(
            f"{s}:{(v[0]['t1'] - t_start) / 1000:.0f}/{(v[-1]['t3'] - t_start) / 1000:.0f}/"
            f"{max((x['t1'] - x['t0']) / 1000 for x in tasks if x['subject'] == s):.0f}"
            for s, v in sorted(per_task.items())
        )
    )
    for e in bad:
        during = [o for o in ok if e["t0"] <= o["t1"] <= e["t1"]]
        held = sum(
            min(o["t3"], e["t1"]) - max(o["t1"], e["t0"])
            for o in ok
            if o["t1"] < e["t1"] and o["t3"] > e["t0"]
        ) / max(1, e["t1"] - e["t0"])
        lines.append(
            f"FAILED {e['subject']} handle {e['handle']} conn {e['conn']} at {(e['t0'] - t_start) / 1000:.0f} ms: "
            f"waited {(e['t1'] - e['t0']) / 1000:.0f} ms, polls {e['polls']}, nominal sleep {e['nominal_ms']} ms, "
            f"actual sleep {e['slept_us'] / 1000:.0f} ms, gave_up={e['gave_up']}; "
            f"{len(during)} acquisitions by others meanwhile "
            f"({sum(1 for o in during if o['handle'] == e['handle'])} on its own handle); "
            f"lock held {held * 100:.0f}% of its wait; {e['outcome']}"
        )
    wal = [e for e in events if e["ev"] == "wal" and e["handle"] in handles]
    checkpoints = [e for e in wal if e["ckpt_ok"] is not None]
    lines.append(
        f"WAL: commits={len(wal)} largest WAL frames={max((e['pages'] for e in wal), default=0)} "
        f"auto-checkpoints={len(checkpoints)} {stats(e['ckpt_us'] / 1000 for e in checkpoints) if checkpoints else ''}"
    )
    runs_ev = [e for e in events if e["ev"] == "run" and e["handle"] in handles]
    lines.append(
        f"pool permit wait ms {stats((e['t_permit'] - e['t_init']) / 1000 for e in runs_ev)}; "
        f"blocking thread start ms {stats((e['t_start'] - e['t_permit']) / 1000 for e in runs_ev)}"
    )
    inits = [e for e in events if e["ev"] == "init" and e["handle"] in handles]
    lines.append(
        "initialisation: "
        + ", ".join(
            f"handle {e['handle']} lock wait {(e['t_locked'] - e['t_start']) / 1000:.0f} ms, "
            f"total {(e['t_end'] - e['t_start']) / 1000:.0f} ms"
            for e in inits
        )
    )
    others = [e for e in events if e["ev"] == "audit" and e["handle"] not in handles and e["outcome"] == "ok"]
    lines.append(f"other tests' audit commits (sibling disk load) ms: {stats((e['t3'] - e['t2']) / 1000 for e in others)}")
    return lines


def analyze_requests(events, deadline_ms=80):
    lines = []
    starts = [e for e in events if e["ev"] == "req_start" and e["deadline_ms"] == deadline_ms]
    for s in starts:
        done = next((d for d in events if d["ev"] == "req_done" and d["t0"] == s["t0"]), None)
        end = done["t1"] if done else float("inf")
        within = lambda e: s["t_deadline"] <= e["t0"] <= end  # noqa: E731
        admits = [a for a in events if a["ev"] == "req_admit" and a["deadline_ms"] == deadline_ms and within(a)]
        calls = [c for c in events if c["ev"] == "req_call" and c["deadline_ms"] == deadline_ms and within(c)]
        budget = [a for a in events if a["ev"] == "admit" and within(a)]
        lines.append(
            f"request {s['op']}: live state {(s['t_live'] - s['t0']) / 1000:.1f} ms, first token "
            f"{(s['t_deadline'] - s['t_live']) / 1000:.1f} ms (both before the deadline); inside the deadline: "
            + "; ".join(f"admit {(a['t1'] - a['t0']) / 1000:.1f} ms" for a in admits)
            + " | "
            + "; ".join(
                f"in-process budget {(b['t_acquired'] - b['t0']) / 1000:.1f} ms, durable quota "
                f"{(b['t_quota'] - b['t_acquired']) / 1000:.1f} ms (wait {b['wait_ms']} ms)"
                for b in budget
            )
            + " | "
            + "; ".join(f"call {(c['t1'] - c['t0']) / 1000:.1f} ms -> {c['result']}" for c in calls)
            + (f" | total after deadline start {(done['t1'] - s['t_deadline']) / 1000:.1f} ms -> {done['result']}" if done else " | no end")
        )
    commits = [(e["t3"] - e["t2"]) / 1000 for e in events if e["ev"] == "audit" and e["outcome"] == "ok"]
    lines.append(f"all audit commits in this run ms: {stats(commits)}")
    runs = [(e["t_end"] - e["t_start"]) / 1000 for e in events if e["ev"] == "run"]
    lines.append(f"all state operations ms: {stats(runs)}")
    inits = [(e["t_end"] - e["t_start"]) / 1000 for e in events if e["ev"] == "init"]
    lines.append(f"state initialisations ms: {stats(inits)}")
    return lines


def analyze_recovery(events):
    data = [e for e in events if e["ev"] == "stream_data"]
    cycles = [e for e in events if e["ev"] == "recovery_cycle"]
    steps = [e for e in events if e["ev"] == "recovery_step"]
    if not cycles:
        return ["no recovery cycles"]
    t0 = min([e["t"] for e in data] or [cycles[0]["t0"]])
    lines = [
        "stream data at ms: " + ", ".join(f"{(e['t'] - t0) / 1000:.0f} {e['services']}" for e in data[:6]),
        "cycles (start ms +duration ms outcome): "
        + ", ".join(f"{(c['t0'] - t0) / 1000:.0f}+{(c['t1'] - c['t0']) / 1000:.0f} {c['outcome']}" for c in cycles),
    ]
    by_step = {}
    for s in steps:
        by_step.setdefault(s["step"], []).append(s)
    for name, values in by_step.items():
        lines.append(
            f"step {name} ms: {stats((v['t1'] - v['t0']) / 1000 for v in values)}; outcomes "
            + ", ".join(str(v["ok"]) for v in values[-6:])
        )
    runs = [(e["t_end"] - e["t_start"]) / 1000 for e in events if e["ev"] == "run"]
    lines.append(f"state operations ms: {stats(runs)}")
    commits = [(e["t3"] - e["t2"]) / 1000 for e in events if e["ev"] == "audit" and e["outcome"] == "ok"]
    lines.append(f"audit commits ms: {stats(commits)}")
    return lines


def section(title):
    print(f"\n===== {title} =====", flush=True)


def loop(title, test, args, count, analyze, detail_passes=2):
    section(title)
    passes = 0
    for n in range(count):
        diag = OUT / f"{test}-{len(args)}-{n}.jsonl"
        code, seconds, output = cargo_test(test, args, diag)
        passes += code == 0
        status = "PASS" if code == 0 else "FAIL"
        print(f"--- run {n + 1}/{count}: {status} in {seconds:.1f} s", flush=True)
        if code != 0:
            for line in failures(output):
                print("    " + line.strip()[:300])
        if code != 0 or n < detail_passes:
            for line in analyze(load(diag)):
                print("    " + line)
    print(f"{title}: {passes}/{count} passed", flush=True)


def main():
    loop("audit_chain, whole file", "audit_chain", [], 12, analyze_audit)
    loop(
        "audit_chain, the concurrent test alone",
        "audit_chain",
        ["--exact", "concurrent_writers_on_one_file_keep_a_single_chain"],
        6,
        analyze_audit,
    )
    loop("enterprise_regressions, whole file", "enterprise_regressions", [], 6, analyze_requests)
    loop(
        "enterprise_regressions, the retry-after test alone",
        "enterprise_regressions",
        ["--exact", "a_retry_after_past_the_deadline_is_reported_as_the_rate_limit"],
        6,
        analyze_requests,
    )
    loop("recovery_integration", "recovery_integration", [], 8, analyze_recovery, detail_passes=3)
    section("probes and strategies")
    code, seconds, output = cargo_test("zz_diag", ["--nocapture", "--test-threads=1"])
    for line in output.splitlines():
        if line.startswith("DIAG"):
            print(line, flush=True)
    print(f"probe exit {code} in {seconds:.0f} s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
