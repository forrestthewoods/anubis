---
name: verify-change
description: What to run before asking for review of an Anubis change, and what CI already covers. Use when a change is ready to push or before opening a PR.
---

# Verify a change

CI (`.github/workflows/ci.yml`) runs on every push to every branch, on Linux, Windows and macOS hosts. Push the branch and read the run; don't repeat locally what it does. Locally, run only what gives fast feedback for the change:

| Change touches | Run locally |
|---|---|
| Any Rust | `cargo test --release --workspace` |
| Rules, cache, staging, a new rule | The affected sample in `linux_dev` and `win_dev`, then the same build again: the second must print `ran 0 tool(s) and 0 in-process action(s)` |
| Compiles, headers, cache keys | `tools/cache_shadow_test.sh` |
| `anubis_cmd`, `deploy_into_workspace`, job keys | `cargo test --release --test generated_files -- --ignored` |
| Anything a build writes | `cargo test --release --test noop_build -- --ignored` |
| Papyrus syntax or ANUBIS files | `cargo run --release -- fmt --check` |

Build a sample with:

```bash
cargo run --release -- build -m //mode:linux_dev -m //mode:win_dev -t //samples/basic/trivial_cpp:main
```

## What CI covers (so you don't have to)

- `cargo test` and every `//samples/basic/...` and `//tests/...` target in linux/win dev, release and static modes, then a warm-cache rebuild that must have 0 misses
- the cache shadow test, the no-op build test, `anubis gc`, `anubis clean`, a workspace path with spaces
- the Python sample (`anubis run`, Linux and Windows wheels, the Windows wheel installed on Windows)
- the 3x3 matrix: every host builds `:main` targets for every OS, each OS runs them
- FFmpeg, but only `macos_release`

## What CI does not cover

- FFmpeg for Linux and Windows: build `//samples/external/ffmpeg:ffmpeg` (needs the FFmpeg_anubis checkout in `samples/external/ffmpeg/FFmpeg`) when a change touches compiles, NASM, links or cache keys
- superdex: never in this repo; test against the fork only when asked
- interactive behavior (the TUI, `anubis run` with a terminal): run it by hand

In the PR's Testing section, say which of these ran and on which host.
