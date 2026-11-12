import gzip
import hashlib
import io
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "packaging/qwen"))
from build_runner import indexed_archive, tree_digest


class IndexedRunnerTests(unittest.TestCase):
    def test_full_archive_and_individual_ranges_match_the_tree(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            (source / "bin").mkdir(parents=True)
            (source / "bin/python").write_bytes(b"interpreter" * 1000)
            (source / "bin/python").chmod(0o755)
            (source / "bin/python3").symlink_to("python")
            (source / "empty").mkdir()
            # Exercise PAX long paths as well as an empty file.
            (source / ("script" * 25 + ".py")).write_text("print('hello')\n")
            (source / "empty-file").touch()
            archive = root / "runner.tar.gz"
            entries = indexed_archive(source, archive)
            data = archive.read_bytes()
            installed = root / "installed"
            with tarfile.open(archive, "r:gz") as tar:
                tar.extractall(installed, filter="data")
            self.assertEqual(tree_digest(source), tree_digest(installed))
            for entry in entries:
                if entry["kind"] != "file":
                    continue
                offset, size = entry["offset"], entry["compressed_size"]
                member = gzip.decompress(data[offset:offset + size])
                with tarfile.open(fileobj=io.BytesIO(member), mode="r:") as tar:
                    files = tar.getmembers()
                    self.assertEqual([f.name for f in files], [entry["path"]])
                    contents = tar.extractfile(files[0]).read()
                    self.assertEqual(hashlib.sha256(contents).hexdigest(), entry["sha256"])
                    self.assertEqual(len(contents), entry["size"])
            second = root / "again.tar.gz"
            self.assertEqual(entries, indexed_archive(source, second))
            self.assertEqual(data, second.read_bytes())

    def test_links_outside_the_runner_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (root / "outside").write_text("private")
            (source / "link").symlink_to("../outside")
            with self.assertRaisesRegex(ValueError, "Non-relocatable"):
                indexed_archive(source, root / "runner.tar.gz")
