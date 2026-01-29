# Architecture Overview

This document describes how Anubis discovers projects, parses Papyrus configuration, and executes builds. It focuses on the runtime flow of the CLI, configuration/resolution pipeline, rule execution, and extension points.

## Repository Layout
- `src/main.rs`: CLI entrypoint and top-level orchestration for build and toolchain installation commands.
- `src/anubis.rs`: Core state container (`Anubis`), target/path helpers, caches, and build orchestration (`build_single_target`).
- `src/papyrus.rs`, `src/papyrus_serde.rs`: Tokenization, parsing, and Serde integration for the Papyrus DSL used in `ANUBIS` files.
- `src/job_system.rs`: Dependency-aware job scheduler and worker pool implementation.
- `src/rules/`: Built-in rules (`cc_rules.rs`, `nasm_rules.rs`) and shared helpers (`rule_utils.rs`).
- `src/toolchain.rs`, `src/toolchain_db.rs`, `src/zig.rs`: Mode/toolchain data models and helpers for fetching or validating toolchain definitions.
- `mode/ANUBIS`, `toolchains/ANUBIS`: Default Papyrus configuration shipped with the repo.

## CLI Lifecycle
1. **Argument parsing**: `Args`/`Commands` in `src/main.rs` parse subcommands. Logging is initialized immediately using the requested level/format/output.
2. **Environment normalization**: `build` wipes most environment variables (except `RUST_*`) to avoid leaking host settings into builds.
3. **Project discovery**: The current directory is walked upward to locate `.anubis_root`. Its parent becomes the project root shared by all later stages.
4. **Command dispatch**:
   - `build`: Creates a single shared `Anubis` instance for the project, then builds each requested target under the requested mode/toolchain (default `//toolchains:default`).
   - `install-toolchains`: Runs download/setup logic in `install_toolchains.rs` (delegating to `toolchain_db` helpers) to materialize toolchains declared in Papyrus.
5. **Process exit**: Errors are logged and converted to a non-zero exit code; success returns 0.

## Configuration and Resolution Pipeline
- **Targets**: User input like `//path/to/pkg:lib` is parsed into `AnubisTarget`, which can derive the config file (`//path/to/pkg/ANUBIS`) and the target name (`lib`).
- **Papyrus loading**: `Anubis` caches raw and resolved Papyrus values keyed by config path. Raw values come directly from parsing; resolved values expand `glob`, `select`, concatenations, and relative paths before downstream use.
- **Modes and toolchains**: `Mode` objects (e.g., debug/release) and `Toolchain` objects (compiler/linker definitions) are Papyrus objects resolved via the same cache. Toolchains are keyed by `(mode, toolchain)` to allow mode-specific overrides.
- **Rule deserialization**: For each target, the Papyrus object is located, its type name is matched against registered `RuleTypeInfo`, and the rule is deserialized into a concrete Rust type. Rule instances are cached per target for reuse during the build session.

## Job Creation and Execution
1. **Entry job**: `build_single_target` constructs a `JobSystem` and `JobContext` (carrying the `Anubis`, resolved `Mode`, and `Toolchain`), then asks the rule to create its root job via `Rule::create_build_job`.
2. **Dependency graph**: Jobs are added with explicit dependencies; the scheduler tracks blockers/blocked edges. If dependencies fail, dependent jobs are rejected immediately to avoid wasted work.
3. **Execution model**: A worker pool sized to the requested `--workers` (default: physical cores) pulls ready jobs from a channel. Each job returns either success with an artifact or a `Deferred` result that requeues the job once blockers complete.
4. **Results and artifacts**: Job outputs are stored in an in-memory map by ID and can be downcast via `JobArtifact`. Built-in rules use artifacts to chain compile → archive → link stages.
5. **Abort handling**: A shared abort flag allows the system to stop scheduling when failures occur.

## Built-in Rules
- **C/C++ (`rules/cc_rules.rs`)**: Defines binary and static library rules. Jobs compile each source, archive objects when building libraries, and link executables. Supports include paths, flags, dependency references, and per-rule output directories.
- **NASM (`rules/nasm_rules.rs`)**: Compiles assembly sources to objects using configured assembler flags and include paths.
- **Shared helpers (`rules/rule_utils.rs`)**: Utilities to create directories, spawn external tools with logging, and compose output paths relative to the project root and selected mode.

## Toolchains and Modes
- **Mode separation**: Mode names select output/build subdirectories (`.anubis-build/{mode}`, `.anubis-bin/{mode}`) and feed into toolchain resolution.
- **Toolchain lookup**: Toolchains describe compiler/linker paths plus platform metadata (e.g., libc selection). The `toolchain_db` utilities load Papyrus definitions and can fetch missing toolchains when `install-toolchains` is invoked.
- **Zig support**: `src/zig.rs` provides helpers for Zig-based libc/toolchain setups, enabling cross-compilation scenarios defined in Papyrus.

## Logging and Diagnostics
- `src/logging.rs` configures tracing subscribers with simple or JSON-like output, supports span/timing collection, and writes to stdout or a file.
- Macros in `src/util.rs` add contextual errors (`bail_loc`, `anyhow_loc`) and timing spans (`timed_span!`) to improve debuggability during rule execution and job scheduling.

## Build Artifacts and Paths
- Intermediate files are placed under `{project_root}/.anubis-build/{mode}`. Final outputs (executables, static libraries) are written to `{project_root}/.anubis-bin/{mode}`.
- Rule utilities ensure directories are created before invoking external tools and normalize paths for cross-platform compatibility.

## Extending Anubis
- **New rules**: Implement the `Rule` trait, register a `RuleTypeInfo` in `Anubis::new`, and define a Papyrus object type. Use the job system to express dependencies between compile/link steps.
- **New language features in Papyrus**: Extend the lexer/parser in `papyrus.rs`, update Serde conversion in `papyrus_serde.rs`, and add new `Value` variants as needed.
- **New toolchains or modes**: Create Papyrus definitions under `toolchains/ANUBIS` or `mode/ANUBIS`. Toolchains can also be fetched via the install command if supported by `toolchain_db`.
