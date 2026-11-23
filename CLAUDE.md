# CLAUDE.md

This file provides guidance to Claude Code when working with code in this repository.

## Project Overview

Anubis is a Rust-based build system for C/C++ projects. It uses a custom DSL called **Papyrus** for configuration files (named `ANUBIS`). The system supports cross-platform compilation using LLVM/Clang toolchains with Windows and Linux targets.

## Build Commands

```bash
# Build the Anubis tool (release mode recommended)
cargo build --release

# Run tests
cargo test

# Build a target using Anubis
cargo run --release -- build -m //mode:win_dev -t //samples/basic/simple_cpp:simple_cpp

# Format every ANUBIS file in the project (run now and then; not checked in CI)
cargo run --release -- fmt

# Install toolchains (LLVM, Zig, MSVC)
cargo run --release -- install-toolchains
cargo run --release -- install-toolchains --only llvm_linux  # One entry from natron.toml
```

### Command-Line Interface

- `build`: Build targets
  - `-m, --mode`: Mode target (e.g., `//mode:win_dev`, `//mode:linux_dev`). Several build every target in each. None builds modeless, which only rules whose `needs_mode()` is false allow (`archive_zip`, `archive_7z`); such a rule can pick a mode per dep with `JobContext::with_mode`
  - `-t, --targets`: Target(s) to build (e.g., `//samples/basic/simple_cpp:simple_cpp`)
  - `-l, --log-level`: Log level (`error`, `warn`, `info`, `debug`, `trace`)
  - `--workers`: Number of parallel workers (default: physical CPU count)
  - `--no-cache`: Skip the build cache and run every action
  - `--stats`: After the build, print worker time by job kind (compiling, linking, ...) and the slowest jobs
  - `--outputs-json <file>`: After the build, write each target's output files (a shared library or extension plus its runtime shared libraries, or an executable plus its runtime shared libraries and `resources`) and mode vars as JSON. For a `py_package`: its files with their destinations, `native_dir` and wheel tag. For a `py_wheel`: the `.whl`
  - `-p, --profile <file>`: Write a Chrome trace (Perfetto, chrome://tracing): worker, job, compile/link/archive and one `subprocess` span per tool run
- `resolve`: Print what targets resolve to (`select`, `glob`, `RelPath`, `+` evaluated) as JSON, for debugging and for tools that read the build without parsing Papyrus: `resolve -m //mode:linux_dev -t //samples/...` prints `[{"mode", "target", "vars", "rule"}]`, where `rule` is the target's fields plus `"type"` (its rule), paths relative to the root with `/`, targets as `//dir:name`. Fields as written, not the effective compile args. A target a pattern found that doesn't resolve in a mode gets `"error"` instead of `"rule"`; a target named directly fails the command. The toolchain is a target like any other (`-t //toolchains:default`). Host-specific (`build_platform`/`build_arch`); a `py_package`'s `mode_vars` derived mode isn't shown (`papyrus::to_json`)
- `targets`: List targets, e.g. `targets //samples/basic/simple_cpp` (one ANUBIS file) or `targets //samples/...` (recursive)
- `clean`: Delete everything Anubis writes: `.anubis-out`, `.anubis-temp` and the build cache (`.anubis-cache`). To rebuild from a warm cache instead, delete only `.anubis-out`
- `gc`: Delete outputs and cache entries no build has used for `--max-age <days>` (default 30); `--dry-run` prints what would go. Also runs by itself at the end of any other command but `clean`, at most once a day, reporting on stderr (`src/gc.rs`)
- `fmt`: Format ANUBIS files (`src/papyrus_fmt.rs`): every one under the project root, or the files given; `--check` lists unformatted files and fails. Works on the token stream, keeps comments and the author's line breaks, never reorders anything (list order matters: `deps` order is `-I` and link order), and refuses any change that would parse to a different value
- `install-toolchains`: Install the toolchains pinned in `natron.toml` (`--only <name>`, `--dry-run`)
- `run`: Build and run a `cc_binary` or `py_env` (`-m`, `-t`, then `-- args`). The program gets the user's environment

### Windows Git Bash Path Fix

**IMPORTANT:** When running in Git Bash on Windows, the shell converts `//` paths to `/`, breaking Anubis target paths. Always prefix Anubis commands with `MSYS_NO_PATHCONV=1`:

```bash
# CORRECT - Git Bash on Windows:
MSYS_NO_PATHCONV=1 cargo run --release -- build -m //mode:win_dev -t //samples/basic/simple_cpp:simple_cpp

# WRONG - Will fail because // becomes /:
cargo run --release -- build -m //mode:win_dev -t //samples/basic/simple_cpp:simple_cpp
```

This prefix is only needed for commands containing `//` target paths. Standard cargo commands (`cargo build`, `cargo test`, `install-toolchains`) don't need it.

## Source Code Architecture (`src/`)

### Core Files

| File | Purpose |
|------|---------|
| `main.rs` | CLI entry point using `clap`. Parses args, initializes logging, dispatches to `build` or `install-toolchains` |
| `anubis.rs` | Core orchestrator. Contains `Anubis` struct with caches, `AnubisTarget` for target paths, `Rule` trait, and `build_single_target` function |
| `papyrus.rs` | Papyrus DSL lexer/parser using `logos`. Parses ANUBIS config files into `Value` AST. Handles `glob()`, `select()`, `RelPath()`, concatenation (`+`) |
| `papyrus_serde.rs` | Custom serde deserializer for Papyrus `Value` types into Rust structs |
| `papyrus_tests.rs` | Unit tests for Papyrus parsing and resolution |

### Build System

| File | Purpose |
|------|---------|
| `cc_rules.rs` | C/C++ build rules: `CcBinary`, `CcStaticLibrary`. Implements compilation, linking, and archiving. Handles dependencies, include dirs, compiler flags |
| `declared_headers.rs` | Declared headers: every file a compile reads must be declared (`public_headers`/`private_headers`, a `cc_binary`'s or NASM rule's `headers`; required whenever the matching include dirs are set). The sorted names of the headers a compile can see, and whether each exists, go in its cache key (a new, removed, renamed or newly created header is a miss: shadowing, `__has_include`); every tool runs with the workspace root as its working directory; after each compile the `.d` is checked: every file must be the source, a visible declared header, under the toolchain's `system_include_dirs`/`system_framework_dirs`, or one of its `system_files` (macOS: the SDK's `SDKSettings.json`, which clang 23 lists); anything else, including files outside the workspace (checked lexically, so not through a symlink's `..`), fails the build. Anubis never reads source or header files. Gap without a sandbox (#209): an undeclared header created after a result was cached can still hit. End-to-end test: `tools/cache_shadow_test.sh` (CI) |
| `lib_overlay.rs` | Case-insensitive library lookup for Windows links on case-sensitive hosts. When the toolchain sets `case_insensitive_library_dirs` (true off Windows), a link lists its library dirs (names only) and passes lld-link `/vfsoverlay:` with `.anubis-out/lib_overlay/<content hash>.yaml` (paths in it are workspace-relative, so the PDB names libraries that way), so `#pragma comment(lib, "Ole32.lib")` finds `Ole32.Lib`. One file per distinct listing; different files whose names differ only by case are left out (found by exact name only). Libraries only a `#pragma` names are not in the link's cache key (#230) |
| `stage.rs` | Staged directories. An executable links into `<target>/link/`; `<target>/bin/` is staged from the link outputs plus every runtime `.so`/`.dll`. An entry is a hard link to a build output, a copy of a source file or generated bytes, at a relative path (`macos_app` bundles use the last two). A stage is keyed on (name, source, source digest) per file, stamped in `<target>/bin.stage`: a matching key with every file present leaves `bin/` untouched, anything else deletes `bin/` and hard-links it again, so a removed dependency's library disappears. Without a cache (`--no-cache`) it always restages |
| `zig_rules.rs` | Zig libc extraction rule: `ZigGlibc`. Extracts libc and runtime libraries from Zig for Linux cross-compilation. Mode-independent: modes that resolve the same inputs (triple, glibc version, lang, link-arg filter, Zig path) share one job per build, outputs under `.anubis-out/modeless/<target dir>/zig_glibc/<inputs id>/` |
| `job_system.rs` | Parallel build execution. Job graph with dependencies, worker thread pool, deferred execution pattern |
| `toolchain.rs` | `Toolchain` and `Mode` structs. Deserializes toolchain configs from Papyrus |
| `build_cache.rs` | Content-addressed build cache (action cache + CAS in `.anubis-cache/`). Remote-ready keys: `ws:`-relative paths, SHA-256 input digests, Natron fingerprints for tools. Two-level keys for C/C++ compiles: the manifest key includes the declared header names (see `declared_headers.rs`), entries hold the digests of the files the `.d` lists, hashed after the compile; a result is not stored if one of those files has an mtime/ctime at or after the compile's start (an edit while it ran) |
| `gc.rs` | `anubis gc`. A cached output is one file with a name in `.anubis-cache/cas` and a hard link per place it sits in `.anubis-out`, so both sides are aged by one rule. Last use is a stamp: `<target dir>/.anubis-used`, touched by `Anubis::out_dir`, and each action cache entry's own mtime, touched by `get_ac`; both at most once an hour. gc deletes target directories and entries whose stamp is older than the limit (plus that hour), then every blob no remaining entry names (and older than an hour, for builds in flight), stale `tmp/` files and stat cache entries of deleted files. Blobs and outputs are never stamped (shared inode; the stat cache reads mtime). Deletes are best-effort (files in use on Windows are skipped). Anything deleted is a cache miss later, never a wrong build. No lock: a build racing gc can at worst fail once on something unused for the whole limit |
| `hints.rs` | Scheduling hints: running average of each compile's duration, persisted locally in `.anubis-cache/hints` (MessagePack). The job queue is a priority heap; slowest-known compiles start first, graph/archive/link jobs before all compiles |

### Toolchain Installation

| File | Purpose |
|------|---------|
| `install_toolchains.rs` | `anubis install-toolchains`: runs Natron on the root `natron.toml` |
| `natron/` (workspace crate) | Toolchain installer: LLVM, Zig, NASM, MSVC CRT and Windows SDK from pinned sources (`url`/`github` entries need `sha256`, `git` entries a full commit hash, `msvc`/`windows_sdk` the full commit hash of their manifest snapshot as `manifest_commit`), into a shared CAS with `toolchains/<dir>` links |

### Utilities

| File | Purpose |
|------|---------|
| `error.rs` | Macros: `bail_loc!`, `anyhow_loc!`, `bail_loc_if!` for errors with file/line/function info |
| `util.rs` | `SlashFix` trait for normalizing path separators to forward slashes |
| `logging.rs` | Configurable logging via `tracing`. `LogConfig`, `timed_span!` macro for performance timing |

## Papyrus DSL Syntax

ANUBIS configuration files use the Papyrus DSL:

### Basic Syntax

```papyrus
# Comments start with #

rule_type(
    name = "target_name",
    field = "string_value",
    count = 4,          # integers (i64); no floats
    enabled = true,     # booleans
    array = ["item1", "item2"],
    map = { key1 = "value1", key2 = "value2" },
)
```

### Built-in Functions

**`glob()`** - File pattern matching:
```papyrus
srcs = glob(["*.cpp", "src/**/*.cpp"])

# With excludes:
srcs = glob(
    includes = ["*.cpp", "**/*.cpp"],
    excludes = ["*_test.cpp"]
)
```

**`select()`** - Platform-conditional values:
```papyrus
flags = select(
    (target_platform, target_arch) => {
        (windows, x64) = ["-target", "x86_64-pc-windows"],
        (linux, x64) = ["-target", "x86_64-linux-gnu"],
        (linux | macos, _) = ["-DUNIX"],  # Use | for OR, _ for any
        default = []
    }
)
```

**`RelPath()`** and `RelPaths()`** - Relative paths (resolved from config file location):
```papyrus
include_dir = RelPath("include")
files = RelPaths(["src/a.cpp", "src/b.cpp"])
```

**Concatenation (`+`)** - Combine arrays or objects:
```papyrus
flags = ["-Wall"] + ["-O2"] + select((platform) => { default = [] })
```

### Shared Values

A top-level `name = value` names a value for the rest of the file; a bare `name` wherever a value goes stands for it (`toolchains/ANUBIS` uses this for what the C and C++ toolchains have in common):
```papyrus
common_flags = [ "-Wall" ] + select((build_type) => { (debug) = [ "-O0" ], (release) = [ "-O2" ] })

cc_binary(name = "a", compiler_flags = common_flags + [ "-DA" ], ...)
cc_binary(name = "b", compiler_flags = common_flags, ...)
```
Data, not code: the parser copies the value in at each use, still unresolved, so a `select`, `glob` or `RelPath` in it behaves as if written there. A value must be defined above its uses (so no cycles) and only once; it is not a target, and other files can't see it; one that nothing uses is an error (usually a field outside its rule's parentheses). It stands for a whole value, not the list inside `glob(...)`/`RelPaths(...)`/`Targets(...)`. There are no parameters: two values that differ in the middle share only their common head or tail. Details: `docs/papyrus.md`.

### Available Variables

Mode variables (defined in `mode/ANUBIS`):
- `target_platform`: `windows`, `linux`, `macos` (experimental: `//mode:macos_dev`/`macos_release`, arm64 only)
- `target_arch`: `x64`, `arm64`
- `build_type`: `debug`, `release`
- `runtime_linkage`: `dynamic` (default modes) or `static` (the `*_static` modes). Which C/C++ runtime the toolchain links:
  - `dynamic`: Windows compiles with `-fms-runtime-lib=dll`/`dll_dbg` and the toolchains' `exe_deps` add `//toolchains:msvc_crt` (and `msvc_cxx_crt` for C++), `cc_prebuilt_library` targets holding the CRT import libs and DLLs, so the DLLs are staged next to executables like any shared library's. Linux links `//toolchains:anubis_cxx` (`libanubis_cxx.so`: libc++ + libc++abi) and `//toolchains:anubis_unwind` (`libanubis_unwind.so`; prefixed so they can't collide with a user's or the system's `libunwind.so`), built from Zig's sources as ordinary `cc_shared_library` targets in `toolchains/ANUBIS`, since Zig's own `libc++.a` hides every symbol. A toolchain's `exe_deps` skip the target being built, so `anubis_cxx` can be built by the toolchain it belongs to. A C target that links a C++ static library links with the C++ toolchain's `exe_deps`, so it gets the C++ runtime. Every module shares one heap and one typeinfo, so `std::string` and exceptions cross `.so`/`.dll` boundaries. Debug builds stage the debug CRT DLLs, which Microsoft doesn't allow redistributing: shipping release builds is the user's job, not Anubis's.
  - `static`: libcmt(d) on Windows, Zig's static `libc++.a` on Linux, linked into each module. Self-contained executables, but each `.dll`/`.so` gets its own runtime copy, so keep C-style APIs between modules.
  - Every mode must define it, since the toolchain `select`s on it.

Auto-injected build-platform variables (the machine running the build):
- `build_platform`: Current OS (`windows`, `linux`, `macos`)
- `build_arch`: Current architecture (`x64`, `arm64`)

## Rule Types

### `mode`
Defines build configurations with variables:
```papyrus
mode(
    name = "win_dev",
    vars = {
        target_platform = "windows",
        target_arch = "x64",
    }
)
```

### `toolchain`
Configures compiler, linker, flags, includes:
```papyrus
toolchain(
    name = "default",
    cpp = CcToolchain(
        compiler = RelPath("llvm/bin/clang.exe"),
        archiver = RelPath("llvm/bin/llvm-ar.exe"),
        compiler_flags = ["-nostdinc", ...],
        system_include_dirs = [...],
        library_dirs = [...],
        libraries = [...],
        defines = [...]
    )
)
```

### `cpp_binary`
Builds an executable:
```papyrus
cpp_binary(
    name = "my_app",
    srcs = glob(["src/*.cpp"]),
    deps = ["//libs/mylib:mylib"],
    compiler_flags = ["-O2"],
    compiler_defines = ["DEBUG"],
    include_dirs = [RelPath("include")],
    headers = glob(["include/**/*.h"]),  # required with include_dirs; see declared_headers.rs
    libraries = ["user32.lib"],
    library_dirs = [RelPath("lib")],
    resources = glob(["assets/**/*"]), # run-time files, staged next to the exe
)
```
`resources` (Buck's name) are files the program reads at run time: hard-linked into its `bin/` dir at their path relative to the ANUBIS file's directory (`assets/cube.obj` -> `bin/assets/cube.obj`), and listed by `--outputs-json`, so wheels and `py_env` ship them too.

### `cpp_static_library`
Builds a static library:
```papyrus
cpp_static_library(
    name = "mylib",
    srcs = glob(["src/*.cpp"]),
    deps = [],

    # Public (exposed to dependents):
    public_compiler_flags = [],
    public_defines = [],
    public_include_dirs = [RelPath("include")],
    public_headers = glob(["include/**/*.h"]),  # transitive, like public_include_dirs
    public_libraries = [],
    public_library_dirs = [],

    # Private (only for this library):
    private_compiler_flags = [],
    private_defines = [],
    private_include_dirs = [],
    private_headers = [],
)
```
Header lists are required whenever the matching include dirs are set (`[]` if there are none). Every file a compile reads must be the source, a visible declared header (its own target's, or a dep's `public_headers`), or under the toolchain's `system_include_dirs`; otherwise the build fails. Generated headers that are committed via `deploy_into_workspace` are declared like any other.

### `cc_shared_library`
Same fields as `cc_static_library` (one `CcLibrary` struct and one build path in `cc_rules.rs`; only the final link step differs). Produces `lib<name>.so` on Linux (soname `lib<name>.so`, `RUNPATH=$ORIGIN`) or `<name>.dll` + import `<name>.lib` on Windows; optional `output_name` replaces `<name>` there (a `select()` gives a mode its own soname, e.g. superdex's double-precision `libmochi_physics_double.so`). Static deps are linked into the shared library and not passed on to dependents. Executables get every runtime `.so`/`.dll` they need hard-linked next to them in a staged `bin/` (see `stage.rs`). Windows needs `__declspec(dllexport)`/`dllimport` in the sources (see `samples/basic/shared_cpp`). C++ types and exceptions can cross a module boundary only with a shared C/C++ runtime (`runtime_linkage = "dynamic"`, see below). Linux toolchains compile with `-fPIC` so any object can go into a shared library.
```papyrus
cc_shared_library(
    name = "greet",
    lang = "cpp",
    srcs = [RelPath("src/greet.cpp")],
    public_include_dirs = [RelPath("include")],
    public_headers = [RelPath("include/greet.h")],
    private_defines = ["GREET_BUILDING"],
    deps = Targets([":util"]),
)
```

### `cc_prebuilt_library`
A library Anubis doesn't build (e.g. the MSVC CRT, a vendor SDK). Dependents link `libraries` (import libs, static libs, `.so` files), compile with its `public_include_dirs`/`public_defines`, and get `runtime_files` (`.dll`/`.so`) staged next to their executables, the same path as a `cc_shared_library`. Files are checked to exist.
```papyrus
cc_prebuilt_library(
    name = "vendor",
    libraries = RelPaths(["lib/vendor.lib"]),
    runtime_files = RelPaths(["bin/vendor.dll"]),
    public_include_dirs = [RelPath("include")],
    public_headers = glob(["include/**/*"]),
)
```

### macOS targets (experimental)
`//mode:macos_dev` / `//mode:macos_release` build arm64 Mach-O from any host: clang plus `ld64.lld` invoked directly (`-arch arm64 -platform_version macos 13.3 <sdk> -syslibroot`), the system libc++, and a macOS SDK that Natron's `apple_sdk` provider unpacks from Apple's Command Line Tools package (`natron.toml` `macos_sdk`). The pin is the sha256 of the package's Payload, which survives Apple re-publishing the package under new URLs; if every listed URL fails, Natron searches Apple's software-update catalog for a package with that Payload. `natron apple-sdk list` shows available SDK packages, `natron apple-sdk pin <url>` prints the options for one. Shared libraries are `lib<name>.dylib` (install name `@rpath/lib<name>.dylib`, `-rpath @loader_path`). `frameworks` (`cc_binary`) / `public_frameworks` (libraries) link `-framework X`; the toolchain's `system_framework_dirs` are `-iframework` dirs and allowed by the `.d` check. With `dsymutil` set, each link also writes `<output>.dSYM` (an executable's is staged into `bin/` next to it). `//toolchains:darwin_builtins` builds compiler-rt's `os_version_check.c` (for `__builtin_available`) and is in every macOS link's `exe_deps`. Cache keys include the Natron install of any path argument (e.g. `-syslibroot`), since the linker reads SDK files no `.d` lists. Objective-C: `.m` sources compile with the C toolchain in any target, `.mm` (Objective-C++) needs `lang = "cpp"`; both get the toolchain's `objc_flags` (`-fobjc-arc`). macOS-only samples live in `samples/macos/` (`objc`: `.m` + `.mm`, Foundation, Metal). NASM is x86-only, so NASM rules fail in arm64 modes: build the `:main` targets, not `//samples/basic/...`, for macOS. With `deploy_mode = "copy"` the cache key takes the SDK's fingerprint from `toolchains/.natron-state.toml`, so hand-edits to a copied SDK aren't seen.

### `macos_app`
An application bundle around a `cc_binary` (macOS modes only; `src/rules/apple_rules.rs`): `<name>.app/Contents/{Info.plist, PkgInfo, MacOS/<exe + dylibs>, Resources/<resources relative to the ANUBIS dir>}`, a staged directory (`stage.rs`: binaries are hard links to the build outputs, resources are copies; a rebuild with no changes touches nothing, any change deletes and restages the bundle, so don't edit or sign it in place). The executable keeps `ld64.lld`'s ad-hoc signature; Developer ID signing and notarization aren't done yet.
```papyrus
macos_app(
    name = "ObjcSample",
    binary = Target(":main"),
    bundle_id = "dev.anubis.samples.objc",
    resources = RelPaths([ "resources/hello.txt" ]),
)
```

### `zig_libc`
Extracts Zig's bundled libc and runtime libraries for cross-compilation:
```papyrus
zig_glibc(
    name = "linux_libc_cpp",
    target = "x86_64-linux-gnu",      # Target triple
    lang = "c++",                      # "c" or "c++"
    glibc_version = "2.28",           # Optional glibc version
    zig_exe = RelPath("zig/0.15.2/bin/windows_x64/zig.exe"),
)
```

Use as a dependency in `cc_binary` to link against libc for Linux cross-compilation:
```papyrus
cc_binary(
    name = "my_linux_app",
    lang = "cpp",
    srcs = ["main.cpp"],
    deps = ["//toolchains:linux_libc_cpp"],  # Links Zig's libc
)
```

### `archive_zip` / `archive_7z`
Package files into a reproducible `.zip` (deflate, fixed timestamp, mode 0644) or `.7z` (one solid LZMA2 block, no dates or attributes). Entries are sorted and named relative to the `ANUBIS` file's directory, under optional `prefix`; optional `level` is 0-9. One rule per format (`src/rules/archive_rules.rs`, shared via the `ArchiveFormat` trait), since formats have different settings. Need no mode: `anubis build -t //samples/basic/nasm_c:src` writes `.anubis-out/modeless/nasm_c_src_<hash>/zip/src.zip` (7z: `<target dir>/7z/<name>.7z`). Packaging other rules' outputs (built in chosen modes) is the planned next step.
```papyrus
archive_zip(
    name = "src",
    srcs = glob(["*.c", "asm/*", "ANUBIS"]),
    prefix = "nasm_c",
    level = 9,
)
```

### `py_extension`
A CPython extension module: same fields as `cc_shared_library` plus optional `module` (defaults to `name`). Output is `<module><ext_suffix>`, with `ext_suffix` from the toolchain's `python = PyToolchain(...)` part (`.cpython-312-x86_64-linux-gnu.so`, `.cp312-win_amd64.pyd`). Every extension depends on the toolchain's `headers` target (`//toolchains:python_headers`: python-build-standalone headers for the target platform, plus `python312.lib` on Windows; Linux leaves Python symbols undefined). On Linux the module exports only `PyInit_<module>` (link version script), like a `.pyd`; everything else, static deps included, stays private. The C/C++ runtime follows the mode's `runtime_linkage` like any shared library: in the default (dynamic) modes the runtime `.so`/`.dll` files (`libanubis_cxx.so`, `vcruntime140.dll`, ...) are runtime files and ship in the wheel; in the `*_static` modes each module carries its own runtime, so `std::` exception types can't be caught across module boundaries. Linux wheels use the static modes for now, Windows wheels the dynamic CRT. Windows debug modes: pyconfig.h turns `_DEBUG` into the debug-Python ABI (`python312_d.lib`), so a raw C-API extension must hide `_DEBUG` around `#include <Python.h>` (see `samples/basic/python_ext/hello.c`; pybind11 does it for its users). Python 3.12 and uv are pinned in `natron.toml` (`python_linux`, `python_windows` installed on every host; `uv_linux`/`uv_windows` on their hosts).
```papyrus
py_extension(
    name = "hello",
    lang = "c",
    srcs = [RelPath("hello.c")],
)
```

`py_package` (`src/rules/py_rules.rs`) describes the native side of one Python package. Anubis never reads a `pyproject.toml`. Its files are the `targets`' outputs (extension modules, shared libraries, executables, their runtime shared libraries and `resources`) minus files with the same name that a `deps` package ships, so a shared library two packages link ships once, in the lower package. `mode_vars` overrides mode variables for the package's targets: they build in a derived mode (`Anubis::derive_mode`: `linux_dev` -> `linux_dev+precision=double`, own toolchain and out dir), so single- and double-precision packages build in one `anubis build` and live in one venv (upstream superdex's `build-fp64/` tree, without a second mode list).
```papyrus
py_package(
    name = "native",
    native_dir = "hello_pkg/_native",      # inside site-packages / the wheel
    targets = Targets(["//samples/basic/python_ext:hello"]),
    deps = Targets(["//other:native"]),    # optional
    mode_vars = { precision = "double" },  # optional
)
```

`py_wheel` (`src/rules/py_wheel.rs`) makes a wheel, as a normal build target: `anubis build -m //mode:linux_release_static -m //mode:win_release -t //samples/python/hello_uv:wheel` writes both wheels (`.anubis-out/<mode>_<hash>/<dirname>_<target>_<hash>/py_wheel/<name>-<version>-<tag>.whl`, listed by `--outputs-json`), so a `win_amd64` wheel is built on Linux. There is no `wheel` command (removed Nov 15: Forrest wants no specialized commands). Two cached actions: (1) **hatchling**, pinned in `natron.toml` with its five dependencies (`PyToolchain.hatchling`, the directories put on `sys.path`), runs under the toolchain's interpreter in `project` with an empty environment and no network, and writes a pure wheel with all the metadata; `srcs` plus `project`'s `pyproject.toml` are its inputs. (2) Anubis writes the final wheel: the same entries plus the package's native files under `native_dir`, the toolchain's tag (`PyToolchain.wheel_tag`, e.g. `cp312-cp312-manylinux_2_28_x86_64`: the toolchain's Python and glibc) in `WHEEL` and the file name, and a new `RECORD`. Anubis never reads `pyproject.toml`; only hatchling projects are supported; the project's backend must know nothing of the native code (no build hook; the `hatch-anubis` hook and `ANUBIS_MODE`/`ANUBIS_EXE`/`ANUBIS_SKIP_NATIVE` were removed Nov 15, plan: https://claude.ai/artifact/NMyHaXhD81A88GaELoM6Ds). Design doc: https://claude.ai/artifact/LBrQt1mjKEkQFFyHtxkVfS, review: https://claude.ai/artifact/L9LnTgstXk3JSgvwfaj4tN
```papyrus
py_wheel(
    name = "wheel",
    package = Target(":native"),       # the py_package
    project = RelPath("."),            # dir with its pyproject.toml
    srcs = glob(["hello_pkg/**/*.py"]),
)
```

`py_env` makes `anubis run` work for Python: `anubis run -m //mode:linux_dev -t //samples/python/hello_uv:py -- hello-greet you` builds its `packages`, runs `uv sync --locked` (editable installs are plain Python; `uv.lock` decides what is installed, so a stale lock is an error fixed with `uv lock`; `--python <Natron interpreter>` only when there's no `.venv` yet), copies each package's files into `<site-packages>/<native_dir>/` (temp file + rename, `MODE` last, file lock; `MANIFEST` lists them as `<sha256>  <name>`, so stale files are deleted and a file whose build-cache digest is unchanged is not copied or read again: delete `MANIFEST` to force a full copy), then `uv run --no-sync -- args` with the user's environment. Packages find `_native` in either layout with `__path__ = pkgutil.extend_path(__path__, __name__)`.
```papyrus
py_env(
    name = "py",
    project = RelPath("."),            # dir with the uv root pyproject.toml
    packages = Targets([":native"]),   # py_packages to put in the venv
    sync_args = [],                    # extra `uv sync` args, e.g. ["--all-packages"]
)
```

### `ifs_stub_library`
Link-only stand-in for a system shared library (Linux's answer to an import lib). The checked-in `.ifs` (made once with `llvm-ifs --input-format=ELF --output-ifs=libX.ifs /path/libX.so.N`) lists the SONAME and symbols; the toolchain's `llvm-ifs` turns it into a stub `.so` at build time. Depend on it like a library; the real `.so` is loaded at run time. No symbol versions, so not for glibc.
```papyrus
ifs_stub_library(
    name = "libX11",
    ifs = RelPath("libX11.ifs"),
)
```

## Target Paths

Targets use the format `//path/to/dir:target_name`:
- `//samples/basic/simple_cpp:simple_cpp` - Example binary target
- `//mode:win_dev` - Windows development mode
- `//toolchains:default` - Default toolchain
- `:relative_target` - Target in same directory

Each directory with targets contains an `ANUBIS` file.

## Directory Structure

```
anubis/
├── src/                    # Rust source code
├── mode/ANUBIS             # Mode definitions (win_dev, linux_dev)
├── toolchains/             # Installed toolchains
│   ├── ANUBIS              # Toolchain configuration
│   ├── llvm/               # LLVM/Clang binaries
│   ├── zig/                # Zig toolchain (for libc)
│   ├── nasm/               # NASM (Windows: nasm.us zip, Linux: Debian package)
│   ├── msvc/               # MSVC CRT headers/libs
│   └── windows_kits/       # Windows SDK
├── samples/
│   ├── basic/              # Self-contained sample projects
│   │   ├── simple_cpp/     # Basic C++ example
│   │   ├── trivial_c/      # Minimal C example
│   │   ├── trivial_cpp/    # Minimal C++ example
│   │   ├── staticlib_cpp/  # Static library example
│   │   ├── nested_staticlib_cpp/  # Diamond dependency example
│   │   ├── shared_cpp/     # Shared libraries (.so / .dll) example
│   │   ├── mixed_c_cpp/    # C program linking a C++ static library
│   │   ├── nasm_c/         # C calling NASM (nasm_static_library, nasm_objects)
│   │   ├── pragma_lib/     # #pragma comment(lib) with mixed casing (lib_overlay.rs)
│   │   ├── ifs_stub/       # Linking through an ifs_stub_library (Linux stub .so)
│   │   └── python_ext/     # CPython extension module (py_extension)
│   ├── python/             # Python packages with Anubis-built natives (`py_env`, `py_wheel`)
│   ├── macos/              # macOS-only samples (objc: Objective-C/C++, frameworks)
│   └── external/           # Samples requiring external repos
│       └── ffmpeg/         # FFmpeg build (requires cloning FFmpeg)
├── tests/                  # Cargo integration tests, plus ANUBIS test cases CI builds (`//tests/...`)
│   ├── header_case/        # Windows header spellings only natron.toml's header_aliases cover (#229)
│   └── path with spaces/   # Spaces in dir, file, include dir and zig_glibc output paths (#196)
├── natron.toml             # Toolchain pins for `install-toolchains`
├── .anubis-out/            # Build outputs, one directory per target per mode (see Output Layout)
├── .anubis-temp/           # Scratch files during a build
├── .anubis-cache/          # Build cache: action cache, CAS blobs, stat cache, scheduling hints
└── .anubis_root            # Project root marker
```

## Output Layout

```
.anubis-out/<mode name>_<hash>/<dirname>_<target name>_<hash>/<label>/<file>
```

`Anubis::out_dir` (`src/anubis.rs`) is the only place that builds these paths. Every target has one directory directly under its mode's directory (`modeless` without a mode), so no target's directory is inside another's and path length doesn't grow with the depth of the repo. `<dirname>` is the last component of the directory holding the target's `ANUBIS` file; the names are for people, the hash of the full label (`//libs/foo:bar` -> `foo_bar_eaa562ef`) makes the directory unique. `<label>` is the rule's step (`cc_build`, `lib`, `link`, `bin`, `nasm`, ...). Per-source intermediates are `<stem>_<hash>.obj` (`.o` for Linux and macOS targets; `Anubis::intermediate_name`), the hash being of the source's path relative to the workspace root. Final artifacts (`main.exe`, `libfoo.so`, `foo.lib`) keep their plain names. Hashes are 8 hex digits of xxh3 over the label or path bytes (`util::path_hash`); everything hashed is relative to the `.anubis_root` directory. Paths can't be typed from memory, but globs work: `.anubis-out/linux_dev_*/trivial_cpp_main_*/bin/main`.

Outputs don't depend on where the workspace is (#272; `tools/relocation_test.sh` in CI builds two copies and compares them byte for byte). Compiles get `-ffile-prefix-map=<root>/=`, `-ffile-compilation-dir=.` and `-gno-codeview-command-line`, so debug info and `__FILE__` say `samples/x/main.cpp` (a program that opens files next to `__FILE__` must run from the workspace root); compile outputs, NASM sources and everything lld-link is given (arguments, the PDB's path in the PE) are named relative to the workspace root, since those tools record paths as given, and every tool's `argv[0]` is relative too (lld-link writes it into PDBs); Zig's libc objects are built with `-fstrip`. Debuggers resolve these paths against their working directory: start them from the workspace root, or map the sources (gdb `directory <root>`, lldb `settings set target.source-map . <root>`, Visual Studio's source and symbol search paths). Not covered: Windows hosts can't set `argv[0]`, so there a PDB still names lld-link's absolute path.

## Job System

The build system uses a parallel job execution model:

1. **Jobs** have IDs, descriptions, and job functions returning `JobFnResult`
2. **Results**: `Success(Arc<dyn JobResult>)`, `Error(anyhow::Error)`, or `Deferred(JobDeferral)`
3. **Deferred Pattern**: Jobs can create child jobs and defer their completion until dependencies finish. Used for compile-then-link workflows.
4. **Worker Pool**: Uses `num_cpus::get_physical()` workers by default
5. **Caching**: Jobs are cached by `(mode, target, substep)` to avoid recompilation

## Key Patterns

### Adding a New Rule Type

1. Define struct in `cc_rules.rs` with `#[derive(Deserialize)]`
2. Implement `Rule` trait with `name()`, `target()`, `build()` methods
3. Implement `PapyrusObjectType` trait
4. Register in `register_rule_typeinfos()`

### Error Handling

Use location-aware macros from `error.rs`:
```rust
bail_loc!("Error message with {}", context);
anyhow_loc!("Create error without returning");
bail_loc_if!(condition, "Conditional bail");
```

### Path Handling

Always normalize paths using `SlashFix`:
```rust
use crate::util::SlashFix;
let normalized = path.slash_fix();  // Converts \ to /
```

## Dependencies

Key crates:
- `clap` - CLI argument parsing
- `logos` - Lexer generator for Papyrus
- `serde` - Serialization/deserialization
- `anyhow` - Error handling
- `tracing` - Logging and diagnostics
- `crossbeam` - Concurrent channels for job system
- `dashmap` - Concurrent hash maps for caches
- `glob` - File pattern matching

## Development Notes

- GitHub issues follow `.claude/skills/issue-workflow/SKILL.md`: triage values live in labels, and `.github/workflows/board-sync.yml` copies them onto the Issue Tracker board. An issue rated `worth: yes` needs a `## Plan` section in its body before it is Triaged. Forrest's feedback on a plan moves the issue to `status: update-plan` (he may drag the card; the sync adopts Status drags): revise the plan from his comments, then back to Triaged
- Every PR description opens with the triage table in `.github/pull_request_template.md` (kind, fixes, severity, size, complexity, confidence, risk, user-visible); the skill defines the fields
- Environment variables: every build tool (compiler, linker, archiver, NASM, Zig, `anubis_cmd` tools) is started with an empty environment (`run_command_verbose`), so a build can't depend on the user's. Anubis itself, and what `anubis run` starts on the user's behalf (their program, uv), keep it. Anubis and Natron decide nothing from a variable; `src/tests/env_guard_tests.rs` fails on any new use outside its allow-list. `natron.toml` expands only a leading `~`
- Build outputs go to `.anubis-out/`; the build cache lives in `.anubis-cache/`
- Toolchains are downloaded on first use via `install-toolchains`
- The FFmpeg example in `samples/external/ffmpeg/` is a large real-world test case (ignore the FFmpeg subdirectory for normal development). `FFmpeg/` is a checkout of the FFmpeg_anubis fork, whose `configure_gen/<platform>_<arch>/` dirs hold each target's generated config (`linux_x64`, `win_x64`, `macos_arm64`); sources differ per `(target_platform, target_arch)` (x86 `.c` + NASM vs aarch64 `.c` + `.S`)
- Cross-compilation from Windows to Linux uses Zig's libc headers
- Linux hosts build both Linux (`//mode:linux_dev`) and Windows (`//mode:win_dev`) targets. On case-sensitive hosts Natron adds case-alias symlinks to the MSVC CRT and Windows SDK (`natron/src/case_fold.rs`); a spelling no rule covers goes in `windows_sdk`'s `header_aliases` in `natron.toml` (part of the install fingerprint; bump the `-cfN` suffix in `msvc.rs`/`windows_sdk.rs` only when `case_fold.rs`'s rules change)
- A build with nothing to do must do nothing: write no file, hash no file, start no tool. The one exception is `anubis gc`'s use stamps (`.anubis-used` files and action cache entry mtimes), rewritten at most once an hour. `tests/noop_build.rs` (CI; `cargo test --release --test noop_build -- --ignored`) builds the samples three times and checks the third; the summary line `ran N tool(s) and M in-process action(s)` is what it reads. New rules: every step that makes new bytes goes through the build cache, a staged directory through `stage.rs`, and a rule takes the file list its dependency reports, never a scan of an output directory. Not there yet (in the test's `KNOWN_FAILURES` or without a sample): `anubis_cmd` (#174), `deploy_into_workspace` (#246)
- Target names must be distinct ignoring case within an ANUBIS file (they name output directories), and not `.`/`..`
- Build cache: compile, archive, link, NASM, `zig_glibc` and `archive_zip`/`archive_7z` are cached in `.anubis-cache/`. Any file Anubis writes must be invalidated with `BuildCache::invalidate` (the digest memo is per-run)

## Common Tasks

**Add a new sample:**
1. Create directory under `samples/basic/` (self-contained) or `samples/external/` (requires external repos)
2. Add source files and `ANUBIS` config
3. Build with `cargo run --release -- build -m //mode:win_dev -t //samples/basic/yourexample:target`

**Debug build issues:**
```bash
cargo run --release -- -l trace build -m //mode:win_dev -t //target:name
```

**Extend toolchain configuration:**
Edit `toolchains/ANUBIS` - add paths, flags, or defines in the appropriate `select()` blocks.
