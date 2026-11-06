"""Bundle the pinned native Drama-3 service and its licenses beside portable Python."""

import shutil
import subprocess
import tempfile
from pathlib import Path

REVISION = "2e303c62be12c6029392972de90ec221bb653e16"


def install(root):
    with tempfile.TemporaryDirectory() as folder:
        folder = Path(folder)
        source, build = folder / "source", folder / "build"
        subprocess.run(["git", "init", "--quiet", source], check=True)
        subprocess.run(["git", "-C", source, "-c", "fetch.fsckObjects=true", "fetch", "--quiet",
                        "--depth=1", "https://github.com/sgl-project/sglang-omni.git", REVISION], check=True)
        actual = subprocess.check_output(["git", "-C", source, "rev-parse", "FETCH_HEAD"], text=True).strip()
        if actual != REVISION:
            raise ValueError("Unexpected Drama-3 server revision")
        subprocess.run(["git", "-C", source, "checkout", "--quiet", "--detach", REVISION], check=True)
        native = source / "sglang_omni_mlx/native"
        mlx = root / "lib/python3.11/site-packages/mlx"
        subprocess.run(["cmake", "-S", native, "-B", build, "-DCMAKE_BUILD_TYPE=Release",
                        f"-DMLX_DIR={mlx / 'share/cmake/MLX'}", "-DCMAKE_OSX_DEPLOYMENT_TARGET=15.0",
                        "-DCMAKE_OSX_ARCHITECTURES=arm64", "-DCMAKE_BUILD_WITH_INSTALL_RPATH=ON"], check=True)
        subprocess.run(["cmake", "--build", build, "--target", "fishaudio_drama3_server", "--parallel", "4"], check=True)
        output = root / "drama3"
        output.mkdir()
        binary = output / "fishaudio_drama3_server"
        shutil.copy2(build / binary.name, binary)
        for path in [*(mlx / "lib").glob("*.dylib"), mlx / "lib/mlx.metallib"]:
            shutil.copy2(path.resolve(strict=True), output / path.name)
        licenses = output / "licenses"
        licenses.mkdir()
        for name, path in {
            "sglang-omni-APACHE-2.0.txt": source / "LICENSE",
            "mlx-audio-MIT.txt": native / "licenses/fishaudio_drama3-MIT.txt",
            "nlohmann-json-MIT.txt": build / "_deps/nlohmann_json-src/LICENSE.MIT",
            "pcre2-LICENCE.txt": build / "_deps/pcre2-src/LICENCE",
            "civetweb-MIT.txt": build / "_deps/civetweb-src/LICENSE.md",
        }.items():
            shutil.copy2(path, licenses / name)
        subprocess.run(["codesign", "--force", "--sign", "-", binary], check=True)
    return {"drama3_server_revision": REVISION, "drama3_mlx_version": "0.32.3"}
