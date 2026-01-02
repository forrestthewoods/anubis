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
cargo run --release -- build -m //mode:win_dev -t //examples/simple_cpp:simple_cpp

# Install toolchains (LLVM, Zig, MSVC)
cargo run --release -- install-toolchains
cargo run --release -- install-toolchains --keep-downloads  # Reuse cached downloads
```

### Command-Line Interface

- `build`: Build targets
  - `-m, --mode`: Mode target (e.g., `//mode:win_dev`, `//mode:linux_dev`)
  - `-t, --targets`: Target(s) to build (e.g., `//examples/simple_cpp:simple_cpp`)
  - `-l, --log-level`: Log level (`error`, `warn`, `info`, `debug`, `trace`)
- `install-toolchains`: Download and install LLVM, Zig, and MSVC toolchains

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
| `zig_rules.rs` | Zig libc extraction rule: `ZigLibc`. Extracts libc and runtime libraries from Zig for Linux cross-compilation |
| `job_system.rs` | Parallel build execution. Job graph with dependencies, worker thread pool, deferred execution pattern |
| `toolchain.rs` | `Toolchain` and `Mode` structs. Deserializes toolchain configs from Papyrus |

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

### `zig_libc`
Extracts Zig's bundled libc and runtime libraries for cross-compilation:
```papyrus
zig_libc(
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

## Target Paths

Targets use the format `//path/to/dir:target_name`:
- `//examples/simple_cpp:simple_cpp` - Example binary target
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
├── examples/
│   ├── simple_cpp/         # Basic C++ example
│   ├── trivial_cpp/        # Minimal example
│   ├── staticlib_cpp/      # Static library example
│   └── ffmpeg/             # Large project example (FFmpeg)
├── .anubis-build/          # Build artifacts (object files)
├── .anubis-out/            # Build outputs (binaries)
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
- Build outputs go to `.anubis-build/` (objects) and `.anubis-out/` (binaries)
- Toolchains are downloaded on first use via `install-toolchains`
- The FFmpeg example in `examples/ffmpeg/` is a large real-world test case (ignore the FFmpeg subdirectory for normal development)
- Cross-compilation from Windows to Linux uses Zig's libc headers

## Common Tasks

**Add a new example:**
1. Create directory under `examples/`
2. Add source files and `ANUBIS` config
3. Build with `cargo run --release -- build -m //mode:win_dev -t //examples/yourexample:target`

**Debug build issues:**
```bash
cargo run --release -- -l trace build -m //mode:win_dev -t //target:name
```

**Extend toolchain configuration:**
Edit `toolchains/ANUBIS` - add paths, flags, or defines in the appropriate `select()` blocks.
