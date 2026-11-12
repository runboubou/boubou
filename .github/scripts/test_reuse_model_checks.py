import copy
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import reuse_model_checks as checks


class RunnerReuseTests(unittest.TestCase):
    def test_actual_checkout_receipt_is_required_and_ambiguous_logs_are_rejected(self):
        source = "b" * 40
        prefix = f"models\t{checks.CHECKOUT_STEP}\t2026-11-12T14:38:14Z "
        receipt = prefix + "[command]/opt/homebrew/bin/git log -1 --format=%H\n" + prefix + source + "\n"
        self.assertEqual(checks.checkout_source(receipt), source)
        self.assertIsNone(checks.checkout_source("workflow head_sha=" + "a" * 40))
        self.assertIsNone(checks.checkout_source(receipt + receipt))
        self.assertIsNone(checks.checkout_source(receipt.replace(checks.CHECKOUT_STEP, "Untrusted other step")))

    def test_exact_digest_or_only_the_reviewed_legacy_transition_can_reuse(self):
        legacy = {"sources_sha256": "core", "sha256": checks.LEGACY_RUNNER, "url": checks.LEGACY_URL}
        self.assertTrue(checks.compatible_runner(legacy, "full", "core", checks.REUSE_BUILDER, True))
        for change in ({"sources_sha256": "changed-core"}, {"sha256": "changed-archive"}, {"url": "https://example.test/runner"}):
            self.assertFalse(checks.compatible_runner(legacy | change, "full", "core", checks.REUSE_BUILDER, True))
        self.assertFalse(checks.compatible_runner(legacy, "full", "core", "new-builder", True))
        self.assertFalse(checks.compatible_runner(legacy, "full", "core", checks.REUSE_BUILDER, False))
        self.assertTrue(checks.compatible_runner({"sources_sha256": "full"}, "full", "core", "new-builder", False))

    def test_missing_or_failed_published_release_is_not_proof(self):
        build = {"source_commit": "a" * 40, "version": "0.1.0-beta.9"}
        run = {"head_sha": build["source_commit"], "head_branch": "v0.1.0-beta.9", "event": "push",
               "path": ".github/workflows/release.yml", "status": "completed", "conclusion": "success"}
        with patch.object(checks, "available"), patch.object(checks, "api") as api:
            for runs, expected in [([], False), ([run | {"conclusion": "failure"}], False),
                                   ([run | {"head_sha": "b" * 40}], False), ([run], True)]:
                api.return_value = {"workflow_runs": runs}
                self.assertEqual(checks.published_release(build), expected)


class LiquidReuseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.original = {
            "src/main.rs": "fn main() {}\n",
            "src/backend.rs": "fn infer() { run_model(); }\n",
            "src/progress.rs": "fn draw() {}\n",
            "native/audio.m": "void audio(void) {}\n",
            "packaging/qwen/smoke.py": "check_model()\n",
            ".github/workflows/qwen-macos.yml": "name: Qwen Mac runtime\n",
            "Cargo.toml": '[package]\nname="boubou"\nversion="0.1.0-beta.9"\n[dependencies]\nserde="1"\n',
            "Cargo.lock": 'version=4\n[[package]]\nname="boubou"\nversion="0.1.0-beta.9"\n[[package]]\nname="serde"\nversion="1.0.0"\nsource="registry+https://example.test"\n',
        }
        for name, content in self.original.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)
        self.git("init", "-q")
        self.commit()
        self.source = self.git("rev-parse", "HEAD").strip()
        patcher = patch.object(checks, "ROOT", self.root)
        patcher.start()
        self.addCleanup(patcher.stop)

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, text=True)

    def commit(self):
        self.git("add", ".")
        self.git("-c", "user.name=Test", "-c", "user.email=test@example.test", "commit", "-qm", "fixture")

    def test_ui_and_package_version_changes_keep_proof_but_core_and_dependencies_do_not(self):
        baseline = checks.liquid_inputs("HEAD")
        (self.root / "src/progress.rs").write_text("fn draw() { show_elapsed(); }\n")
        for name in ["Cargo.toml", "Cargo.lock"]:
            path = self.root / name
            path.write_text(path.read_text().replace("0.1.0-beta.9", "0.1.0-beta.10"))
        self.commit()
        self.assertEqual(checks.liquid_inputs("HEAD"), baseline)
        for name, content in [
            ("src/backend.rs", "fn infer() { changed_model(); }\n"),
            ("native/audio.m", "void audio(void) { changed_audio(); }\n"),
            ("Cargo.toml", self.original["Cargo.toml"].replace('serde="1"', 'serde="2"')),
            ("Cargo.lock", self.original["Cargo.lock"].replace('version="1.0.0"', 'version="2.0.0"')),
        ]:
            for path, original in self.original.items():
                (self.root / path).write_text(original)
            (self.root / name).write_text(content)
            self.commit()
            self.assertNotEqual(checks.liquid_inputs("HEAD"), baseline, name)

    def test_reuse_requires_a_successful_real_model_step_on_matching_inputs(self):
        # A PR event names its head, but checkout tested a different merge commit.
        run = {"id": 1, "head_sha": "a" * 40, "html_url": "https://example.test/run/1",
               "path": ".github/workflows/liquid-macos.yml", "status": "completed", "conclusion": "success"}
        job = {"conclusion": "success", "steps": [{"name": "Real model conversations, offline", "conclusion": "success"}]}
        with patch.object(checks, "api") as api, patch.object(checks, "job_source", return_value=self.source):
            for conclusion in ["success", "skipped", "failure"]:
                changed = copy.deepcopy(job)
                changed["steps"][0]["conclusion"] = conclusion
                api.side_effect = [{"workflow_runs": [run]}, {"jobs": [changed]}]
                self.assertEqual(checks.liquid(), conclusion == "success")
            api.side_effect = [{"workflow_runs": []}]
            self.assertFalse(checks.liquid())

    def test_qwen_requires_matching_cli_integration_even_when_models_are_reusable(self):
        run = {"id": 2, "head_sha": self.source, "html_url": "https://example.test/run/2",
               "path": ".github/workflows/qwen-macos.yml", "status": "completed", "conclusion": "success"}
        job = {"conclusion": "success", "steps": [{
            "name": "Check offline speech inference through the loopback service", "conclusion": "success"}]}
        candidate = {"name": "candidate", "conclusion": "success"}
        (self.root / "src/progress.rs").write_text("fn draw() { show_elapsed(); }\n")
        for name in ["Cargo.toml", "Cargo.lock"]:
            path = self.root / name
            path.write_text(path.read_text().replace("0.1.0-beta.9", "0.1.0-beta.10"))
        self.commit()
        with patch.object(checks, "api") as api, patch.object(checks, "job_source", return_value=self.source) as source:
            api.side_effect = [{"workflow_runs": [run]}, {"jobs": [job, candidate]}]
            self.assertTrue(checks.qwen())
            api.side_effect = [{"workflow_runs": [run]}, {"jobs": [job, candidate]}]
            source.side_effect = [self.source, None]
            self.assertFalse(checks.qwen(), "Missing package checkout proof must not reuse integration")
            source.side_effect = None
            (self.root / "src/backend.rs").write_text("fn infer() { changed_model(); }\n")
            self.commit()
            api.side_effect = [{"workflow_runs": [run]}, {"jobs": [job, candidate]}]
            self.assertFalse(checks.qwen())
            self.assertEqual(api.call_args.args[0], "actions/runs/2/jobs")

    def test_qwen_smoke_or_workflow_changes_invalidate_integration_receipt(self):
        baseline = checks.qwen_inputs("HEAD")
        for name in ["packaging/qwen/smoke.py", ".github/workflows/qwen-macos.yml"]:
            for path, original in self.original.items():
                (self.root / path).write_text(original)
            path = self.root / name
            path.write_text(path.read_text() + "# changed validation\n")
            self.commit()
            self.assertNotEqual(checks.qwen_inputs("HEAD"), baseline)


if __name__ == "__main__":
    unittest.main()
