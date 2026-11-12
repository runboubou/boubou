"""Reuse completed model checks only for the same reviewed inputs and immutable assets."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[2]
# #99 added the builder and smoke script to the digest without changing the runner's contents.
# This exact transition accepts beta.9's existing proof; any later builder change requires a build.
REUSE_BUILDER = "e2647ed112eb4fda115411bb9a0ce0139a96577b"
LEGACY_RUNNER = "41e327889bf5f96d30056b964424883deb79ec6aeeaf308a91e9f275292a46ec"
LEGACY_URL = "https://github.com/runboubou/boubou/releases/download/v0.1.0-beta.9/boubou-qwen-runner-0.1.0-beta.9-darwin-arm64.tar.gz"
UI = {"src/onboarding.rs", "src/progress.rs", "src/talk.rs", "src/tui.rs", "src/update.rs"}
CHECKOUT_STEP = "Run actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1"


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT)


def available(commit):
    if len(commit) != 40 or any(c not in "0123456789abcdef" for c in commit):
        raise ValueError("Invalid source commit")
    if subprocess.run(["git", "cat-file", "-e", f"{commit}^{{commit}}"], cwd=ROOT,
                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
        subprocess.run(["git", "fetch", "--quiet", "--depth=1", "origin", commit], cwd=ROOT, check=True)


def blob(ref, path):
    return git("rev-parse", f"{ref}:{path}").decode().strip()


def api(path):
    repository = os.environ["GITHUB_REPOSITORY"]
    return json.loads(subprocess.check_output(["gh", "api", f"repos/{repository}/{path}"]))


def successful(run, workflow, commit=None):
    return (run.get("conclusion") == "success" and run.get("status") == "completed"
            and run.get("path") == f".github/workflows/{workflow}"
            and (commit is None or run.get("head_sha") == commit))


def published_release(build):
    commit, version = build["source_commit"], build["version"]
    available(commit)
    runs = api(f"actions/runs?head_sha={commit}&status=success&per_page=30")["workflow_runs"]
    return any(successful(run, "release.yml", commit) and run.get("head_branch") == f"v{version}"
               and run.get("event") == "push" for run in runs)


def compatible_runner(manifest, full, legacy, builder, same_smoke):
    return (manifest.get("sources_sha256") == full or
            (manifest.get("sources_sha256") == legacy and builder == REUSE_BUILDER
             and same_smoke and manifest.get("sha256") == LEGACY_RUNNER
             and manifest.get("url") == LEGACY_URL))


def runner(published, output):
    build = json.loads((published / "BUILD.json").read_text())
    if not published_release(build):
        return False
    module_path = ROOT / "packaging/qwen/build_runner.py"
    sys.path.insert(0, str(module_path.parent))
    spec = importlib.util.spec_from_file_location("build_runner", module_path)
    builder = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(builder)
    target = builder.TARGETS["Darwin", "arm64"]
    legacy = {key: value for key, value in target.items() if key != "tools"}
    manifest_path = published / "qwen/runtime.json"
    manifest = json.loads(manifest_path.read_text())
    if not compatible_runner(manifest, builder.sources_digest(target), builder.sources_digest(legacy),
                             blob("HEAD", "packaging/qwen/build_runner.py"),
                             blob("HEAD", "packaging/qwen/smoke.py") ==
                             blob(build["source_commit"], "packaging/qwen/smoke.py")):
        return False
    # Keep URL, digest and all provenance exactly as the verified package published them.
    builder.download(manifest["url"], manifest["sha256"], output / "runner.tar.gz")
    if (output / "runner.tar.gz").stat().st_size != manifest["size"]:
        raise ValueError("Reused runner has the wrong size")
    shutil.copyfile(manifest_path, output / "runtime.json")
    print(f"Reusing the immutable runner verified by v{build['version']}", file=sys.stderr)
    return True


def liquid_inputs(ref):
    """UI files and this crate's version do not alter an already tested inference path."""
    paths = git("ls-tree", "-r", "--name-only", ref, "--", "src", "native", "build.rs",
                "Cargo.toml", "Cargo.lock", ".github/workflows/liquid-macos.yml").decode().splitlines()
    digest = hashlib.sha256()
    for path in sorted(set(paths) - UI):
        content = git("show", f"{ref}:{path}")
        if path in {"Cargo.toml", "Cargo.lock"}:
            data = tomllib.loads(content.decode())
            if path == "Cargo.toml":
                data["package"].pop("version", None)
            else:
                for package in data["package"]:
                    if package["name"] == "boubou" and "source" not in package:
                        package.pop("version", None)
            content = json.dumps(data, sort_keys=True).encode()
        digest.update(path.encode() + b"\0" + content + b"\0")
    return digest.hexdigest()


def qwen_inputs(ref):
    """The CLI/runtime integration requires both the Rust caller and its packaged Python path."""
    digest = hashlib.sha256(liquid_inputs(ref).encode())
    paths = git("ls-tree", "-r", "--name-only", ref, "--", "packaging/qwen",
                "packaging/cli.cjs", "packaging/build-macos.py",
                ".github/workflows/qwen-macos.yml").decode().splitlines()
    for path in sorted(path for path in paths if not path.endswith(".md")):
        digest.update(path.encode() + b"\0" + git("show", f"{ref}:{path}") + b"\0")
    return digest.hexdigest()


def checkout_source(log):
    """Read the pinned checkout action's actual Git receipt, not a PR run's head_sha."""
    lines = [line.split("\t", 2) for line in log.splitlines()]
    commits = []
    for before, after in zip(lines, lines[1:]):
        if (len(before) == len(after) == 3 and before[:2] == after[:2]
                and before[1] == CHECKOUT_STEP
                and re.search(r"\[command\].*/git log -1 --format=%H$", before[2])):
            commit = after[2].split(" ", 1)[-1]
            if re.fullmatch(r"[0-9a-f]{40}", commit):
                commits.append(commit)
    return commits[0] if len(commits) == 1 else None


def job_source(run, job):
    log = subprocess.check_output(["gh", "run", "view", str(run["id"]), "--repo",
                                   os.environ["GITHUB_REPOSITORY"], "--job", str(job["id"]), "--log"], text=True)
    return checkout_source(log)


def checked_inference(workflow, step_name, inputs):
    current = inputs("HEAD")
    runs = api(f"actions/workflows/{workflow}/runs?status=success&per_page=20")["workflow_runs"]
    for run in [run for run in runs if successful(run, workflow)][:5]:
        jobs = api(f"actions/runs/{run['id']}/jobs")["jobs"]
        tested = [job for job in jobs if job.get("conclusion") == "success" and any(
                step.get("name") == step_name and
                step.get("conclusion") == "success" for step in job.get("steps", []))]
        if not tested:
            continue
        source = job_source(run, tested[0])
        if source is None:
            continue
        available(source)
        if inputs(source) != current:
            continue
        if workflow == "qwen-macos.yml":
            # The checked Python source and the packaged Rust caller must come from the same tree.
            candidates = [job for job in jobs if job.get("name") == "candidate" and job.get("conclusion") == "success"]
            if len(candidates) != 1 or job_source(run, candidates[0]) != source:
                continue
        print(f"Reusing {workflow} model checks at {source}: {run['html_url']}", file=sys.stderr)
        return True
    return False


def liquid():
    return checked_inference("liquid-macos.yml", "Real model conversations, offline", liquid_inputs)


def qwen():
    return checked_inference("qwen-macos.yml", "Check offline speech inference through the loopback service", qwen_inputs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    reuse = sub.add_parser("runner")
    reuse.add_argument("published", type=Path)
    reuse.add_argument("output", type=Path)
    sub.add_parser("liquid")
    sub.add_parser("qwen")
    args = parser.parse_args()
    try:
        if args.command == "runner":
            reused = runner(args.published, args.output)
        else:
            reused = liquid() if args.command == "liquid" else qwen()
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        # Missing proof never skips validation. A partial runner is not handed to another build.
        print(f"Cannot reuse model checks: {error}", file=sys.stderr)
        if args.command == "runner" and (args.output / "runner.tar.gz").exists():
            return 2
        reused = False
    return 0 if reused else 1


if __name__ == "__main__":
    sys.exit(main())
