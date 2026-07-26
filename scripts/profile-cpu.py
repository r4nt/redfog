#!/usr/bin/env python3
"""Per-thread CPU profiler for a live redfog-server process.

Non-invasive: reads /proc/<pid>/task/*/stat deltas over a sampling window —
no gdbserver/perf attach (gdbserver pauses the target before any client can
even connect, which perturbs exactly the kind of timing behavior these
investigations care about — see project memory; perf isn't installed on
this machine anyway). Takes one snapshot, sleeps for the window, takes
another, and reports each GStreamer thread's aggregate CPU% (of one core)
over that window — replacing the previous ad hoc one-off /proc sampling
with something repeatable across runs.

Run this WHILE a real client is actively streaming (see
scripts/sudo-live-session.sh) — idle CPU tells you nothing. Cross-reference
against the periodic "video: N fps, M kbps" line redfog-moonlight logs
every 5s (see EncodedFrameStats in crates/redfog-moonlight/src/session.rs)
to know what fps/bitrate a given CPU number corresponds to.

Usage:
    scripts/profile-cpu.py [--pid PID] [--duration SECONDS]

Without --pid, auto-detects the single running (non-zombie) redfog-server
process via /proc/*/comm — errors out if there's more than one (e.g. a
prior run's unreaped defunct process still listed under the same name) so
you don't silently profile the wrong one.

redfog-server usually runs as root (see sudo-live-session.sh) — if this
script can't read its /proc/<pid>/task/*/stat entries as your normal user,
re-run it with sudo too.
"""
import argparse
import os
import re
import sys
import time

CLK_TCK = os.sysconf("SC_CLK_TCK")
STAT_RE = re.compile(r"^\d+ \((.*)\) (.*)$")


def find_pid():
    candidates = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open(f"/proc/{pid}/comm") as f:
                comm = f.read().strip()
            with open(f"/proc/{pid}/status") as f:
                status = f.read()
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        if comm != "redfog-server":
            continue
        if re.search(r"^State:\s*Z", status, re.MULTILINE):
            continue  # zombie — no live threads to sample
        candidates.append(pid)
    if not candidates:
        sys.exit("error: no running (non-zombie) redfog-server process found; pass --pid explicitly or start one via scripts/sudo-live-session.sh")
    if len(candidates) > 1:
        sys.exit(f"error: multiple redfog-server processes found, pass --pid explicitly: {candidates}")
    return candidates[0]


def snapshot(pid):
    threads = {}
    task_dir = f"/proc/{pid}/task"
    try:
        tids = os.listdir(task_dir)
    except FileNotFoundError:
        sys.exit(f"error: pid {pid} no longer exists")
    except PermissionError:
        sys.exit(f"error: permission denied reading {task_dir} — redfog-server is probably running as root; re-run this script with sudo too")
    for tid in tids:
        try:
            with open(f"{task_dir}/{tid}/stat") as f:
                line = f.read()
        except (FileNotFoundError, ProcessLookupError):
            continue  # thread exited between listdir() and open()
        m = STAT_RE.match(line)
        if not m:
            continue
        comm = m.group(1)
        # `rest` starts at stat's field 3 (state), so utime (field 14
        # overall) is index 11 and stime (field 15) is index 12 here.
        rest = m.group(2).split()
        utime, stime = int(rest[11]), int(rest[12])
        threads[tid] = (comm, utime, stime)
    return threads


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--pid", type=int, help="redfog-server PID (auto-detected if omitted)")
    parser.add_argument("--duration", type=float, default=10.0, help="sampling window in seconds (default: 10)")
    args = parser.parse_args()

    pid = args.pid or find_pid()
    print(f"sampling redfog-server (pid {pid}) for {args.duration}s...", file=sys.stderr)

    before = snapshot(pid)
    time.sleep(args.duration)
    after = snapshot(pid)

    by_name = {}
    for tid, (comm, u1, s1) in after.items():
        if tid not in before:
            continue  # thread started after the window began; not comparable
        _, u0, s0 = before[tid]
        delta_ticks = (u1 - u0) + (s1 - s0)
        if delta_ticks <= 0:
            continue
        name, count, ticks = by_name.get(comm, (comm, 0, 0))
        by_name[comm] = (comm, count + 1, ticks + delta_ticks)

    rows = sorted(by_name.values(), key=lambda r: r[2], reverse=True)
    total_ticks = sum(r[2] for r in rows)

    if not rows:
        print("no CPU activity observed on any thread during the window — is the pipeline actually running/streaming?", file=sys.stderr)
        return

    print(f"{'THREAD':<20} {'CPU%':>7} {'THREADS':>8}")
    for name, count, ticks in rows:
        pct = ticks / CLK_TCK / args.duration * 100
        print(f"{name:<20} {pct:6.1f}% {count:8d}")
    total_pct = total_ticks / CLK_TCK / args.duration * 100
    print(f"{'TOTAL':<20} {total_pct:6.1f}%")


if __name__ == "__main__":
    main()
