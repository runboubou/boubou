#!/usr/bin/env python3
"""Run Boubou's ordinary, model-free and device-free verification."""

import argparse
import hashlib
import os
from pathlib import Path
import platform
import re
import shlex
import subprocess
import sys
import tempfile

from check_repository import ROOT, candidate_files


class VerificationError(RuntimeError):
    pass


def run_step(label, command, root=ROOT, capture=False, report_success=True):
    print(f"RUN: {label}: {shlex.join(str(part) for part in command)}", flush=True)
    environment = os.environ.copy()
    environment["CARGO_TERM_COLOR"] = "never"
    try:
        result = subprocess.run(
            command, cwd=root, env=environment, check=False, text=True,
            stdout=subprocess.PIPE if capture else None,
            stderr=subprocess.STDOUT if capture else None, timeout=900,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise VerificationError(f"{label}: {type(error).__name__}") from error
    if capture and result.stdout:
        print(result.stdout, end="" if result.stdout.endswith("\n") else "\n", flush=True)
    if result.returncode != 0:
        raise VerificationError(f"{label}: exited {result.returncode}")
    if report_success:
        print(f"PASS: {label}", flush=True)
    return result.stdout or ""


def test_counts(output):
    summaries = re.findall(
        r"^test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;",
        output, re.MULTILINE,
    )
    passed = sum(int(row[0]) for row in summaries)
    failed = sum(int(row[1]) for row in summaries)
    ignored = sum(int(row[2]) for row in summaries)
    if passed == 0 or failed:
        raise VerificationError("Rust tests: no successful non-ignored tests were observed")
    return passed, ignored


def run_tool_tests(root):
    output = run_step("verification-tool regressions", [
        sys.executable, "-m", "unittest", "discover", "-s", "scripts/tests", "-v",
    ], root, capture=True, report_success=False)
    summary = re.search(r"^Ran (\d+) tests? in [^\n]+$", output, re.MULTILINE)
    skipped = re.search(r"^OK \(skipped=(\d+)\)$", output, re.MULTILINE)
    count = int(summary.group(1)) if summary else 0
    skipped_count = int(skipped.group(1)) if skipped else 0
    if count == 0 or count <= skipped_count:
        raise VerificationError("verification-tool regressions: no non-skipped tests were executed")
    print(f"PASS: {count - skipped_count} verification-tool regressions executed", flush=True)
    return count - skipped_count


def candidate_fingerprint(root):
    digest = hashlib.sha256()
    for name in candidate_files(root):
        path = root / name
        if path.is_symlink() or any((root / p).is_symlink() for p in Path(name).parents):
            raise VerificationError(f"Candidate contains a symlink: {name}")
        digest.update(name.encode("utf-8") + b"\0")
        if path.is_file():
            digest.update(hashlib.sha256(path.read_bytes()).digest())
        else:
            digest.update(b"deleted")
    return digest.hexdigest()


def native_compile(root):
    if platform.system() != "Darwin":
        print("NOT RUN: native audio harness compilation (requires macOS)", flush=True)
        return
    with tempfile.TemporaryDirectory(prefix="boubou-native-check-") as directory:
        run_step("native audio harness compilation (no execution)", [
            "clang", "-O2", "-fobjc-arc", "-mmacosx-version-min=13.0",
            "-framework", "AVFoundation", "-framework", "Foundation",
            "-framework", "CoreAudio", "native/audio_tests.m", "-o",
            str(Path(directory) / "audio-tests"),
        ], root)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--offline", action="store_true", help="Use already cached Cargo dependencies")
    parser.add_argument("--repository-only", action="store_true", help="Check repository policy and its tests only")
    args = parser.parse_args(argv)
    try:
        before = candidate_fingerprint(ROOT)
        run_step("repository guardrails", [sys.executable, "scripts/check_repository.py"])
        print(f"Candidate source SHA-256: {before}", flush=True)
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False)
        print(f"Git HEAD: {head.stdout.strip() or 'unborn'}", flush=True)
        status = subprocess.run(["git", "status", "--porcelain"], cwd=ROOT, text=True,
                                stdout=subprocess.PIPE, check=True)
        print(f"Working tree: {'dirty (candidate includes local changes)' if status.stdout else 'clean'}", flush=True)
        run_tool_tests(ROOT)
        if args.repository_only:
            print("NOT RUN: Rust formatting, lint, tests, build, and native compilation (--repository-only)")
        else:
            run_step("Rust formatting", ["cargo", "fmt", "--all", "--", "--check"])
            locked = ["--locked"] + (["--offline"] if args.offline else [])
            run_step("Rust lint", ["cargo", "clippy", *locked, "--all-targets", "--all-features", "--", "-D", "warnings"])
            output = run_step("Rust ordinary tests", ["cargo", "test", *locked, "--all-targets", "--all-features", "--", "--color", "never"], capture=True, report_success=False)
            passed, ignored = test_counts(output)
            print(f"PASS: {passed} ordinary Rust tests executed; {ignored} explicitly ignored", flush=True)
            run_step("Rust build", ["cargo", "build", *locked])
            native_compile(ROOT)
        after = candidate_fingerprint(ROOT)
        if before != after:
            raise VerificationError("candidate files changed during verification; review the changes and rerun")
    except (VerificationError, OSError, subprocess.SubprocessError, UnicodeError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print("NOT RUN: real models, microphone/speakers, acoustic acceptance, signing/notarization, or release checks")
    print("PASS: requested verification scope; this does not establish product or release acceptance")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
