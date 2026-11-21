# natron

Vendor compiler toolchains into source-controlled projects.

natron is a small Rust library + CLI that downloads, caches, and deploys
compiler toolchains (LLVM, Zig, NASM, MSVC, Windows SDK, etc.) declared in
a TOML config file. Toolchains live in a shared content-addressed cache;
each project gets a deploy directory that symlinks into the cache (or holds
real copies, if you want to commit the toolchain into source control).

The name comes from the Egyptian preservation salt — natron preserves
toolchains the way mummification preserved bodies, with rather better
ergonomics.

## Why

If your build depends on `clang-21.1.6` for Windows MSVC builds, you don't
want to depend on whatever happens to be on `$PATH`. natron pins exact
versions in `natron.toml`, fetches them from upstream, and stages them
under `<project>/toolchains/` so the build sees a stable, named tree.

## Usage

```bash
natron install         # install + deploy everything in natron.toml
natron install --dry-run
natron install --only llvm21
natron install --mode copy   # override deploy mode for this run
natron list                  # show what's deployed in this project
natron list --cache          # show every install in the global cache
natron clean --downloads     # empty <cache>/downloads/
natron clean --all --yes     # nuke the whole cache
natron msvc versions         # list Microsoft VS builds available on the mirror
natron msvc packages --build-version 18.6.11819.183
natron msvc extract  --build-version 18.6.11819.183 --out C:\temp\msvc
natron windows_sdk versions  # list Windows SDK versions available on the mirror
natron windows_sdk packages --sdk-version 26100
natron windows_sdk extract  --sdk-version 26100 --out C:\temp\sdk
```

`natron` with no subcommand defaults to `install`.

## Config

A `natron.toml` at your project root:

```toml
[settings]
deploy_dir   = "toolchains"
deploy_mode  = "symlink"      # default; per-toolchain may override
cache_dir    = "~/.cache/natron"  # optional; `~` is the home folder, environment variables are not expanded

[[toolchain]]
name        = "llvm21"
deploy_dir  = "llvm21"
provider    = "github"
[toolchain.options]
repo  = "llvm/llvm-project"
tag   = "llvmorg-21.1.6"
asset = "clang+llvm-21.1.6-x86_64-pc-windows-msvc.tar.xz"
strip_prefix = "clang+llvm-21.1.6-x86_64-pc-windows-msvc"

[[toolchain]]
name        = "nasm"
deploy_dir  = "nasm"
provider    = "url"
[toolchain.options]
url     = "https://www.nasm.us/pub/nasm/releasebuilds/3.01/win64/nasm-3.01-win64.zip"
strip_prefix = "nasm-3.01"

[[toolchain]]
name        = "zig"
deploy_dir  = "zig"
provider    = "url"
[toolchain.options]
url          = "https://ziglang.org/download/0.15.2/zig-x86_64-windows-0.15.2.zip"
strip_prefix = "zig-x86_64-windows-0.15.2"
sha256       = "3a0ed1e8799a2f8ce2a6e6290a9ff22e6906f8227865911fb7ddedc3cc14cb0c"

[[toolchain]]
name        = "msvc"
deploy_dir  = "msvc"
provider    = "msvc"
[toolchain.options]
build_version   = "18.6.11819.183"   # required: exact Microsoft VS build (see `natron msvc versions`)
manifest_commit = "329b52d92875be0aaf5f2258a18f7df5edf95abd"   # required: that build's snapshot

[[toolchain]]
name        = "windows_sdk"
deploy_dir  = "windows_sdk"
provider    = "windows_sdk"
[toolchain.options]
sdk_version     = "26100"            # required: exact Windows SDK build (see `natron windows_sdk versions`)
manifest_commit = "329b52d92875be0aaf5f2258a18f7df5edf95abd"   # required: snapshot to install it from
```

Multiple toolchains of the same provider type are first-class. Vendor
LLVM 18 and LLVM 21 side by side; each gets its own `[[toolchain]]`
block with distinct `name` and `deploy_dir`.

## Built-in providers

- **`url`**: download a fixed URL (http/https/file). Required `url`, `sha256`.
  Optional `archive` (inferred from URL filename), `strip_prefix`, `flatten`.
- **`github`**: download a GitHub release asset. Required `repo`, `tag`,
  `asset`, `sha256`. Optional `version` (display), `archive`, `strip_prefix`.
- **`git`**: install one commit of a git repository, for sources with no
  release archives. Required `repo` and `commit` (the full 40-hex hash, which
  pins the content the way `sha256` pins a download). Optional `version`
  (display), `strip_prefix` (install only that subdirectory) and `flatten`. Shallow-fetches
  just that commit with gitoxide (`gix`) over Natron's own HTTP client, so no
  `git` install is needed. The
  user's git config is ignored (no `core.autocrlf`, no filter drivers), so every
  host gets the same bytes. Use it where GitHub's generated `/archive/`
  tarballs are blocked or unstable. The repository must be readable without
  credentials (nothing prompts, no credential helper runs). Submodules are not
  fetched and Git LFS files stay pointer files. `ssh` URLs need the `ssh`
  program and `file://` URLs need `git`; `http(s)` needs nothing.
- `flatten` (`url` and `git`): `flatten = { into = "inc", files = ["*.hxx", "*.h"] }`
  also places every file whose name matches into one directory of the install,
  for source trees that keep headers next to the code but include them by bare
  name (OpenCASCADE). One `-I` then replaces one per package, which matters on
  Windows, where failed header lookups are slow. Same content, stored once; two
  matching files with the same name are an error.
- **`msvc`**: install MSVC compiler + CRT + redist from one exact
  Microsoft VS build. Required `build_version` (e.g.
  `18.6.11819.183`); its major (16/17/18) selects the VS series.
  Required `manifest_commit`: the full commit hash of that build's
  snapshot on the mirror (`natron msvc versions` prints it); the build at
  that commit must be `build_version`.
  Optional `base_install` (`none` | `default` | `full`, default
  `default`) and `extras` (list of glob patterns added on top of the
  base set). See "MSVC package selection" below.
- **`windows_sdk`**: install Windows SDK headers + libs. Required
  `sdk_version` (e.g. `26100`) and `manifest_commit` (a mirror snapshot
  that lists that SDK; `natron windows_sdk versions` prints the newest). Optional `base_install` (`none` |
  `default` | `full`, default `default`) and `extras` (list of MSI
  filename prefixes added on top). Independently versioned from MSVC.
  See "Windows SDK package selection" below.

Both providers source data from the
[`roblabla/msvc-manifest-history`][mh] community mirror — Microsoft's
per-VS-release `channel.json` and `manifest.json`, snapshotted on
`release-{16,17,18}` branches. Each install pins one snapshot by its
full commit hash (`manifest_commit`). The commit hash covers the
`manifest.json` bytes, and the manifest carries every payload's sha256,
so whoever controls the mirror can't change what an install downloads.
A payload without a sha256 is refused. `sdk_version` alone would not be
a pin: Microsoft republishes an SDK's MSIs under the same version
(`26100` was 10.0.26100.16 in May 2026 snapshots and .18 in November's).
To move to a newer build, change both values.

If the mirror goes away, both installs break. Known tradeoff —
preferable to losing historical-version reproducibility.

[mh]: https://github.com/roblabla/msvc-manifest-history

### MSVC package selection

`base_install` chooses the starting set; `extras` adds more on top.

- `default` (the default): compiler + locale resources + CRT headers +
  desktop CRT + store CRT + redist. Roughly the minimum useful native
  C/C++ toolchain.
- `full`: every package in the snapshot (including legacy compat
  toolsets Microsoft re-ships in every VS installer). ~10 GB.
  Mutually exclusive with `extras`.
- `none`: install only what's in `extras`. Requires at least one
  pattern.

```toml
# Just the default set.
[toolchain.options]
build_version   = "18.6.11819.183"
manifest_commit = "<40 hex digits>"

# Default + a few extras.
[toolchain.options]
build_version   = "18.6.11819.183"
manifest_commit = "<40 hex digits>"
extras        = ["ATL.*", "MFC.*"]

# Everything in the snapshot.
[toolchain.options]
build_version   = "18.6.11819.183"
manifest_commit = "<40 hex digits>"
base_install  = "full"
```

Pattern rules for `extras`:

- `*` matches any characters; `?` matches one character.
- Matching is case-insensitive.
- Patterns without a `Microsoft.` prefix match against the
  family-relative tail. For a snapshot whose primary compiler family is
  `Microsoft.VC.14.50.18.0`, `ATL.X64.base` matches
  `Microsoft.VC.14.50.18.0.ATL.X64.base`.
- Patterns starting with `Microsoft.` match raw package IDs (escape
  hatch for outside-family packages like
  `Microsoft.VC.Preview.DIA.*`).
- Every pattern must match at least one package, otherwise the install
  fails instead of silently producing a partial toolchain.

### MSVC debug commands

```bash
# Every available Microsoft VS build per series (newest-first).
natron msvc versions
natron msvc versions --vs vs2026

# Every package in one build's snapshot, grouped: family first, other.
natron msvc packages --build-version 18.6.11819.183

# Download + extract every package at a build into per-package dirs.
# Use this to discover which package contains a missing file (grep,
# Explorer, Everything, etc.). You manage cleanup of the --out directory.
natron msvc extract --build-version 18.6.11819.183 --out C:\temp\msvc
```

`versions` enumerates each release branch's commits via the GitHub API
and fetches each commit's small `channel.json` (~130 KB; cached after
first run). `extract` reuses the global download cache, so re-runs don't
re-download. Already-populated package dirs in `--out` are skipped.

### Windows SDK package selection

Same `base_install` shape as MSVC, but the selection unit is **MSI
filename prefix** rather than package id glob — the SDK's user-facing
"components" are at the MSI level.

- `default` (the default): 7 essential MSIs that get you a typical
  C/C++ dev set — Universal CRT, Win32 desktop headers/libs, OneCore
  headers, UWP/Windows Store headers + libs + tools.
- `full`: every MSI in the SDK component meta-package's dep graph
  (debuggers, ARM/ARM64 target libs, driver headers, signing tools,
  etc.). ~3-4 GB. Mutually exclusive with `extras`.
- `none`: install only what's in `extras`. Requires at least one prefix.

```toml
# Default install.
[toolchain.options]
sdk_version     = "26100"
manifest_commit = "<40 hex digits>"

# Default + extras (common: signtool, mt.exe, rc.exe, ARM64 libs).
[toolchain.options]
sdk_version     = "26100"
manifest_commit = "<40 hex digits>"
extras = [
  "Universal CRT Tools x64",         # signtool, etc.
  "SDK ARM64 Additions",
]

# Everything.
[toolchain.options]
sdk_version     = "26100"
manifest_commit = "<40 hex digits>"
base_install = "full"
```

`extras` entries are plain filename prefixes — no globs. Each entry must
match at least one MSI in the SDK, otherwise install fails (no silent
typos).

### Windows SDK debug commands

```bash
# Every available Windows SDK version (newest-first).
natron windows_sdk versions

# Every MSI in a specific SDK, grouped by default-installed vs
# available-for-extras. Use this to discover what to put in extras.
natron windows_sdk packages --sdk-version 26100

# Download + extract every MSI from one SDK into per-MSI dirs.
# Use this to discover which MSI contains a missing file.
natron windows_sdk extract --sdk-version 26100 --out C:\temp\sdk
```

## Deploy modes

- **`symlink`** (default): a single directory symlink from the deploy dir
  to the cache install tree. Instant. Atomic version swaps (just rewrite
  the link). Cross-volume safe. On Windows, falls back to a directory
  junction when creating symlinks lacks privilege.
- **`copy`**: a real file copy. Slow and uses real disk space, but the
  only mode where deployed files are independent of the cache — use
  when you want to commit the toolchain into source control.

The cache itself always uses hardlinks internally (each install tree's
files are hardlinks into a content-addressed `cas/` directory). That
machinery is independent of the deploy mode and isn't user-visible.

## Cache layout

natron uses one global cache (default `~/.natron/` on every platform —
shared across all projects, like `~/.cargo` or `~/.rustup`):

```
<cache>/
  installs/
    <fingerprint>/
      tree/                  # the install (read-only)
      metadata.toml
  cas/<aa>/<bbcc.../>        # content-addressed blobs (xxhash3-128)
  downloads/                 # archive download cache
  staging/                   # in-progress installs (renamed atomically into installs/)
```

The CAS dedupes byte-identical files across toolchains. Two LLVM versions
that share a `LICENSE` or a sub-component end up sharing one inode in the
cache, with hardlinks into each install tree.

## Library API

```rust
use natron::{Natron, Config};

let n = Natron::from_config_file("natron.toml".as_ref())?;
let report = n.sync()?;
for entry in &report.entries {
    println!("{}: {:?}", entry.name, entry.action);
}
```

For tests / advanced consumers, `Natron::from_config_with_registry(cfg, reg)`
takes a custom `ProviderRegistry`, letting you point providers at fixture
URLs (every network-using provider has a `with_*_url` constructor for
overriding its base URL).

## Tests

`cargo test` is fully hermetic — no network. Synthetic archives and fake
release JSON are generated at test time and served via `file://` URLs.

Integration tests that hit real upstream services are individually
`#[ignore]`'d. Use cargo's standard runner flags to opt in:

```bash
cargo test                              # hermetic only (default)
cargo test -- --ignored                 # only the network tests
cargo test -- --include-ignored         # both
```

## License

MIT OR Unlicense.
