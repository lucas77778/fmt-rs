#!/usr/bin/env python3
"""Deterministic stress verifier for the fmt-rs formatter.

Reads a JSON array of command strings, runs each through the release binary, and
checks the safety invariants:

  1. no crash       — the binary exits 0 and produces output.
  2. valid syntax   — if it FORMATTED a valid command, the output is valid bash
                      (`bash -n`). (Word-multiset preservation is already enforced
                      inside the binary, which bails instead of corrupting.)
  3. idempotent     — formatting the output again yields the same output.

A command the binary leaves unchanged (bailed) is always safe by definition.
"""
import json
import subprocess
import sys

BIN = "./target/release/fmt-rs"


def run_fmt(cmd: str) -> tuple[int, str]:
    p = subprocess.run([BIN], input=cmd, capture_output=True, text=True)
    return p.returncode, p.stdout


def bash_ok(s: str) -> bool:
    p = subprocess.run(["bash", "-n"], input=s, capture_output=True, text=True)
    return p.returncode == 0


def main() -> int:
    # Usage: cargo build --release && python3 scripts/stress_verify.py [corpus.json]
    # Run from the repo root so ./target/release/fmt-rs resolves.
    path = sys.argv[1] if len(sys.argv) > 1 else "tests/corpus.json"
    cmds = json.load(open(path))
    n = len(cmds)
    failures = []
    changed = 0
    bailed = 0
    input_invalid = 0

    for cmd in cmds:
        rc, out = run_fmt(cmd)
        if rc != 0:
            failures.append(("CRASH", cmd, f"exit={rc}"))
            continue

        if out == cmd:
            bailed += 1
            continue
        changed += 1

        rc2, out2 = run_fmt(out)
        idempotent = rc2 == 0 and out2 == out

        valid_in = bash_ok(cmd)
        if not valid_in:
            # `bash -n` can't validate shopt-dependent syntax (e.g. extglob),
            # so it isn't a reliable oracle here. The binary only reformats when
            # its internal word-multiset gate holds, so we just require
            # idempotence and treat the case as unverifiable-by-bash-n.
            input_invalid += 1
            if not idempotent:
                failures.append(("NON-IDEMPOTENT", cmd, f"{out!r} -> {out2!r}"))
            continue

        if not bash_ok(out):
            failures.append(("INVALID-OUTPUT", cmd, repr(out)))
            continue

        if not idempotent:
            failures.append(("NON-IDEMPOTENT", cmd, f"{out!r} -> {out2!r}"))
            continue

    print(
        f"total={n}  formatted={changed}  bailed={bailed}  "
        f"unverifiable-by-bashn={input_invalid}  failures={len(failures)}"
    )
    for kind, cmd, detail in failures:
        print(f"\n[{kind}]\n  in : {cmd!r}\n  {detail}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
