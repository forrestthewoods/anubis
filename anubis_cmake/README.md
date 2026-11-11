# anubis-cmake (experimental)

Writes one `CMakeLists.txt` that builds Anubis C/C++ targets with the
consumer's own compiler, so CMake users can `add_subdirectory` (or
`FetchContent`) a project that is built with Anubis. Its own crate so that
Anubis itself knows nothing about CMake.

```bash
cargo build --release -p anubis_cmake
anubis-cmake --namespace superdex \
    -m //mode:linux_dev //mode:linux_release //mode:win_dev //mode:win_release \
    -t //superdex_robotics:superdex_robotics -o anubis/cmake/CMakeLists.txt \
    --system //toolchains:x11_headers
# in CI: same command plus --check, to fail when the file is stale
```

How it works, and what it refuses, is described at the top of
`src/export.rs`. In short:

- Each target gets exactly the compile and link arguments Anubis would give
  it, in Anubis's order (deps' public args first, then its own), as PRIVATE
  properties; libraries also get INTERFACE properties for the consumer's
  targets.
- `cc_static_library`, `cc_shared_library`, `cc_binary`, `ifs_stub_library`
  (as a system library). `zig_glibc` is skipped. `--system <target>` marks
  targets the consumer's system provides. Anything else is an error, and so
  is any path inside the installed toolchains.
- Modes are merged into generator expressions on `target_platform`
  (`$<PLATFORM_ID>`) and `build_type` (`$<CONFIG>`: `debug` is Debug, `release`
  every other config). Other mode variables that differ are an error.
  Configuring for a platform or processor the modes don't cover is an error
  (`<ns>_ALLOW_ANY_PLATFORM` overrides).
- The toolchain settings the code depends on are exported too, ahead of each
  target's own as in Anubis: `-std=`, instruction-set flags (`-msse4.2`,
  `-mavx`, ...), `-fPIC`, and feature defines (`UNICODE`,
  `WIN32_LEAN_AND_MEAN`, `_GNU_SOURCE`, ...). Build preferences come from your
  CMake configuration: optimization and debug info (`CMAKE_BUILD_TYPE`), the C
  runtime (`CMAKE_MSVC_RUNTIME_LIBRARY`), linker flags. Your compiler brings
  its own headers, C library and C++ standard library. The generated file's
  header lists what isn't exported.
- Defines are passed as `-D` options, not COMPILE_DEFINITIONS: CMake sorts
  those, which changes the winner when a macro is defined twice.
- GCC and Clang (including clang-cl and AppleClang). cl.exe is refused.
- Needs CMake 3.21 or newer.

## For consumers

- `add_subdirectory(<dir> EXCLUDE_FROM_ALL)` builds only what you link;
  without it every exported executable builds too.
- Target names are `<ns>::<name>`, with the directory added when two exported
  targets share a name, so adding a target can rename another.
- No `install()` rules: you can't `install(EXPORT)` a target that links an
  exported one.
- On Windows, only the exported executables get their DLLs copied next to
  them; copy `$<TARGET_RUNTIME_DLLS:yours>` for your own.

## Tests

`cargo test -p anubis_cmake`: unit tests, a golden file for the fixtures
(`tests/fixtures/CMakeLists.golden.txt`, `UPDATE_GOLDEN=1` rewrites it), and an
end-to-end test that exports `tests/fixtures` plus some samples, builds them
with a pinned CMake and Ninja and checks each fixture prints what an Anubis
build prints (`tests/fixtures/expected.txt`), and that every fixture source
compiles with the same flags, defines and include dirs as under Anubis, apart
from the toolchain settings left to CMake (Linux, with
`ANUBIS_CMAKE_COMPARE_ANUBIS=1`). Install the pinned tools with

```bash
cargo run --release -p natron -- -c anubis_cmake/natron.toml install
```

`ANUBIS_CMAKE_COMPARE_ANUBIS=1` also builds the fixtures with Anubis and checks
its output against the same file. CI: `.github/workflows/cmake-export.yml`,
separate from Anubis's own workflow.
