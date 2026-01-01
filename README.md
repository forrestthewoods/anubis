# Anubis

Anubis is a fast, modern build system for C/C++ projects written in Rust. It uses a custom declarative DSL called **Papyrus** for configuration files and supports cross-platform compilation using LLVM/Clang toolchains.

## Philosophy

Anubis is designed with these principles in mind:

- **Simplicity**: Build configuration should be readable and declarative, not a programming language
- **Speed**: Parallel compilation with efficient job scheduling to minimize build times
- **Portability**: Cross-compile from any platform to Windows or Linux using hermetic toolchains
- **Self-contained**: Toolchains are downloaded and managed automatically - no system dependencies required

## Quick Start

### 1. Build Anubis

```bash
cargo build --release
```

### 2. Install Toolchains

Anubis automatically downloads and manages LLVM/Clang, Zig (for libc), and MSVC headers/libraries:

```bash
cargo run --release -- install-toolchains
```

### 3. Build a Target

```bash
cargo run --release -- build -m //mode:win_dev -t //examples/simple_cpp:simple_cpp
```

## Configuration Files (ANUBIS)

Build configuration is defined in files named `ANUBIS` using the Papyrus DSL. Each directory containing build targets has an ANUBIS file.

### Simple Binary Example

```papyrus
cpp_binary(
    name = "simple_cpp",
    srcs = glob(["*.cpp"]),
    deps = [],
)
```

### Static Library with Dependencies

```papyrus
cpp_binary(
    name = "main",
    srcs = [ RelPath("src/main.cpp") ],
    deps = [
        "//examples/staticlib_cpp:foo",
    ],
)

cpp_static_library(
    name = "foo",
    srcs = [ RelPath("src/foo.cpp") ],
    public_include_dirs = [ RelPath("include") ],
    deps = [],
)
```

### Key Papyrus Features

- **`glob()`** - Pattern matching for source files: `glob(["src/**/*.cpp"])`
- **`RelPath()`** - Paths relative to the ANUBIS file location
- **`select()`** - Platform-conditional values based on target OS/architecture
- **Concatenation (`+`)** - Combine arrays or objects

## Command-Line Usage

```bash
# Build a target
anubis build -m //mode:win_dev -t //path/to:target

# Build multiple targets
anubis build -m //mode:linux_dev -t //app:main -t //lib:core

# Specify worker threads
anubis build -m //mode:win_dev -t //app:main -w 8

# Enable debug logging
anubis build -l debug -m //mode:win_dev -t //app:main

# Install toolchains (reuse cached downloads)
anubis install-toolchains --keep-downloads
```

### Available Modes

- `//mode:win_dev` - Windows x64 development build
- `//mode:linux_dev` - Linux x64 development build

## Target Paths

Targets use the format `//path/to/dir:target_name`:

- `//examples/simple_cpp:simple_cpp` - Binary target in examples/simple_cpp
- `//libs/mylib:mylib` - Library target in libs/mylib
- `:local_target` - Target in the current directory

## Rule Types

### `cpp_binary`

Builds an executable from C++ sources:

```papyrus
cpp_binary(
    name = "my_app",
    srcs = glob(["src/*.cpp"]),
    deps = ["//libs/mylib:mylib"],
    compiler_flags = ["-O2"],
    compiler_defines = ["NDEBUG"],
    include_dirs = [RelPath("include")],
    libraries = ["user32"],
    library_dirs = [RelPath("lib")],
)
```

### `cpp_static_library`

Builds a static library from C++ sources:

```papyrus
cpp_static_library(
    name = "mylib",
    srcs = glob(["src/*.cpp"]),
    deps = [],

    # Public (exposed to dependents):
    public_compiler_flags = [],
    public_defines = [],
    public_include_dirs = [RelPath("include")],

    # Private (only for this library):
    private_compiler_flags = [],
    private_defines = [],
    private_include_dirs = [],
)
```

## Project Structure

```
my_project/
├── .anubis_root          # Marks project root
├── mode/ANUBIS           # Build mode definitions
├── toolchains/ANUBIS     # Toolchain configuration
├── src/
│   ├── ANUBIS            # Build targets for src/
│   └── *.cpp
├── .anubis-build/        # Intermediate build files (objects)
└── .anubis-out/          # Final build outputs (binaries)
```

## License

[Add license information here]
