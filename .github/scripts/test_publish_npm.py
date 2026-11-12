import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import publish_npm as release


class PublishTests(unittest.TestCase):
    version = "0.1.0-beta.10"
    previous = "0.1.0-beta.9"
    newer = "0.1.0-beta.11"
    archive_url = "https://registry.npmjs.org/boubou/-/boubou-0.1.0-beta.10.tgz"

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.tarball = Path(temporary.name) / "package.tgz"
        self.tarball.write_bytes(b"the tested archive")
        self.archive = self.tarball.read_bytes()
        self.present = True
        self.tags = {"beta": self.previous, "latest": self.previous}
        self.commands = []
        self.requests = []
        self.responses = {}
        self.now = 0
        for target, replacement in [
            ("subprocess.run", self.run_command),
            ("time.monotonic", lambda: self.now),
            ("time.sleep", self.sleep),
        ]:
            patcher = patch.object(getattr(release, target.split(".")[0]), target.split(".")[1], replacement)
            patcher.start()
            self.addCleanup(patcher.stop)
        quiet = patch("builtins.print")
        quiet.start()
        self.addCleanup(quiet.stop)

    def sleep(self, seconds):
        self.now += seconds

    def run_command(self, command, **kwargs):
        self.assertIn("timeout", kwargs)
        if command[0] == "curl":
            self.assertIn("--max-time", command)
            url = command[-1]
            self.requests.append(url)
            if url in self.responses:
                response = self.responses[url]
                if isinstance(response, list):
                    response = response.pop(0)
                if isinstance(response, Exception):
                    raise response
                body, status = response
            elif url.endswith("/dist-tags"):
                body, status = json.dumps(self.tags).encode(), 200
            elif url == self.archive_url:
                body, status = self.archive, 200
            elif url == f"{release.REGISTRY}/boubou/{self.version}":
                metadata = {"name": "boubou", "version": self.version, "dist": {"tarball": self.archive_url}}
                body, status = (json.dumps(metadata).encode(), 200) if self.present else (b"{}", 404)
            else:
                self.fail(f"Unexpected request: {url}")
            return subprocess.CompletedProcess(command, 0, body + f"\n{status}".encode())
        self.assertEqual(command[0], "npm")
        self.commands.append(command[1:])
        if command[1] == "publish":
            self.present = True
            self.tags[command[command.index("--tag") + 1]] = self.version
        elif command[1:3] == ["dist-tag", "add"]:
            self.tags[command[4]] = self.version
        else:
            self.fail(f"Unexpected npm command: {command}")
        return subprocess.CompletedProcess(command, 0)

    def publish(self):
        release.publish(self.version, self.tarball)

    def test_rerun_repairs_older_tags_without_republishing(self):
        self.publish()
        self.assertEqual(self.commands, [
            ["dist-tag", "add", f"boubou@{self.version}", "beta"],
            ["dist-tag", "add", f"boubou@{self.version}", "latest"],
        ])

    def test_missing_tags_are_repaired(self):
        self.tags = {}
        self.publish()
        self.assertEqual(self.tags, {"beta": self.version, "latest": self.version})
        self.assertTrue(all(command[0] == "dist-tag" for command in self.commands))

    def test_equal_or_newer_tags_are_preserved(self):
        for version in [self.version, self.newer]:
            with self.subTest(version=version):
                self.tags = {"beta": version, "latest": version}
                self.publish()
                self.assertEqual(self.commands, [])
                self.assertEqual(self.tags, {"beta": version, "latest": version})

    def test_stable_latest_is_preserved_even_when_numerically_older(self):
        self.tags["latest"] = "0.0.1"
        self.publish()
        self.assertEqual(self.tags, {"beta": self.version, "latest": "0.0.1"})
        self.assertEqual(len(self.commands), 1)

    def test_unpublished_older_version_does_not_rewind_its_channel(self):
        self.present = False
        self.tags["beta"] = self.newer
        with self.assertRaisesRegex(ValueError, "Refusing to publish"):
            self.publish()
        self.assertEqual(self.commands, [])

    def test_new_version_publishes_once_then_promotes_latest(self):
        self.present = False
        self.publish()
        self.assertEqual(self.commands, [
            ["publish", str(self.tarball), "--tag", "beta", "--access", "public"],
            ["dist-tag", "add", f"boubou@{self.version}", "latest"],
        ])

    def test_existing_different_tarball_fails_without_writes(self):
        self.archive = b"different archive"
        with self.assertRaisesRegex(ValueError, "different bytes.*Release a new version"):
            self.publish()
        self.assertEqual(self.commands, [])

    def test_lookup_failures_never_authorize_registry_writes(self):
        for url in [f"{release.REGISTRY}/-/package/boubou/dist-tags",
                    f"{release.REGISTRY}/boubou/{self.version}", self.archive_url]:
            for response in [(b"unavailable", 503), subprocess.TimeoutExpired("curl", 70)]:
                with self.subTest(url=url, response=response):
                    self.responses = {url: response}
                    with self.assertRaises((RuntimeError, subprocess.TimeoutExpired)):
                        self.publish()
                    self.assertEqual(self.commands, [])

    def test_invalid_registry_versions_fail_without_writes(self):
        self.tags["latest"] = "not-a-version"
        with self.assertRaisesRegex(ValueError, "Invalid npm version"):
            self.publish()
        self.assertEqual(self.commands, [])

    def test_wrong_package_identity_or_tarball_metadata_fails_without_writes(self):
        url = f"{release.REGISTRY}/boubou/{self.version}"
        for metadata in [[], {"name": "other", "version": self.version},
                         {"name": "boubou", "version": self.version, "dist": {"tarball": "--help"}}]:
            with self.subTest(metadata=metadata):
                self.responses[url] = (json.dumps(metadata).encode(), 200)
                with self.assertRaises(ValueError):
                    self.publish()
                self.assertEqual(self.commands, [])

    def test_delayed_package_visibility_publishes_only_once(self):
        self.present = False
        metadata = {"name": "boubou", "version": self.version, "dist": {"tarball": self.archive_url}}
        self.responses[f"{release.REGISTRY}/boubou/{self.version}"] = [
            (b"{}", 404), (b"{}", 404), (b"unavailable", 503), (json.dumps(metadata).encode(), 200),
        ]
        self.publish()
        self.assertEqual(sum(command[0] == "publish" for command in self.commands), 1)
        self.assertEqual(self.requests.count(self.archive_url), 1)
        self.assertEqual(self.now, 60)

    def test_tag_permission_failure_is_actionable_and_does_not_republish(self):
        def refuse_tag(command, **kwargs):
            if command[:3] == ["npm", "dist-tag", "add"]:
                raise subprocess.CalledProcessError(1, command)
            return self.run_command(command, **kwargs)

        with patch.object(release.subprocess, "run", refuse_tag):
            with self.assertRaisesRegex(RuntimeError, "Allow npm dist-tag"):
                self.publish()
        self.assertEqual(self.commands, [])

    def test_delayed_tag_visibility_does_not_download_tarball_again(self):
        url = f"{release.REGISTRY}/-/package/boubou/dist-tags"
        stale = (json.dumps(self.tags).encode(), 200)
        updated = (json.dumps({"beta": self.version, "latest": self.version}).encode(), 200)
        self.responses[url] = [stale, stale, stale, (b"unavailable", 503), updated, updated]
        with patch.object(release, "served", wraps=release.served) as served:
            self.publish()
        self.assertEqual(served.call_count, 1)
        self.assertEqual(self.now, 60)
        self.assertEqual(len(self.commands), 1)

    def test_visibility_timeout_is_bounded_and_does_not_republish(self):
        url = f"{release.REGISTRY}/-/package/boubou/dist-tags"
        self.responses[url] = (json.dumps(self.tags).encode(), 200)
        with self.assertRaisesRegex(RuntimeError, "within 30 minutes"):
            self.publish()
        self.assertEqual(self.now, 1800)
        self.assertEqual(len(self.commands), 1)
        self.assertEqual(self.commands[0][0], "dist-tag")


class VersionTests(unittest.TestCase):
    def test_semver_order_includes_numeric_prereleases_and_stable(self):
        versions = ["1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta",
                    "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "1.1.0", "2.0.0"]
        self.assertEqual(sorted(reversed(versions), key=release.version_key), versions)
        self.assertEqual(release.version_key("1.0.0+build.1"), release.version_key("1.0.0+build.2"))

    def test_invalid_versions_are_rejected(self):
        for version in [None, 1, "", "1.0", "v1.0.0", "01.0.0", "1.0.0-beta.01", "1.0.0-", "1.0.0+", "1.0.0\n"]:
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.version_key(version)


if __name__ == "__main__":
    unittest.main()
