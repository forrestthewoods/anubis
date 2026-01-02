# Anubis Syntax Highlighting for VSCode

Provides syntax highlighting for ANUBIS build configuration files using the Papyrus DSL.

## Features

- Syntax highlighting for ANUBIS configuration files
- Automatic language detection for files named `ANUBIS`
- Bracket matching and auto-closing
- Comment toggling with `#`

## Highlighted Elements

| Element | Examples |
|---------|----------|
| **Comments** | `# This is a comment` |
| **Strings** | `"hello"`, `"path/to/file.cpp"` |
| **Keywords** | `select`, `default`, `glob`, `includes`, `excludes` |
| **Built-in Functions** | `RelPath()`, `RelPaths()` |
| **Rule Types** | `mode`, `toolchain`, `cc_binary`, `cc_static_library`, `cpp_binary`, `cpp_static_library`, `nasm_objects`, `nasm_static_library` |
| **Type Constructors** | `CcToolchain`, `NasmToolchain` |
| **Constants** | `true`, `false`, `_` (wildcard) |
| **Numbers** | `42`, `-3.14`, `1e10` |
| **Operators** | `=`, `=>`, `+`, `\|` |

## Installation

### From Source (Development)

1. Copy or symlink this folder to your VSCode extensions directory:
   - **Windows**: `%USERPROFILE%\.vscode\extensions\anubis-syntax`
   - **macOS/Linux**: `~/.vscode/extensions/anubis-syntax`

2. Reload VSCode

### Manual Steps

```bash
# Linux/macOS
ln -s /path/to/anubis/tools/anubis-vscode ~/.vscode/extensions/anubis-syntax

# Windows (PowerShell as Admin)
New-Item -ItemType SymbolicLink -Path "$env:USERPROFILE\.vscode\extensions\anubis-syntax" -Target "C:\path\to\anubis\tools\anubis-vscode"
```

## Example

```papyrus
# Build configuration for a C++ binary
cc_binary(
    name = "my_app",
    lang = "cpp",
    srcs = glob(["src/*.cpp"]),
    deps = ["//libs/mylib:mylib"],
    compiler_flags = ["-O2", "-Wall"] + select(
        (target_platform) => {
            (windows) = ["-DWIN32"],
            (linux) = ["-DLINUX"],
            default = [],
        }
    ),
)
```

## File Associations

The extension automatically activates for files named `ANUBIS`. To manually set the language mode:

1. Open the file in VSCode
2. Click on the language indicator in the status bar (bottom right)
3. Select "Papyrus" or "ANUBIS"

## Development

The extension uses TextMate grammars defined in `syntaxes/papyrus.tmLanguage.json`. To modify the syntax highlighting:

1. Edit `syntaxes/papyrus.tmLanguage.json`
2. Reload VSCode window (`Ctrl+Shift+P` → "Developer: Reload Window")

## License

MIT
