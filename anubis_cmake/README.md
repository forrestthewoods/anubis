# anubis-cmake (experimental)

Writes one `CMakeLists.txt` that builds Anubis C/C++ targets with the
consumer's own compiler, so CMake users can `add_subdirectory` (or
`FetchContent`) a project that is built with Anubis. Its own crate so that
Anubis itself knows nothing about CMake.

```bash
cargo build --release -p anubis_cmake
anubis-cmake -m //mode:linux_dev //mode:linux_release //mode:win_dev //mode:win_release \
    -t //superdex_robotics:superdex_robotics -o anubis/cmake/CMakeLists.txt --namespace superdex
# in CI: same command plus --check, to fail when the file is stale
```

What it exports, and what it refuses, is described at the top of
`src/export.rs`. In short:

- `cc_static_library`, `cc_shared_library`, `cc_binary`, `ifs_stub_library`
  (as a system library). `zig_glibc` is skipped. Anything else is an error.
- Modes are merged into generator expressions on `target_platform`
  (`$<PLATFORM_ID>`) and `build_type` (`$<CONFIG>`). Other mode variables
  that differ between the given modes are an error.
- The toolchain is not exported, except its `-std=`. Code that needs a
  toolchain define (e.g. `_GNU_SOURCE`) should state it on its target.
- GCC and Clang (including clang-cl and AppleClang). cl.exe is refused.
