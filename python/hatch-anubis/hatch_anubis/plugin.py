"""The `anubis` hatchling build hook.

Configured in a member's pyproject.toml:

    [build-system]
    requires = ["hatchling", "hatch-anubis"]
    build-backend = "hatchling.build"

    [tool.hatch.build.hooks.anubis]
    targets = ["//path/to:my_extension"]   # Anubis targets whose outputs ship
    native_dir = "my_pkg/_native"          # where they go inside the wheel
    exclude_targets = ["//dep:extension"]  # optional: files another wheel
                                           # ships (shared libs both link)

hatchling still writes everything else: METADATA, entry points, licenses,
RECORD and editable installs. The hook only:

1. runs `anubis build -m <mode> -t <targets> --outputs-json <tmp>`,
2. adds every output file to the wheel under `native_dir`, except files
   that `exclude_targets` also produce,
3. tags the wheel for the mode's platform (e.g. `cp312-cp312-win_amd64`,
   also when built on Linux).

Environment:
    ANUBIS_MODE         mode to build, `linux_release` or `//mode:linux_release`
                        (default: the host's release mode). List it in
                        `[tool.uv] cache-keys` so uv rebuilds when it changes.
    ANUBIS_EXE          the anubis binary (default: `anubis` on PATH)
    ANUBIS_SKIP_NATIVE  `1` makes editable builds skip native code. `anubis run`
                        sets it, because it copies native files into the venv
                        itself (much faster than re-zipping them on every change).
"""

import json
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

_ARCH = {"x64": "x86_64", "arm64": "aarch64"}


def _find_anubis_root(start):
    for d in [start, *start.parents]:
        if (d / ".anubis_root").is_file():
            return d
    raise RuntimeError(
        f"hatch-anubis: no .anubis_root above {start}. Build wheels from the "
        "source tree (`uv build --wheel`), not from an sdist."
    )


def _default_mode():
    return "//mode:win_release" if sys.platform == "win32" else "//mode:linux_release"


def _platform_tag(vars, manylinux):
    target_platform = vars.get("target_platform")
    arch = vars.get("target_arch", "x64")
    if target_platform == "windows":
        return {"x64": "win_amd64", "arm64": "win_arm64"}[arch]
    if target_platform == "linux":
        return f"manylinux_{manylinux}_{_ARCH[arch]}"
    if target_platform == "macos":
        raise RuntimeError("hatch-anubis: macOS wheels are not supported yet")
    raise RuntimeError(f"hatch-anubis: mode has no known target_platform: {vars}")


class AnubisBuildHook(BuildHookInterface):
    PLUGIN_NAME = "anubis"

    def initialize(self, version, build_data):
        if self.target_name != "wheel":
            return
        if version == "editable" and os.environ.get("ANUBIS_SKIP_NATIVE") == "1":
            return

        targets = self.config.get("targets") or []
        exclude_targets = self.config.get("exclude_targets") or []
        native_dir = self.config.get("native_dir")
        if not targets or not native_dir:
            raise RuntimeError("hatch-anubis: set `targets` and `native_dir` in [tool.hatch.build.hooks.anubis]")

        mode = os.environ.get("ANUBIS_MODE") or self.config.get("mode") or _default_mode()
        if not mode.startswith("//"):
            mode = f"//mode:{mode}"
        anubis = os.environ.get("ANUBIS_EXE") or shutil.which("anubis")
        if not anubis:
            raise RuntimeError("hatch-anubis: anubis not found; put it on PATH or set ANUBIS_EXE")
        root = _find_anubis_root(pathlib.Path(self.root).resolve())

        with tempfile.TemporaryDirectory() as tmp:
            out_json = pathlib.Path(tmp) / "outputs.json"
            cmd = [anubis, "build", "-m", mode, "-t", *targets, *exclude_targets, "--outputs-json", str(out_json)]
            self.app.display_info(f"hatch-anubis: {' '.join(cmd)}")
            subprocess.run(cmd, cwd=root, check=True)
            outputs = json.loads(out_json.read_text())

        excluded = {f for e in outputs[len(targets):] for f in e["files"]}
        outputs = outputs[: len(targets)]
        seen = {}
        for entry in outputs:
            for f in entry["files"]:
                if f in excluded:
                    continue
                name = pathlib.Path(f).name
                if name in seen and seen[name] != f:
                    raise RuntimeError(f"hatch-anubis: two outputs named {name}: {seen[name]} and {f}")
                seen[name] = f
                build_data["force_include"][f] = f"{native_dir}/{name}"

        build_data["pure_python"] = False
        python_tag = self.config.get("python_tag") or f"cp{sys.version_info[0]}{sys.version_info[1]}"
        manylinux = self.config.get("manylinux", "2_28")
        build_data["tag"] = f"{python_tag}-{python_tag}-{_platform_tag(outputs[0]['vars'], manylinux)}"
