# Anubis

Anubis is a Rust-based build system for C and C++ projects. It uses a small declarative DSL called **Papyrus** to describe build targets, toolchains, and modes without embedding a general-purpose scripting language. The focus is on reproducible builds, straightforward configuration, and the ability to cross-compile with a single set of checked-in rules.

## Project philosophy

Anubis is intentionally scoped: it aims to be practical and predictable rather than flashy.

- **Readable configuration.** Papyrus files are short, declarative, and live alongside the code they describe so that build intent is visible in the repo.
- **Reproducible builds.** Toolchains are downloaded and versioned, and the build graph is resolved from the project root marked by `.anubis_root`.
- **Cross-compilation as a first-class workflow.** Modes capture platform and architecture choices so the same project can target Windows and Linux from a single host.
- **Small, composable rules.** Targets build on a few core primitives (C/C++ binaries and libraries, NASM objects) instead of a large rule surface.

## Repository layout

- `src/` — Rust implementation of the CLI, parser, scheduler, and rule execution.
- `mode/ANUBIS` — Example mode definitions that bind values like `target_platform` and `target_arch`.
- `toolchains/ANUBIS` — Example toolchain configuration for Clang/LLD with the associated C/C++ runtime bits.
- `samples/` — Sample build projects demonstrating Papyrus configuration: `basic/` contains self-contained projects, `external/` contains projects requiring external repositories (like FFmpeg).

## Getting started

### Build the CLI

```bash
cargo build --release
```

The resulting binary lives at `target/release/anubis`.

### Download toolchains

Anubis can fetch the toolchains described in `toolchains/ANUBIS` so you do not need to manage them manually.

```bash
cargo run --release -- install-toolchains
```

### Build targets

All commands are run from within a directory beneath `.anubis_root` (this file marks the project root).

```bash
# Build a single target with a chosen mode
target/release/anubis build \
  -m //mode:win_dev \
  -t //samples/basic/simple_cpp:simple_cpp

# Build multiple targets and control parallelism
target/release/anubis build \
  -m //mode:linux_dev \
  -t //samples/basic/trivial_cpp:trivial_cpp \
  -t //samples/basic/staticlib_cpp:main \
  -w 8

# Increase log verbosity when diagnosing issues
target/release/anubis build -l debug -m //mode:win_dev -t //samples/basic/simple_cpp:simple_cpp
```

## Authoring Papyrus files

Each directory that defines build targets contains an `ANUBIS` file written in the Papyrus DSL. Targets use the format `//path/to/dir:target_name`, where relative targets can be referenced with `:target_name` inside the same directory.

### Basic binary

```papyrus
cpp_binary(
    name = "simple_cpp",
    srcs = glob(["*.cpp"]),
    deps = [],
)
```

This compiles all `.cpp` sources in the directory into a single executable.

### Static library with public headers

```papyrus
cpp_static_library(
    name = "foo",
    srcs = [RelPath("src/foo.cpp")],
    public_include_dirs = [RelPath("include")],
)
```

Downstream targets inherit the include directory through `public_include_dirs`, keeping transitive dependencies explicit.

### Mixing C, C++, and assembly

More complex projects can combine rule types while keeping platform details in modes and toolchains:

```papyrus
# A C library with private compiler flags and public headers
c_static_library(
    name = "image_core",
    srcs = glob(["image/*.c"]),
    public_include_dirs = [RelPath("include")],
    private_compiler_flags = ["-Wall", "-Wextra"],
)

# Specialized NASM objects assembled with preincluded config
nasm_objects(
    name = "image_asm",
    srcs = RelPaths(["asm/color_convert.asm", "asm/filters.asm"]),
    preincludes = RelPaths(["asm/config.asm"]),
)

# A C++ binary that links everything together
cpp_binary(
    name = "viewer",
    srcs = glob(["app/*.cpp"]),
    deps = [
        ":image_core",
        ":image_asm",
    ],
    compiler_flags = select(
        (target_platform, _) => {
            (windows, _) = ["-DUNICODE"],
            (linux, _) = ["-pthread"],
        }
    ),
)
```

- `select` allows values to vary by mode (e.g., `target_platform` or `target_arch`).
- `RelPath` and `RelPaths` keep paths relative to the current `ANUBIS` file, making directories self-contained.
- `glob` expands file patterns without needing a scripting language.

### Modes and toolchains

Modes and toolchains are defined separately from targets so you can reuse the same build graph across hosts.

```papyrus
# mode/ANUBIS
mode(
    name = "win_dev",
    vars = {
        target_platform = "windows",
        target_arch = "x64",
    }
)

# toolchains/ANUBIS
toolchain(
    name = "default",
    cpp = CcToolchain(
        compiler = select((build_platform, build_arch) => {
            (windows, x64) = RelPath("llvm/x86_64-pc-windows-msvc/bin/clang++.exe")
        }),
        compiler_flags = select((target_platform, target_arch) => {
            (windows, x64) = ["-target", "x86_64-pc-windows-msvc"],
            (linux, x64) = ["-target", "x86_64-linux-gnu"],
        }),
        system_include_dirs = select((target_platform, target_arch) => {
            (windows, x64) = [RelPath("windows_kits/Include/10.0.26100.0/ucrt")],
            (linux, x64) = [RelPath("zig/0.15.2/lib/libc/include/x86_64-linux-gnu")],
        })
    )
)
```

Targets refer to modes via `-m //mode:...` and use a toolchain named `default` by convention.

## Why Anubis might interest you

- You want to describe C/C++ projects with a concise, declarative language without embedding a full scripting runtime.
- You prefer builds that pull their own toolchains rather than depending on host state.
- You need to express cross-platform differences but keep most of the graph shared across targets.
- You want examples of a minimal yet complete build graph (see the `samples/` directory) to adapt to your own codebase.

## License

This project is dual-licensed under MIT and the Unlicense. See `LICENSE-MIT` and `UNLICENSE` for details.
