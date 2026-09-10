#!/usr/bin/env python3
"""Run the synthetic stream soak and sample child RSS (Linux/macOS ps)."""
import argparse
import hashlib
import json
import math
import pathlib
import platform
import subprocess
import tempfile
import time


def qualify(result, samples, args):
    warmed = [s["rss_bytes"] for s in samples if s["seconds"] >= args.warmup_seconds]
    growth = max(warmed) - warmed[0] if warmed else None
    sample_times = [0.0] + [s["seconds"] for s in samples] + [result.get("wall_seconds", result["seconds"])]
    sampling_gap = max((b - a for a, b in zip(sample_times, sample_times[1:])), default=float("inf"))
    samples_valid = all(math.isfinite(s["seconds"]) and math.isfinite(s["rss_bytes"]) and s["seconds"] >= 0 and s["rss_bytes"] > 0 for s in samples) and all(b >= a for a, b in zip(sample_times, sample_times[1:]))
    checks = {
        "process_succeeded": result.get("process_returncode", 0) == 0 and not result.get("error"),
        "rss_sampling_continuous": bool(samples) and samples_valid and sampling_gap <= args.max_sample_gap_seconds,
        "completed_duration": result["seconds"] >= args.seconds,
        "delivered_data": result["delivered"] > 0,
        "delivery_gap_bounded": result.get("max_delivery_gap_seconds",float("inf")) <= args.max_delivery_gap_seconds,
        "reconnected": result["generation"] >= 2,
        "loss_behavior": result["lost_messages"] > 0 if args.consumer_delay_ms >= 10 else result["lost_messages"] == 0 if args.consumer_delay_ms == 0 else True,
        "rss_samples_present": len(samples) >= 2,
        "rss_below_limit": bool(samples) and result["peak_sampled_rss_bytes"] <= args.max_rss_mib * 1024**2,
        "warm_samples_present": len(warmed) >= 2,
        "rss_growth_below_limit": growth is not None and growth <= args.max_rss_growth_mib * 1024**2,
    }
    result.update(checks=checks, passed=all(checks.values()), warmed_peak_growth_bytes=growth, max_rss_sample_gap_seconds=sampling_gap,
                  thresholds={"max_rss_mib":args.max_rss_mib,"max_rss_growth_mib":args.max_rss_growth_mib,"warmup_seconds":args.warmup_seconds,"max_delivery_gap_seconds":args.max_delivery_gap_seconds,"max_sample_gap_seconds":args.max_sample_gap_seconds})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--seconds", type=int, default=60)
    parser.add_argument("--consumer-delay-ms", type=int, default=0)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--max-rss-mib", type=float, default=128)
    parser.add_argument("--max-rss-growth-mib", type=float, default=32)
    parser.add_argument("--max-delivery-gap-seconds", type=float, default=10)
    parser.add_argument("--max-sample-gap-seconds", type=float, default=5)
    parser.add_argument("--warmup-seconds", type=int, default=30)
    args = parser.parse_args()
    if not (0 < args.max_sample_gap_seconds < float("inf")) or not (0 < args.max_delivery_gap_seconds < float("inf")) or args.seconds < 1 or args.consumer_delay_ms < 0 or args.warmup_seconds < 0 or not (0 < args.max_rss_mib < float("inf")) or not (0 < args.max_rss_growth_mib < float("inf")):
        parser.error("duration must be positive and delay nonnegative")
    root = pathlib.Path(__file__).resolve().parents[1]
    samples = []
    binary = root / "target/release/enterprise-bench"
    binary_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
    result = {"seconds": 0, "delivered": 0, "generation": 0, "lost_messages": 0}
    with tempfile.TemporaryFile(mode="w+") as output, tempfile.TemporaryFile(mode="w+b") as errors:
        process = None
        started = time.monotonic()
        try:
            process = subprocess.Popen([str(binary), "stream", str(args.seconds), str(args.consumer_delay_ms)], stdout=output, stderr=errors)
            while process.poll() is None:
                measured = subprocess.run(["ps", "-o", "rss=", "-p", str(process.pid)], text=True, capture_output=True, timeout=2)
                if measured.returncode == 0 and measured.stdout.strip():
                    samples.append({"seconds": round(time.monotonic()-started, 3), "rss_bytes": int(measured.stdout.strip()) * 1024})
                if time.monotonic() - started > args.seconds + 30:
                    raise TimeoutError("soak exceeded bounded shutdown allowance")
                time.sleep(1)
            if process.returncode:
                raise RuntimeError(f"soak failed: {process.returncode}")
            output.seek(0)
            payload = json.load(output)
            required = ("seconds", "delivered", "generation", "lost_messages", "max_delivery_gap_seconds")
            if not isinstance(payload, dict) or any(not isinstance(payload.get(k), (int, float)) or isinstance(payload[k], bool) or not math.isfinite(payload[k]) or payload[k] < 0 for k in required):
                raise ValueError("invalid soak result metrics")
            result.update(payload)
        except (OSError, ValueError, RuntimeError, TimeoutError, subprocess.TimeoutExpired) as error:
            result["error"] = f"{type(error).__name__}: {error}"
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait()
            result["process_returncode"] = process.returncode if process is not None else None
            result["wall_seconds"] = time.monotonic() - started
            errors.seek(0, 2)
            errors.seek(max(0, errors.tell() - 8192))
            result["stderr_tail"] = errors.read().decode("utf-8", errors="replace")
    result.update(binary_sha256=binary_hash, platform=platform.platform(), rss_sampling_seconds=1, rss_samples=samples,
                  peak_sampled_rss_bytes=max((s["rss_bytes"] for s in samples), default=None),
                  memory_scope="one-second sampled child resident memory; not allocator high-water mark or a long-duration stability guarantee")
    qualify(result, samples, args)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({k:v for k,v in result.items() if k != "rss_samples"}, indent=2))
    if not result["passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
