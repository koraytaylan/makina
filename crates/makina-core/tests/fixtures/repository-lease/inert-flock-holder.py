#!/usr/bin/env python3
"""Inert cross-language flock fixture; never brokers Git or workflow work."""
import argparse
import fcntl
import json
import os
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--git-common-dir", required=True)
parser.add_argument("--run", required=True)
parser.add_argument("--nonblocking", action="store_true")
args = parser.parse_args()
common = Path(args.git_common_dir).resolve(strict=True)
if not common.is_dir() or not args.run or any(c in args.run for c in "/\\\0"):
    raise SystemExit("invalid fixture input")
flags = os.O_RDWR | os.O_CREAT | os.O_CLOEXEC
if hasattr(os, "O_NOFOLLOW"):
    flags |= os.O_NOFOLLOW
fd = os.open(common / "makina.repository.lock", flags, 0o600)
try:
    lock_flags = fcntl.LOCK_EX | (fcntl.LOCK_NB if args.nonblocking else 0)
    try:
        fcntl.flock(fd, lock_flags)
    except BlockingIOError:
        print(json.dumps({"event": "contended", "run": args.run}), flush=True)
        raise SystemExit(75)
    print(json.dumps({"event": "ready", "run": args.run}), flush=True)
    for line in __import__("sys").stdin:
        request = json.loads(line)
        if request == {"op": "release"}:
            print(json.dumps({"event": "released", "run": args.run}), flush=True)
            raise SystemExit(0)
        print(json.dumps({"error": "fixture accepts only release"}), flush=True)
finally:
    os.close(fd)
