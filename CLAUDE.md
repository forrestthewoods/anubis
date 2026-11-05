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

# Install toolchains (LLVM, Zig, MSVC)
cargo run --release -- install-toolchains
cargo run --release -- install-toolchains --keep-downloads  # Reuse cached downloads
```

### Command-Line Interface

- `build`: Build targets
  - `-m, --mode`: Mode target (e.g., `//mode:win_dev`, `//mode:linux_dev`)
  - `-t, --targets`: Target(s) to build (e.g., `//samples/basic/simple_cpp:simple_cpp`)
  - `-l, --log-level`: Log level (`error`, `warn`, `info`, `debug`, `trace`)
  - `--workers`: Number of parallel workers (default: physical CPU count)
  - `--no-cache`: Skip the build cache and run every action
- `install-toolchains`: Download and install LLVM, Zig, and MSVC toolchains

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
| `zig_rules.rs` | Zig libc extraction rule: `ZigGlibc`. Extracts libc and runtime libraries from Zig for Linux cross-compilation |
| `job_system.rs` | Parallel build execution. Job graph with dependencies, worker thread pool, deferred execution pattern |
| `toolchain.rs` | `Toolchain` and `Mode` structs. Deserializes toolchain configs from Papyrus |
| `build_cache.rs` | Content-addressed build cache (action cache + CAS in `.anubis-cache/`). Remote-ready keys: `ws:`-relative paths, SHA-256 input digests, Natron fingerprints for tools. Two-level keys for C/C++ compiles via the `.d` file |
| `hints.rs` | Scheduling hints: running average of each compile's duration, persisted locally in `.anubis-cache/hints` (MessagePack). The job queue is a priority heap; slowest-known compiles start first, graph/archive/link jobs before all compiles |

### Toolchain Installation

| File | Purpose |
|------|---------|
| `install_toolchains.rs` | Downloads and installs LLVM, Zig, and MSVC/Windows SDK from official sources. Handles SHA256 verification, archive extraction |
| `toolchain_db.rs` | SQLite database (`.anubis_db`) tracking installed toolchains and versions |
| `zig.rs` | Zig libc extraction helpers. Runs Zig compiler with verbose output, parses linker command, extracts and caches libc libraries |

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

Auto-injected host variables:
- `host_platform`: Current OS (`windows`, `linux`, `macos`)
- `host_arch`: Current architecture (`x64`, `arm64`)

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
    public_libraries = [],
    public_library_dirs = [],

    # Private (only for this library):
    private_compiler_flags = [],
    private_defines = [],
    private_include_dirs = [],
)
```

### `cc_shared_library`
Same fields as `cc_static_library` (one `CcLibrary` struct and one build path in `cc_rules.rs`; only the final link step differs). Produces `lib<name>.so` on Linux (soname `lib<name>.so`, `RUNPATH=$ORIGIN`) or `<name>.dll` + import `<name>.lib` on Windows. Static deps are linked into the shared library and not passed on to dependents. Executables get every runtime `.so`/`.dll` they need hard-linked next to them. Windows needs `__declspec(dllexport)`/`dllimport` in the sources (see `samples/basic/shared_cpp`). Windows uses the static CRT, so each DLL has its own heap: don't free memory across a DLL boundary (e.g. return a `std::string`); the debug CRT pops an assertion dialog, which hangs CI. Linux toolchains compile with `-fPIC` so any object can go into a shared library.
```papyrus
cc_shared_library(
    name = "greet",
    lang = "cpp",
    srcs = [RelPath("src/greet.cpp")],
    public_include_dirs = [RelPath("include")],
    private_defines = ["GREET_BUILDING"],
    deps = Targets([":util"]),
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
│   ├── msvc/               # MSVC CRT headers/libs
│   └── windows_kits/       # Windows SDK
├── samples/
│   ├── basic/              # Self-contained sample projects
│   │   ├── simple_cpp/     # Basic C++ example
│   │   ├── trivial_c/      # Minimal C example
│   │   ├── trivial_cpp/    # Minimal C++ example
│   │   ├── staticlib_cpp/  # Static library example
│   │   ├── nested_staticlib_cpp/  # Diamond dependency example
│   │   └── shared_cpp/     # Shared libraries (.so / .dll) example
│   └── external/           # Samples requiring external repos
│       └── ffmpeg/         # FFmpeg build (requires cloning FFmpeg)
├── .anubis-build/          # Build artifacts (object files)
├── .anubis-bin/            # Build outputs (binaries)
├── .anubis_db              # SQLite toolchain database
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
- `rusqlite` - SQLite for toolchain database
- `glob` - File pattern matching

## Development Notes

- The project clears environment variables before builds to ensure clean compilation
- Build outputs go to `.anubis-build/` (objects) and `.anubis-bin/` (binaries)
- Toolchains are downloaded on first use via `install-toolchains`
- The FFmpeg example in `samples/external/ffmpeg/` is a large real-world test case (ignore the FFmpeg subdirectory for normal development)
- Cross-compilation from Windows to Linux uses Zig's libc headers
- Linux hosts build both Linux (`//mode:linux_dev`) and Windows (`//mode:win_dev`) targets. On case-sensitive hosts Natron adds case-alias symlinks to the MSVC CRT and Windows SDK (`natron/src/case_fold.rs`)
- Build cache: compile, archive, link and `zig_glibc` are cached in `.anubis-cache/`. Any file Anubis writes must be invalidated with `BuildCache::invalidate` (the digest memo is per-run)

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
