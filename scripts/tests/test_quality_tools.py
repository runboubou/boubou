"""Failure-path regressions for the checks that gate contributions."""

from contextlib import redirect_stdout
import io
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import check_repository
import verify


class RepositoryChecks(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def check_file(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        return check_repository.file_errors(self.root, name)

    def test_ordinary_utf8_source_passes(self):
        self.assertEqual([], self.check_file("notes.md", "# Review\nUse caf\u00e9 as sample text.\n".encode()))

    def test_cjk_prose_and_filename_fail(self):
        self.assertTrue(self.check_file("notes.md", "\u4e2d\u6587\n".encode()))
        self.assertTrue(self.check_file("\u4e2d.md", b"English prose\n"))

    def test_conflict_marker_and_private_key_fail_without_printing_contents(self):
        self.assertTrue(self.check_file("source.rs", b"<<<<<<< branch\nlet value = 1;\n"))
        key = "-----BEGIN " + "PRIVATE KEY-----\nprivate-value\n"
        errors = self.check_file("notes.md", key.encode())
        self.assertTrue(errors)
        self.assertNotIn("private-value", "\n".join(errors))

    def test_private_files_and_binary_assets_fail(self):
        for name in (".env", "cache/model.gguf", "recording.wav", "signing.p12"):
            with self.subTest(name=name):
                self.assertTrue(self.check_file(name, b"must not publish\n"))
        self.assertTrue(self.check_file("data.txt", b"binary\0data"))
        self.assertTrue(self.check_file("invalid.txt", b"\xff"))

    def test_symlinks_are_not_followed(self):
        (self.root / "link.md").symlink_to("missing-private-target")
        self.assertTrue(check_repository.file_errors(self.root, "link.md"))

    def test_git_inventory_includes_tracked_ignored_files(self):
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        (self.root / ".gitignore").write_text("*.wav\n", encoding="utf-8")
        (self.root / "tracked.wav").write_bytes(b"synthetic")
        (self.root / "ignored.wav").write_bytes(b"synthetic")
        (self.root / "new.md").write_text("New text\n", encoding="utf-8")
        subprocess.run(["git", "add", "-f", "tracked.wav"], cwd=self.root, check=True)
        names = check_repository.candidate_files(self.root)
        self.assertIn("tracked.wav", names)
        self.assertIn("new.md", names)
        self.assertNotIn("ignored.wav", names)

    def test_workflow_rejects_mutable_actions_and_privilege(self):
        source = "permissions:\n  contents: read\njobs:\n  test:\n    steps:\n      - uses: actions/checkout@main\n        with:\n          persist-credentials: false\n"
        self.assertTrue(check_repository.workflow_errors("quality.yml", source))
        for forbidden in ("pull_request_target", "self-hosted", "write-all", "${{ secrets.TOKEN }}"):
            self.assertTrue(check_repository.workflow_errors("quality.yml", source + forbidden))

    def test_each_checkout_must_disable_credentials_and_inline_permissions_fail(self):
        source = "permissions:\n  contents: read\njobs:\n  test:\n    steps:\n"
        checkout = "      - uses: actions/checkout@" + "a" * 40 + "\n"
        safe = source + checkout + "        with:\n          persist-credentials: false\n"
        self.assertEqual([], check_repository.workflow_errors("quality.yml", safe))
        self.assertTrue(check_repository.workflow_errors("quality.yml", safe + checkout))
        inline = safe.replace("    steps:", "    permissions: {contents: write}\n    steps:")
        self.assertTrue(check_repository.workflow_errors("quality.yml", inline))


class Verification(unittest.TestCase):
    def test_empty_or_undiscoverable_tool_suite_fails(self):
        for filename in (None, "misnamed_regressions.py"):
            with self.subTest(filename=filename), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                tests = root / "scripts/tests"
                tests.mkdir(parents=True)
                if filename:
                    (tests / filename).write_text(
                        "import unittest\nclass Missing(unittest.TestCase):\n"
                        "    def test_hidden(self):\n        self.assertTrue(True)\n",
                        encoding="utf-8",
                    )
                with redirect_stdout(io.StringIO()) as output:
                    with self.assertRaises(verify.VerificationError):
                        verify.run_tool_tests(root)
                self.assertNotIn("PASS:", output.getvalue())

    def test_tool_suite_executes_real_tests_and_rejects_all_skipped(self):
        for skip in (False, True):
            with self.subTest(skip=skip), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                tests = root / "scripts/tests"
                tests.mkdir(parents=True)
                (tests / "test_regression.py").write_text(
                    "import unittest\nclass Sample(unittest.TestCase):\n"
                    + ("    @unittest.skip('fixture')\n" if skip else "")
                    + "    def test_real(self):\n        self.assertEqual(2, 1 + 1)\n",
                    encoding="utf-8",
                )
                with redirect_stdout(io.StringIO()):
                    if skip:
                        with self.assertRaises(verify.VerificationError):
                            verify.run_tool_tests(root)
                    else:
                        self.assertEqual(1, verify.run_tool_tests(root))

    def test_failed_command_is_not_reported_as_pass(self):
        with tempfile.TemporaryDirectory() as directory, redirect_stdout(io.StringIO()) as output:
            with self.assertRaises(verify.VerificationError):
                verify.run_step("failing child", [sys.executable, "-c", "raise SystemExit(7)"], Path(directory))
        self.assertNotIn("PASS:", output.getvalue())

    def test_ignored_only_or_missing_test_results_fail(self):
        for output in ("", "test result: ok. 0 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out;\n"):
            with self.assertRaises(verify.VerificationError):
                verify.test_counts(output)

    def test_multiple_test_binaries_are_counted(self):
        output = "test result: ok. 2 passed; 0 failed; 3 ignored; 0 measured; 0 filtered out;\n"
        output += "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;\n"
        self.assertEqual((3, 3), verify.test_counts(output))

    def test_native_harness_is_only_compiled(self):
        with patch("verify.platform.system", return_value="Darwin"), patch("verify.run_step") as run:
            verify.native_compile(Path("/candidate"))
        self.assertEqual(1, run.call_count)
        self.assertEqual("clang", run.call_args.args[1][0])
        self.assertIn("native/audio_tests.m", run.call_args.args[1])

    def test_native_compile_is_explicitly_skipped_on_linux(self):
        with patch("verify.platform.system", return_value="Linux"), patch("verify.run_step") as run:
            with redirect_stdout(io.StringIO()) as output:
                verify.native_compile(Path("/candidate"))
        run.assert_not_called()
        self.assertIn("NOT RUN", output.getvalue())


if __name__ == "__main__":
    unittest.main()
