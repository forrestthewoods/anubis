# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Anubis is a Rust-based build system for C/C++ projects that uses custom configuration files (ANUBIS) written in the Papyrus DSL. It supports cross-platform compilation using LLVM/Clang and Zig toolchains with Windows and Linux targets.

## Build Commands

### Primary Build Command
```bash
cargo build --release
cargo run --release
```

### Testing
```bash
cargo test
```

### Development Build
```bash
cargo build
cargo run
```

## Architecture Overview

### Core Components

- **Anubis Core** (`src/anubis.rs`): Main build orchestrator with caching systems for configurations, toolchains, and build rules
- **Papyrus DSL** (`src/papyrus.rs`): Custom configuration language parser and evaluator for ANUBIS files
- **Job System** (`src/job_system.rs`): Parallel build execution system
- **C++ Rules** (`src/cpp_rules.rs`): C/C++ specific build rule implementations
- **Toolchain** (`src/toolchain.rs`): Cross-platform toolchain configuration and management

### Configuration System

The build system uses ANUBIS configuration files that define:
- **Build targets** (e.g., `cpp_binary`)
- **Toolchains** with platform-specific settings
- **Modes** for different build configurations (win_dev, linux_dev)

Configuration files support:
- `select()` expressions for platform-conditional values
- `glob()` for file pattern matching
- Variable interpolation with mode-specific variables

### Target System

Targets follow the format `//path/to/directory:target_name`:
- `//examples/hello_world:hello_world` - Example C binary
- `//mode:linux_dev` - Linux development mode
- `//toolchains:default` - Default toolchain configuration

### Caching Architecture

The system implements comprehensive caching:
- Raw and resolved configuration caches
- Mode and toolchain caches  
- Rule and job caches
- All keyed by target paths and build contexts

## Toolchain Setup

The project includes embedded toolchains in `toolchains/`:
- **LLVM/Clang** for compilation
- **MSVC** libraries for Windows targets
- **Zig** for cross-compilation and libc dependencies

Cross-compilation is configured through platform-specific include paths, library directories, and compiler flags in `toolchains/ANUBIS`.

## Example Projects

- `examples/hello_world/` - Basic C program with ANUBIS configuration
- `examples/simple_cpp/` - C++ example project

Each example contains:
- Source files (`.c`, `.cpp`)
- ANUBIS configuration file
- Platform-specific build scripts

## Development Notes

- The main entry point hardcodes build targets in `src/main.rs:54-59`
- Supports both Windows (`//mode:win_dev`) and Linux (`//mode:linux_dev`) targets
- Uses Rust's `anyhow` for error handling throughout
- Implements custom serde deserializers for Papyrus configuration values