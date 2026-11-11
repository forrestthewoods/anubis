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
  - `-p, --profile <file>`: Write a Chrome trace (Perfetto, chrome://tracing): worker, job, compile/link/archive and one `subprocess` span per tool run
- `targets`: List targets, e.g. `targets //samples/basic/simple_cpp` (one ANUBIS file) or `targets //samples/...` (recursive)
- `clean`: Delete everything Anubis writes: `.anubis-out`, `.anubis-temp` and the build cache (`.anubis-cache`). To rebuild from a warm cache instead, delete only `.anubis-out`
- `fmt`: Format ANUBIS files (`src/papyrus_fmt.rs`): every one under the project root, or the files given; `--check` lists unformatted files and fails. Works on the token stream, keeps comments and the author's line breaks, sorts only `srcs` and `deps`, and refuses any change that would parse to a different value
- `install-toolchains`: Install the toolchains pinned in `natron.toml` (`--only <name>`, `--dry-run`)
- `run`: Build and run a `cc_binary` (`-m`, `-t`, then `-- args`)

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
| `declared_headers.rs` | Declared headers: every file a compile reads must be declared (`public_headers`/`private_headers`, a `cc_binary`'s or NASM rule's `headers`; required whenever the matching include dirs are set). The sorted names of the headers a compile can see, and whether each exists, go in its cache key (a new, removed, renamed or newly created header is a miss: shadowing, `__has_include`); every tool runs with the workspace root as its working directory; after each compile the `.d` is checked: every file must be the source, a visible declared header, or under the toolchain's `system_include_dirs`; anything else, including files outside the workspace (checked lexically, so not through a symlink's `..`), fails the build. Anubis never reads source or header files. Gap without a sandbox (#209): an undeclared header created after a result was cached can still hit. End-to-end test: `tools/cache_shadow_test.sh` (CI) |
| `zig_rules.rs` | Zig libc extraction rule: `ZigGlibc`. Extracts libc and runtime libraries from Zig for Linux cross-compilation. Mode-independent: modes that resolve the same inputs (triple, glibc version, lang, link-arg filter, Zig path) share one job per build, outputs under `.anubis-out/modeless/<target>/zig_glibc/<inputs id>/` |
| `job_system.rs` | Parallel build execution. Job graph with dependencies, worker thread pool, deferred execution pattern |
| `toolchain.rs` | `Toolchain` and `Mode` structs. Deserializes toolchain configs from Papyrus |
| `build_cache.rs` | Content-addressed build cache (action cache + CAS in `.anubis-cache/`). Remote-ready keys: `ws:`-relative paths, SHA-256 input digests, Natron fingerprints for tools. Two-level keys for C/C++ compiles: the manifest key includes the declared header names (see `declared_headers.rs`), entries hold the digests of the files the `.d` lists |
| `hints.rs` | Scheduling hints: running average of each compile's duration, persisted locally in `.anubis-cache/hints` (MessagePack). The job queue is a priority heap; slowest-known compiles start first, graph/archive/link jobs before all compiles |

### Toolchain Installation

| File | Purpose |
|------|---------|
| `install_toolchains.rs` | `anubis install-toolchains`: runs Natron on the root `natron.toml` |
| `natron/` (workspace crate) | Toolchain installer: LLVM, Zig, NASM, MSVC CRT and Windows SDK from pinned sources (`url`/`github` entries need `sha256`), into a shared CAS with `toolchains/<dir>` links |

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

### Available Variables

Mode variables (defined in `mode/ANUBIS`):
- `target_platform`: `windows`, `linux`
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
)
```

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
Same fields as `cc_static_library` (one `CcLibrary` struct and one build path in `cc_rules.rs`; only the final link step differs). Produces `lib<name>.so` on Linux (soname `lib<name>.so`, `RUNPATH=$ORIGIN`) or `<name>.dll` + import `<name>.lib` on Windows. Static deps are linked into the shared library and not passed on to dependents. Executables get every runtime `.so`/`.dll` they need hard-linked next to them. Windows needs `__declspec(dllexport)`/`dllimport` in the sources (see `samples/basic/shared_cpp`). C++ types and exceptions can cross a module boundary only with a shared C/C++ runtime (`runtime_linkage = "dynamic"`, see below). Linux toolchains compile with `-fPIC` so any object can go into a shared library.
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
Package files into a reproducible `.zip` (deflate, fixed timestamp, mode 0644) or `.7z` (one solid LZMA2 block, no dates or attributes). Entries are sorted and named relative to the `ANUBIS` file's directory, under optional `prefix`; optional `level` is 0-9. One rule per format (`src/rules/archive_rules.rs`, shared via the `ArchiveFormat` trait), since formats have different settings. Need no mode: `anubis build -t //samples/basic/nasm_c:src` writes `.anubis-out/modeless/samples/basic/nasm_c/src/zip/src.zip` (7z: `<name>/7z/<name>.7z`). Packaging other rules' outputs (built in chosen modes) is the planned next step.
```papyrus
archive_zip(
    name = "src",
    srcs = glob(["*.c", "asm/*", "ANUBIS"]),
    prefix = "nasm_c",
    level = 9,
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
│   │   └── nasm_c/         # C calling NASM (nasm_static_library, nasm_objects)
│   └── external/           # Samples requiring external repos
│       └── ffmpeg/         # FFmpeg build (requires cloning FFmpeg)
├── natron.toml             # Toolchain pins for `install-toolchains`
├── .anubis-out/            # Build outputs per mode/target (mostly read-only hard links into the cache)
├── .anubis-temp/           # Scratch files during a build
├── .anubis-cache/          # Build cache: action cache, CAS blobs, stat cache, scheduling hints
└── .anubis_root            # Project root marker
```

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

- The project clears environment variables before builds to ensure clean compilation
- Build outputs go to `.anubis-out/`; the build cache lives in `.anubis-cache/`
- Toolchains are downloaded on first use via `install-toolchains`
- The FFmpeg example in `samples/external/ffmpeg/` is a large real-world test case (ignore the FFmpeg subdirectory for normal development)
- Cross-compilation from Windows to Linux uses Zig's libc headers
- Linux hosts build both Linux (`//mode:linux_dev`) and Windows (`//mode:win_dev`) targets. On case-sensitive hosts Natron adds case-alias symlinks to the MSVC CRT and Windows SDK (`natron/src/case_fold.rs`)
- Build cache: compile, archive, link, NASM and `zig_glibc` are cached in `.anubis-cache/`. Any file Anubis writes must be invalidated with `BuildCache::invalidate` (the digest memo is per-run)

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
