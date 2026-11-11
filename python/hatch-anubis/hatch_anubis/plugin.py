"""The `anubis` hatchling build hook.

Configured in a member's pyproject.toml:

    [build-system]
    requires = ["hatchling", "hatch-anubis"]
    build-backend = "hatchling.build"

    [tool.hatch.build.hooks.anubis]
    target = "//path/to:wheel"     # a `py_package` in an ANUBIS file

The `py_package` says which Anubis targets ship and where they go in the
wheel. hatchling still writes everything else: METADATA, entry points,
licenses, RECORD and editable installs. The hook only:

1. runs `anubis build -m <mode> -t <target> --outputs-json <tmp>`,
2. adds every file it lists to the wheel,
3. tags the wheel as Anubis says (e.g. `cp312-cp312-win_amd64`, also when
   built on Linux): the tag belongs to the toolchain that built the files,
   not to the Python running this hook.

Environment:
    ANUBIS_MODE         mode to build, `linux_release` or `//mode:linux_release`
                        (default: `linux_release_static` on Linux,
                        `win_release` on Windows). List it in
                        `[tool.uv] cache-keys` so uv rebuilds when it changes.
    ANUBIS_EXE          the anubis binary (default: `anubis` on PATH)
    ANUBIS_SKIP_NATIVE  `1` makes editable builds skip native code. `anubis run`
                        sets it, because it copies native files into the venv
                        itself (much faster than re-zipping them on every change).
"""

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

def _find_anubis_root(start):
    for d in [start, *start.parents]:
        if (d / ".anubis_root").is_file():
            return d
    raise RuntimeError(
        f"hatch-anubis: no .anubis_root above {start}. Build wheels from the "
        "source tree (`uv build --wheel`), not from an sdist."
    )


def _default_mode():
    # Linux wheels link libc++ statically into each module (decided Nov 11
    # 2026): nothing of Anubis's own to ship. Windows uses the CRT DLLs.
    return "//mode:win_release" if sys.platform == "win32" else "//mode:linux_release_static"


class AnubisBuildHook(BuildHookInterface):
    PLUGIN_NAME = "anubis"

    def initialize(self, version, build_data):
        if self.target_name != "wheel":
            return
        if version == "editable" and os.environ.get("ANUBIS_SKIP_NATIVE") == "1":
            return

        target = self.config.get("target")
        if not target:
            raise RuntimeError('hatch-anubis: set `target = "//path:py_package"` in [tool.hatch.build.hooks.anubis]')

        mode = os.environ.get("ANUBIS_MODE") or self.config.get("mode") or _default_mode()
        if not mode.startswith("//"):
            mode = f"//mode:{mode}"
        anubis = os.environ.get("ANUBIS_EXE") or shutil.which("anubis")
        if not anubis:
            raise RuntimeError("hatch-anubis: anubis not found; put it on PATH or set ANUBIS_EXE")
        root = _find_anubis_root(pathlib.Path(self.root).resolve())

        with tempfile.TemporaryDirectory() as tmp:
            out_json = pathlib.Path(tmp) / "outputs.json"
            cmd = [anubis, "build", "-m", mode, "-t", target, "--outputs-json", str(out_json)]
            self.app.display_info(f"hatch-anubis: {' '.join(cmd)}")
            subprocess.run(cmd, cwd=root, check=True)
            package = json.loads(out_json.read_text())[0].get("package")
        if package is None:
            raise RuntimeError(f"hatch-anubis: {target} is not a py_package")

        for f in package["files"]:
            build_data["force_include"][f["path"]] = f"{package['native_dir']}/{f['dest']}"
        build_data["pure_python"] = False
        build_data["tag"] = package["wheel_tag"]
