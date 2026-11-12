"""Publish the tested tarball and reconcile npm tags without moving them backwards."""

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

REGISTRY = "https://registry.npmjs.org"


def version_key(version):
    match = re.fullmatch(
        r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
        r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
        r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?", version
    ) if isinstance(version, str) else None
    if not match:
        raise ValueError(f"Invalid npm version: {version!r}")
    prerelease = match[4].split(".") if match[4] else []
    if any(part.isdigit() and len(part) > 1 and part.startswith("0") for part in prerelease):
        raise ValueError(f"Invalid npm version: {version!r}")
    return (*map(int, match.group(1, 2, 3)), not prerelease,
            tuple((0, int(part)) if part.isdigit() else (1, part) for part in prerelease))


def request(url, missing=False):
    result = subprocess.run(
        ["curl", "-sSL", "--connect-timeout", "10", "--max-time", "60",
         "--max-filesize", str(32 * 1024 * 1024), "--proto", "=https",
         "--proto-redir", "=https", "--write-out", "\n%{http_code}", "--url", url],
        check=True, capture_output=True, timeout=70,
    )
    body, status = result.stdout.rsplit(b"\n", 1)
    if missing and status == b"404":
        return None
    if status != b"200":
        raise RuntimeError(f"npm lookup returned HTTP {status.decode()}; no registry write is safe")
    return body


def tags():
    values = json.loads(request(f"{REGISTRY}/-/package/boubou/dist-tags"))
    if not isinstance(values, dict):
        raise ValueError("npm returned invalid dist-tags")
    for value in values.values():
        version_key(value)
    return values


def served(version, expected):
    body = request(f"{REGISTRY}/boubou/{version}", missing=True)
    if body is None:
        return False
    metadata = json.loads(body)
    if not isinstance(metadata, dict) or metadata.get("name") != "boubou" or metadata.get("version") != version:
        raise ValueError("npm returned a different package identity")
    dist = metadata.get("dist")
    url = dist.get("tarball") if isinstance(dist, dict) else None
    if not isinstance(url, str) or not url.startswith("https://"):
        raise ValueError("npm returned an invalid tarball URL")
    actual = request(url)
    if actual != expected:
        raise ValueError(f"npm already holds different bytes for boubou@{version}. Release a new version.")
    return True


def wait_for(description, check, deadline):
    while time.monotonic() < deadline:
        try:
            if check():
                return
        except (RuntimeError, OSError, subprocess.SubprocessError) as error:
            print(f"Waiting for npm: {error}", file=sys.stderr)
        time.sleep(min(30, max(0, deadline - time.monotonic())))
    raise RuntimeError(f"npm did not serve {description} within 30 minutes. Re-run this job.")


def reconcile_tag(tag, version, deadline, prerelease_only=False):
    wanted = version_key(version)

    def keep(current):
        return current is not None and (
            version_key(current) >= wanted or (prerelease_only and version_key(current)[3])
        )

    current = tags().get(tag)
    if keep(current):
        print(f"Keeping npm {tag} at {current}")
        return
    try:
        subprocess.run(["npm", "dist-tag", "add", f"boubou@{version}", tag], check=True, timeout=120)
    except subprocess.CalledProcessError as error:
        raise RuntimeError(
            f"npm refused to move {tag}. Check the trusted publisher's Allow npm dist-tag permission, "
            "then re-run this job."
        ) from error
    wait_for(f"boubou@{version} as {tag}", lambda: keep(tags().get(tag)), deadline)
    print(f"npm {tag} serves {version} or a newer version")


def publish(version, tarball):
    wanted = version_key(version)
    channel = version.split("-", 1)[1].split("+", 1)[0].split(".", 1)[0] if not wanted[3] else "latest"
    expected = Path(tarball).read_bytes()
    current = tags().get(channel)
    deadline = time.monotonic() + 30 * 60
    if served(version, expected):
        print(f"npm already serves boubou@{version}")
    else:
        if current and version_key(current) >= wanted:
            raise ValueError(f"Refusing to publish {version}: npm {channel} already points to {current}.")
        subprocess.run(["npm", "publish", str(tarball), "--tag", channel, "--access", "public"],
                       check=True, timeout=300)
        wait_for(f"the exact boubou@{version} tarball", lambda: served(version, expected), deadline)
    reconcile_tag(channel, version, deadline)
    if channel != "latest":
        # Stable latest is never replaced by a prerelease, even if its version number is smaller.
        reconcile_tag("latest", version, deadline, prerelease_only=True)


if __name__ == "__main__":
    try:
        publish(os.environ["VERSION"], os.environ["TARBALL"])
    except (ValueError, KeyError, RuntimeError, OSError, subprocess.SubprocessError) as error:
        sys.exit(f"::error::{error}")
