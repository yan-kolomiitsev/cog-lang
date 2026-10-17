#!/usr/bin/env python3
"""Run a pinned libFuzzer binary, measuring child CPU time independently of wall time.

Linux/Unix runner. Any nonzero exit, parent timeout or sanitizer report stops the
campaign and retains its log/artifacts. Never executes Cog programs itself.
"""
import argparse
import datetime
import hashlib
import json
import math
import os
import platform
from pathlib import Path
import re
import resource
import subprocess
import sys
import time

if sys.platform != "linux":
    raise SystemExit("campaign CPU/RSS accounting is validated only on Linux")

MAX_LENGTH = {"frontend": 65536, "kdl": 262145, "decoder": 65536, "verifier": 36864}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def corpus_stats(directory):
    files = sorted(p for p in directory.iterdir() if p.is_file())
    manifest = hashlib.sha256()
    total_bytes = 0
    for path in files:
        size = path.stat().st_size
        total_bytes += size
        manifest.update(path.name.encode("utf-8", errors="surrogateescape"))
        manifest.update(b"\0" + size.to_bytes(8, "little"))
        manifest.update(bytes.fromhex(digest(path)))
    return {"files": len(files), "bytes": total_bytes,
            "manifest_sha256": manifest.hexdigest()}


def save(path, value):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def inspect_log(path):
    final_coverage = None
    final_executions = None
    finding = False
    with path.open(errors="replace") as stream:
        for line in stream:
            coverage = re.search(r"cov:\s*(\d+)\s+ft:\s*(\d+)", line)
            if coverage:
                final_coverage = (int(coverage.group(1)), int(coverage.group(2)))
            executions = re.search(r"stat::number_of_executed_units:\s*(\d+)", line)
            if executions:
                final_executions = int(executions.group(1))
            if re.search(r"ERROR:|SUMMARY:.*Sanitizer|runtime error:", line):
                finding = True
    return {"sanitizer_or_fuzzer_error": finding,
            "final_coverage": list(final_coverage) if final_coverage else None,
            "executions": final_executions}


def run(args):
    if not math.isfinite(args.cpu_hours) or args.cpu_hours <= 0:
        raise ValueError("CPU hours must be finite and positive")
    if args.chunk_seconds < 1 or args.parent_timeout <= args.chunk_seconds:
        raise ValueError("parent timeout must exceed a positive chunk duration")
    binary = args.binary.resolve(strict=True)
    run_directory = args.run_directory.resolve()
    # Exclusive creation prevents overwriting evidence or silently counting a
    # previous campaign. Resume requires an explicit audit/continuation record.
    run_directory.mkdir(parents=True, exist_ok=False)
    corpus = args.corpus.resolve()
    artifacts = args.artifacts.resolve()
    corpus.mkdir(parents=True, exist_ok=True)
    artifacts.mkdir(parents=True, exist_ok=True)
    record = {
        "state": "running", "target": args.target,
        "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "runner_pid": os.getpid(), "platform": platform.platform(),
        "architecture": platform.machine(), "binary": str(binary), "binary_sha256": digest(binary),
        "corpus": str(corpus), "artifacts": str(artifacts), "initial_corpus": corpus_stats(corpus),
        "required_cpu_seconds": args.cpu_hours * 3600, "cpu_seconds": 0.0,
        "cpu_accounting": "RUSAGE_CHILDREN user+system; single child per chunk",
        "rss_accounting": "RUSAGE_CHILDREN maximum high-water mark in KiB (campaign process lifetime)",
        "base_seed": args.seed, "chunk_seconds": args.chunk_seconds,
        "parent_timeout_seconds": args.parent_timeout,
        "fuzzer_timeout_seconds": 5, "rss_limit_mb": 1024,
        "asan_options": os.environ.get("ASAN_OPTIONS", ""),
        "tool_evidence": str(args.tool_evidence.resolve(strict=True)),
        "tool_evidence_sha256": digest(args.tool_evidence), "chunks": [],
    }
    evidence = run_directory / "campaign.json"
    save(evidence, record)
    started = time.monotonic()
    while record["cpu_seconds"] < record["required_cpu_seconds"]:
        if digest(binary) != record["binary_sha256"]:
            record.update(state="blocked", reason="pinned binary changed")
            save(evidence, record)
            return 1
        index = len(record["chunks"])
        seconds = min(args.chunk_seconds, max(1, math.ceil(record["required_cpu_seconds"] - record["cpu_seconds"])))
        command = [str(binary), str(corpus), f"-artifact_prefix={artifacts}/",
                   f"-max_total_time={seconds}", f"-max_len={MAX_LENGTH[args.target]}",
                   "-rss_limit_mb=1024", "-timeout=5", f"-seed={args.seed + index}", "-print_final_stats=1"]
        log = run_directory / f"chunk-{index:04}.log"
        before = resource.getrusage(resource.RUSAGE_CHILDREN)
        wall = time.monotonic()
        timed_out = False
        with log.open("xb") as output:
            child = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT)
            record["active_child_pid"] = child.pid
            record["active_command"] = command
            save(evidence, record)
            try:
                code = child.wait(timeout=args.parent_timeout)
            except subprocess.TimeoutExpired:
                timed_out = True
                child.kill()
                code = child.wait()
        after = resource.getrusage(resource.RUSAGE_CHILDREN)
        user = after.ru_utime - before.ru_utime
        system = after.ru_stime - before.ru_stime
        chunk = {"index": index, "command": command, "exit_code": code, "parent_timeout": timed_out,
                 "user_cpu_seconds": user, "system_cpu_seconds": system,
                 "wall_seconds": time.monotonic() - wall,
                 "children_maxrss_kib_cumulative_high_water": after.ru_maxrss,
                 "log": str(log), "log_sha256": digest(log), **inspect_log(log)}
        record["chunks"].append(chunk)
        record["cpu_seconds"] += user + system
        record["wall_seconds"] = time.monotonic() - started
        record["corpus_now"] = corpus_stats(corpus)
        record["children_maxrss_kib_campaign_high_water"] = after.ru_maxrss
        record.pop("active_child_pid", None)
        record.pop("active_command", None)
        if code != 0 or timed_out or chunk["sanitizer_or_fuzzer_error"]:
            record.update(state="finding_requires_investigation", reason="abnormal fuzz exit; preserve all evidence")
            save(evidence, record)
            return 1
        save(evidence, record)
    record.update(state="cpu_budget_completed_pending_review",
                  finished_utc=datetime.datetime.now(datetime.timezone.utc).isoformat())
    save(evidence, record)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=MAX_LENGTH)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--corpus", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--run-directory", required=True, type=Path)
    parser.add_argument("--tool-evidence", required=True, type=Path)
    parser.add_argument("--cpu-hours", type=float, default=24)
    parser.add_argument("--chunk-seconds", type=int, default=3600)
    parser.add_argument("--parent-timeout", type=int, default=7200)
    parser.add_argument("--seed", type=int, default=20261017)
    return run(parser.parse_args())


if __name__ == "__main__":
    sys.exit(main())
