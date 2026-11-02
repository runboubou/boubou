#!/usr/bin/env python3
"""Check the candidate checkout, without reading ignored files or Git history.

These are conservative repository guardrails, not an English-language detector,
a general YAML parser, a secret scanner, or a substitute for workflow review.
"""

import argparse
from pathlib import Path
import re
import subprocess
import sys
from typing import List


ROOT = Path(__file__).resolve().parents[1]
REQUIRED = (
    "AGENTS.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "Cargo.lock",
    "docs/development/review.md",
    "docs/development/testing.md",
    "docs/specs/0002-engineering-quality.md",
    "docs/specs/TEMPLATE.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    ".github/workflows/quality.yml",
)
CJK = re.compile(
    "[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff"
    "\U00020000-\U0002ffff\U00030000-\U000323af]"
)
CONFLICT = re.compile(r"^(?:<{7}|={7}|>{7})(?:[ \t].*)?$", re.MULTILINE)
PRIVATE_KEY = re.compile(r"-----BEGIN (?:[A-Z0-9]+ )?PRIVATE KEY-----")
FORBIDDEN_SUFFIXES = {
    ".aiff", ".bin", ".flac", ".gguf", ".key", ".log", ".mp3", ".p12",
    ".pcm", ".pem", ".pfx", ".safetensors", ".wav",
}
MAX_FILE_BYTES = 1024 * 1024


def candidate_files(root: Path) -> List[str]:
    """Include tracked files even when ignored, and ordinary untracked additions."""
    result = subprocess.run(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    return sorted(set(result.stdout.decode("utf-8").split("\0")) - {""})


def workflow_errors(name: str, source: str) -> List[str]:
    """Static checks for this project's deliberately simple workflow style.

    Reject surprising YAML indirection rather than attempt to interpret it. GitHub
    validates YAML syntax; reviewers must still inspect triggers, data flow, jobs,
    shell commands, and the exact pinned action revisions.
    """
    errors = []
    lines = [line.split(" #", 1)[0] for line in source.splitlines()
             if not line.lstrip().startswith("#")]
    visible = "\n".join(lines)
    if re.search(r"\b(?:pull_request_target|self-hosted|write-all)\b", visible):
        errors.append(f"{name}: privileged triggers, self-hosted runners, and write-all are forbidden")
    if re.search(r"\bsecrets\s*[.\[]", visible):
        errors.append(f"{name}: ordinary verification must not consume secrets")
    if re.search(r"(?:^|:)\s*[&*][\w-]+|^\s*<<\s*:", visible, re.MULTILINE):
        errors.append(f"{name}: YAML anchors and aliases require a separately reviewed policy change")
    if not re.search(r"^permissions:\s*\n  contents: read\s*$", visible, re.MULTILINE):
        errors.append(f"{name}: declare top-level permissions with contents: read")
    if re.search(r"^[ \t]*permissions:[ \t]*\S", visible, re.MULTILINE):
        errors.append(f"{name}: use block-style permission mappings so every permission is reviewable")
    if re.search(r"^\s*[\w-]+:\s*['\"]?write['\"]?\s*$", visible, re.MULTILINE):
        errors.append(f"{name}: verification workflows must not request write permissions")
    if re.search(r"^\s*(?:-\s*)?uses\s*:", visible, re.MULTILINE):
        for line in lines:
            match = re.match(r"\s*(?:-\s*)?uses\s*:\s*(\S+)\s*$", line)
            if match and not re.fullmatch(r"[\w.-]+/[\w./-]+@[0-9a-f]{40}", match.group(1)):
                errors.append(f"{name}: every external action must use a full 40-character commit SHA")
    for index, line in enumerate(lines):
        checkout = re.match(r"^(\s*)(-\s*)?uses\s*:\s*actions/checkout@", line)
        if not checkout:
            continue
        step_indent = len(checkout.group(1)) - (0 if checkout.group(2) else 2)
        block = []
        for following in lines[index + 1:]:
            if following.strip() and len(following) - len(following.lstrip()) <= step_indent:
                break
            block.append(following)
        with_indent = step_indent + 2
        with_pattern = r"^ {" + str(with_indent) + r"}with:\s*$"
        disabled = False
        for position, following in enumerate(block):
            if not re.match(with_pattern, following):
                continue
            for option in block[position + 1:]:
                if option.strip() and len(option) - len(option.lstrip()) <= with_indent:
                    break
                if re.fullmatch(" " * (with_indent + 2) + r"persist-credentials:\s*false\s*", option):
                    disabled = True
        if not disabled:
            errors.append(f"{name}: every checkout step must set with.persist-credentials: false")
    return errors


def file_errors(root: Path, name: str) -> List[str]:
    errors = []
    relative = Path(name)
    path = root / relative
    if relative.is_absolute() or ".." in relative.parts:
        return [f"{name}: path is outside the candidate checkout"]
    if CJK.search(name):
        errors.append(f"{name}: filename contains CJK characters; repository contributions use English")
    if any((root / part).is_symlink() for part in (relative, *relative.parents)):
        return errors + [f"{name}: symlinks are not accepted by the candidate file checker"]
    if not path.exists():
        return errors  # A tracked deletion is part of a legitimate candidate.
    if not path.is_file():
        return errors + [f"{name}: expected a regular file"]
    basename = relative.name.lower()
    if (relative.suffix.lower() in FORBIDDEN_SUFFIXES or basename == ".ds_store"
            or basename == ".env" or basename.startswith(".env.")
            or basename in {"id_rsa", "id_ed25519", "credentials.json"}):
        return errors + [f"{name}: private data, credentials, model assets, and runtime artifacts must not be committed"]
    if path.stat().st_size > MAX_FILE_BYTES:
        return errors + [f"{name}: exceeds the 1 MiB source-file limit; review asset handling explicitly"]
    data = path.read_bytes()
    if b"\0" in data:
        return errors + [f"{name}: binary content requires an explicit repository-policy change"]
    try:
        source = data.decode("utf-8")
    except UnicodeDecodeError:
        return errors + [f"{name}: repository text must be UTF-8"]
    if CJK.search(source):
        errors.append(f"{name}: contains CJK characters; repository contributions use English")
    if CONFLICT.search(source):
        errors.append(f"{name}: unresolved merge-conflict marker")
    if PRIVATE_KEY.search(source):
        errors.append(f"{name}: private-key marker detected; contents withheld")
    if name.startswith(".github/workflows/") and relative.suffix in {".yml", ".yaml"}:
        errors.extend(workflow_errors(name, source))
    return errors


def check(root: Path) -> List[str]:
    files = candidate_files(root)
    errors = []
    for name in REQUIRED:
        if name not in files or not (root / name).is_file():
            errors.append(f"{name}: required contribution or verification entry point is missing")
    for name in files:
        errors.extend(file_errors(root, name))
    return errors


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT)
    args = parser.parse_args(argv)
    try:
        errors = check(args.root.resolve())
    except (OSError, UnicodeError, subprocess.CalledProcessError) as error:
        print(f"FAIL: cannot inspect candidate files ({type(error).__name__})", file=sys.stderr)
        return 1
    if errors:
        for error in errors:
            print(f"FAIL: {error}", file=sys.stderr)
        return 1
    print("PASS: candidate repository guardrails (current files only; history and remote policy not checked)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
