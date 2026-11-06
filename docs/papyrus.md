# Papyrus

Papyrus is the small language `ANUBIS` files are written in. It describes
data, not programs: there are no variables, functions or loops. A file is a
list of objects, and everything platform-specific is a `select()` resolved
against the build's mode. The parser lives in `src/papyrus.rs`, and the
mapping onto Rust structs in `src/papyrus_serde.rs`.

## Files and objects

An `ANUBIS` file is a sequence of objects. An object is a type name followed
by `field = value` pairs in parentheses:

```papyrus
# Comments run from '#' to the end of the line.
cc_binary(
    name = "main",
    lang = "cpp",
    srcs = [RelPath("main.cpp")],
)
```

- Top-level objects are rules. The `name` field makes the rule a target:
  `//<dir>:<name>`, where `<dir>` is the file's directory relative to the
  repo root (the directory holding `.anubis_root`).
- Commas between fields and list items are optional, and trailing commas are
  fine.
- Objects nest, e.g. `toolchain(c = CcToolchain(...))`.

## Values

| Syntax | Meaning |
|---|---|
| `"text"` | String. There is no backslash escaping, so use `/` in paths. |
| `42`, `-1` | Integer (64-bit signed). No fractions or exponents. A field of a smaller type, e.g. `u8`, rejects values that don't fit. |
| `true` / `false` | Boolean. |
| `[a, b]` | Array. Items may be any value. |
| `{ key = value }` | Map with identifier keys (e.g. a mode's `vars`). |
| `Type(field = value)` | Object. |
| `RelPath("x")` / `RelPaths(["x", "y"])` | Path(s) relative to this file's directory. |
| `glob([...])` | Files matching patterns, relative to this file's directory. |
| `Target("//dir:name")` / `Targets([":name"])` | Target reference(s). `:name` is a target in the same file. |
| `select(...)` / `multi_select(...)` | Pick a value from the mode's variables. |
| `a + b` | Concatenation. |

Integers, booleans and strings are distinct: `5` doesn't fill a string field
and `"5"` doesn't fill an integer field.

### Paths

`RelPath`, `RelPaths` and `glob` resolve to absolute paths under the file's
directory. They must stay there: `..` segments, absolute paths and drive
letters are rejected, so a rule can't reach into another package's files.
Depend on a target in that directory instead.

```papyrus
include_dirs = [RelPath("include")]
srcs = RelPaths(["src/a.cpp", "src/b.cpp"])
```

### glob

```papyrus
srcs = glob(["*.cpp", "src/**/*.cpp"])
srcs = glob(includes = ["**/*.c"], excludes = ["**/*_test.c"])
```

Results are sorted, so command lines (and build cache keys) are stable. A
glob that matches nothing is an error.

## select

`select()` chooses one value based on variables:

```papyrus
flags = select(
    (target_platform, target_arch) => {
        (windows, x64) = ["-target", "x86_64-pc-windows-msvc"],
        (linux, x64)   = ["-target", "x86_64-linux-gnu"],
        (linux | macos, _) = ["-DUNIX"],
        default = [],
    }
)
```

- The inputs in the first parentheses name variables. Each arm lists one
  pattern per input: a value, alternatives separated by `|`, or `_` for any
  value.
- The first matching arm wins. `default` is used only when no arm matches,
  wherever it is listed.
- If nothing matches and there is no `default`, the value is *unresolved*.
  That is an error only when a target using it is built in that mode, so a
  target can leave out platforms it doesn't support.
- Naming a variable the mode doesn't define is an error.

`multi_select()` has the same syntax, but concatenates the values of **every**
matching arm in order. `default` still applies only when nothing matches.
Toolchains use it to layer flags:

```papyrus
defines = multi_select(
    (target_platform, build_type) => {
        (_, debug)   = ["_DEBUG"],
        (_, release) = ["NDEBUG"],
        (windows, _) = ["WIN32_LEAN_AND_MEAN"],
    }
)
```

### Variables

A mode (`mode/ANUBIS`) defines variables in its `vars` map, such as
`target_platform`, `target_arch` and `build_type`. Anubis adds two for the
machine running the build:

- `build_platform`: `windows`, `linux` or `macos`
- `build_arch`: `x64` or `arm64`

Use `target_*` to describe the output and `build_*` to pick tools that run
on this machine.

## Concatenation

`+` joins two values of the same kind:

- Arrays, path lists and target lists append.
- Objects of the same type merge field by field: fields present on both
  sides are concatenated, and the rest are copied. This is how a toolchain
  extends a shared base object.
- A string followed by a path makes a string, e.g. `"-I" + RelPath("inc")`.

Integers and booleans don't concatenate; `1 + 2` is an error, not `3`.

If either side is an unresolved `select`, the result is unresolved too.

```papyrus
compiler_flags = ["-Wall"] + select((build_type) => {
    (release) = ["-O2"],
    default = [],
})
```

## Resolution

Parsing happens once per file. Resolution happens per mode: selects are
evaluated against the mode's variables, paths are made absolute, and
`:name` targets get the file's directory. The resolved object is then
deserialized into the rule's Rust struct, and unknown fields are rejected.
