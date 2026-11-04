"""Install the local data read by speech-to-speech during startup."""

import hashlib
import io
import subprocess
import tempfile
import zipfile
from urllib.request import urlopen


def install(root):
    nltk = "550b6625bcef1f2abff2ff770a5a0d272c9c6b2a"
    for subdir, name, digest in [
        ("tokenizers", "punkt_tab", "e57f64187974277726a3417ca6f181ec5403676c717672eef6a748a7b20e0106"),
        ("taggers", "averaged_perceptron_tagger_eng", "6025f530624335c67d6547d44757b357b4e79bae030a0383e9887a92c1718f0b"),
    ]:
        url = f"https://raw.githubusercontent.com/nltk/nltk_data/{nltk}/packages/{subdir}/{name}.zip"
        with urlopen(url, timeout=90) as response:
            archive = response.read()
        if hashlib.sha256(archive).hexdigest() != digest:
            raise ValueError(f"Invalid NLTK data: {name}")
        with zipfile.ZipFile(io.BytesIO(archive)) as z:
            z.extractall(root / "nltk_data" / subdir)

    revision = "066409f3d9db45a30d21dfcbf9147826ffdd7052"
    with tempfile.TemporaryDirectory() as tmp:
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=tmp)
        git("init", "--quiet")
        git("-c", "fetch.fsckObjects=true", "fetch", "--quiet", "--depth=1",
            "https://github.com/snakers4/silero-vad.git", revision)
        if git("rev-parse", "FETCH_HEAD").decode().strip() != revision:
            raise ValueError("Unexpected Silero revision")
        names = git("ls-tree", "-r", "--name-only", revision, "src/silero_vad").decode().splitlines()
        for name in ["hubconf.py", "LICENSE", *names]:
            if name.startswith("src/silero_vad/data/") and not name.endswith((".py", "silero_vad.jit")):
                continue
            path = root / "torch/hub/snakers4_silero-vad_master" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(git("show", f"{revision}:{name}"))
        (root / "torch/hub/trusted_list").write_text("snakers4_silero-vad\n")
    return {"nltk_data_revision": nltk, "silero_revision": revision}
