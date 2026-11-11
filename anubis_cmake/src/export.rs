//! Write a CMakeLists.txt that builds the given C/C++ targets with the
//! consumer's own compiler, for projects that want to ship Anubis builds to
//! CMake users without maintaining CMake by hand.
//!
//! Experimental, and deliberately its own crate: Anubis itself knows nothing
//! about CMake. The exporter only reads rules the way a build would
//! (`Anubis::get_rule` for each mode) and never runs a build.
//!
//! How it stays faithful to Anubis:
//! - Each target's compile and link arguments are computed the way
//!   `cc_rules.rs` computes them: the public args of every library reachable
//!   through deps (pre-order, each once), then the target's own, de-duplicated
//!   keeping the first; libraries in link order. The target gets that exact
//!   list as PRIVATE properties, and links its deps with `$<LINK_ONLY:..>` so
//!   CMake adds nothing of its own (CMake would put a target's own args
//!   before its deps', the reverse of Anubis).
//! - Libraries also get INTERFACE properties (their public args, flattened the
//!   same way) for the consumer's own targets.
//! - Modes (`-m`) are resolved separately, then merged: a value found in every
//!   mode is unconditional, the rest get a generator expression on the mode
//!   variables that tell the modes apart (`target_platform` ->
//!   `$<PLATFORM_ID:..>`, `build_type` -> `$<CONFIG:..>`). Merging keeps every
//!   mode's order; when the modes order values incompatibly, each mode gets
//!   its own complete list instead.
//! - The toolchain settings the code depends on are exported too, for this
//!   directory only, before each target's own as in Anubis: `-std=`,
//!   instruction-set flags (`-mavx`, ...), `-fPIC`, and feature defines
//!   (`UNICODE`, `_GNU_SOURCE`, ...); see `code_depends_on`. Build
//!   preferences (optimization, debug info, the C runtime, linker flags) come
//!   from the consumer's CMake configuration, and Anubis's hermetic setup
//!   (`-nostdinc`, `-target`, its sysroot and bundled libc++) from the
//!   consumer's compiler.
//! - Flags pass through unchanged for GCC/Clang; clang-cl gets each behind
//!   `/clang:`. MSVC's own cl.exe takes different flags, so the generated file
//!   refuses it rather than guess.
//!
//! Anything it can't express faithfully is an error, never a silent guess.

use anubis::anubis::{Anubis, AnubisTarget, DEFAULT_TOOLCHAIN};
use anubis::rules::cc_rules::{CcBinary, CcLanguage, CcLibrary, CcLibraryKind};
use anubis::rules::ifs_rules::IfsStubLibrary;
use anubis::rules::zig_rules::ZigGlibc;
use anubis::toolchain::Mode;
use anubis::{anyhow_loc, bail_loc};
use camino::{Utf8Path, Utf8PathBuf};
use indexmap::{IndexMap, IndexSet};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::hash::Hash;
use std::sync::Arc;

pub struct ExportOptions {
    pub modes: Vec<String>,
    pub targets: Vec<String>,
    /// Targets the consumer's system provides (e.g. vendored X11 headers):
    /// exported as empty INTERFACE libraries, their deps not followed.
    pub system_targets: Vec<String>,
    /// The CMakeLists.txt to write; paths in it are relative to its directory.
    pub output: Utf8PathBuf,
    /// Prefix for CMake target names and the `<namespace>::` of their aliases.
    pub namespace: String,
    /// The command line, recorded in the file's header.
    pub command_line: String,
}

/// Generate the CMakeLists.txt text.
pub fn export_cmake(anubis: &Anubis, opts: &ExportOptions) -> anyhow::Result<String> {
    if opts.modes.is_empty() {
        bail_loc!("anubis-cmake needs at least one mode (-m), e.g. one per platform and build type");
    }
    if !is_identifier(&opts.namespace) {
        bail_loc!(
            "namespace [{}] must be a C identifier (letters, digits, _)",
            opts.namespace
        );
    }
    let modes = opts
        .modes
        .iter()
        .map(|m| anubis.get_mode(&AnubisTarget::new(m)?).map(|m| as_native(&m)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let dims = Dims::new(&modes)?;
    let roots = anubis.expand_targets(&opts.targets)?;
    let system: HashSet<AnubisTarget> = anubis.expand_targets(&opts.system_targets)?.into_iter().collect();

    // Resolve every reachable target once per mode.
    let mut targets: IndexMap<AnubisTarget, Vec<Option<TargetData>>> = IndexMap::new();
    let mut toolchains: Vec<ToolchainInfo> = vec![];
    for (mi, mode) in modes.iter().enumerate() {
        let toolchain = anubis.get_toolchain(mode, &AnubisTarget::new(DEFAULT_TOOLCHAIN)?)?;
        let info = ToolchainInfo::new(toolchain.c.as_ref(), toolchain.cpp.as_ref())?;
        let mut stack: Vec<AnubisTarget> = roots.iter().rev().cloned().collect();
        let mut seen: HashSet<AnubisTarget> = HashSet::new();
        while let Some(t) = stack.pop() {
            if !seen.insert(t.clone()) {
                continue;
            }
            let data = if system.contains(&t) {
                TargetData::provided(&t)
            } else {
                let rule = anubis.get_rule(&t, mode)?;
                match TargetData::from_rule(&t, rule)? {
                    Some(d) => d,
                    None => continue,
                }
            };
            stack.extend(data.deps.iter().rev().cloned());
            targets.entry(t.clone()).or_insert_with(|| vec![None; modes.len()])[mi] = Some(data);
        }
        toolchains.push(info);
    }

    let names = cmake_names(targets.keys(), &opts.namespace)?;
    let order = topo_order(&targets)?;

    // Each target's args, per mode, the way Anubis computes them.
    let mut resolved: HashMap<AnubisTarget, Vec<Option<Resolved>>> = HashMap::new();
    for mi in 0..modes.len() {
        let graph: ModeGraph = targets.iter().filter_map(|(t, pm)| pm[mi].as_ref().map(|d| (t, d))).collect();
        let mut memo = HashMap::new();
        for (t, d) in &graph {
            let r = resolve(d, &graph, &mut memo);
            resolved.entry((*t).clone()).or_insert_with(|| vec![None; modes.len()])[mi] = Some(r);
        }
    }

    let out_dir = opts.output.parent().ok_or_else(|| anyhow_loc!("no parent dir for [{}]", opts.output))?;
    let paths = PathRenderer::new(&anubis.root, out_dir)?;

    // Static libraries linked into a shared library must be position independent.
    let mut pic: HashSet<AnubisTarget> = HashSet::new();
    for pm in resolved.values() {
        for r in pm.iter().flatten() {
            if let Some(linked) = &r.linked_into_shared {
                pic.extend(linked.iter().cloned());
            }
        }
    }

    // The toolchain's own -fPIC (exported with its flags) covers everything;
    // otherwise only static libraries linked into shared ones need PIC.
    // Windows has no PIC.
    let toolchain_pic =
        toolchains.iter().enumerate().all(|(m, t)| t.pic || dims.platform_per_mode[m] == "windows");
    if toolchain_pic {
        pic.clear();
    }

    let langs = Languages::new(&targets, &dims);
    let mut out = String::new();
    write_header(&mut out, opts, &modes, &paths, &langs, &dims, &toolchains);
    write_toolchain(&mut out, &dims, &paths, &langs, &toolchains)?;

    let shared: HashSet<AnubisTarget> = targets
        .iter()
        .filter(|(_, pm)| pm.iter().flatten().any(|d| d.kind == Kind::Shared))
        .map(|(t, _)| t.clone())
        .collect();
    let ctx = Ctx {
        dims: &dims,
        names: &names,
        paths: &paths,
        pic: &pic,
        shared: &shared,
        n_modes: modes.len(),
    };
    let mut source_langs: HashMap<Utf8PathBuf, (CcLanguage, AnubisTarget)> = HashMap::new();
    for t in &order {
        write_target(&mut out, t, &targets[t], &resolved[t], &ctx, &mut source_langs)?;
    }
    Ok(out)
}

/// The mode as if building on the target platform itself. The export must not
/// depend on the machine that runs it (a mac has no toolchain entry, and
/// `--check` must agree everywhere), and the consumer builds natively anyway.
fn as_native(mode: &Mode) -> Arc<Mode> {
    let mut m = mode.clone();
    for (build, target) in [
        ("build_platform", "target_platform"),
        ("build_arch", "target_arch"),
    ] {
        if let Some(v) = m.vars.get(target).cloned() {
            m.vars.insert(build.to_string(), v);
        }
    }
    Arc::new(m)
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ----------------------------------------------------------------------------
// The toolchain: what's exported from it and what isn't
// ----------------------------------------------------------------------------
#[derive(Clone, Debug, Default)]
struct ToolchainInfo {
    pic: bool,
    /// Per language (C, C++), what's exported, in Anubis's order: flags
    /// (grouped with their arguments) and defines.
    flags: [Vec<Vec<String>>; 2],
    defines: [Vec<String>; 2],
    /// What isn't exported, for the generated file's header.
    dropped: IndexSet<String>,
}

const LANGS: [&str; 2] = ["C", "CXX"];

/// Toolchain flags the code depends on: the language standard, the
/// instruction sets intrinsics need (which also set the minimum CPU),
/// exceptions and RTTI, and position-independent code for shared libraries. Everything else the
/// toolchain passes is a build preference (optimization, debug info,
/// reproducibility, ABI defaults) or Anubis's hermetic setup, and is left to
/// the consumer's CMake.
fn code_depends_on(flag: &str) -> bool {
    const ISA: &[&str] = &[
        "sse",
        "ssse",
        "avx",
        "fma",
        "f16c",
        "bmi",
        "popcnt",
        "lzcnt",
        "aes",
        "pclmul",
        "sha",
        "crc32",
        "movbe",
        "adx",
        "rdrnd",
        "rdseed",
        "xsave",
        "vpclmulqdq",
        "gfni",
        "vaes",
        "amx",
    ];
    flag.starts_with("-std=")
        || flag == "-fPIC"
        || flag == "-fpic"
        || ["-fexceptions", "-fno-exceptions", "-frtti", "-fno-rtti"].contains(&flag)
        || ["-march=", "-mcpu=", "-mfpu="].iter().any(|p| flag.starts_with(p))
        || flag.strip_prefix("-m").is_some_and(|isa| ISA.iter().any(|p| isa.starts_with(p)))
}

/// Toolchain defines the code depends on: feature macros that change what
/// headers declare (`UNICODE`, `WIN32_LEAN_AND_MEAN`, `_GNU_SOURCE`, ...).
/// Not: the build type's (`NDEBUG`, `_DEBUG`), the C runtime's (`_MT`,
/// `_DLL`), Anubis's bundled libc++ configuration, or macros the compiler
/// predefines.
fn define_code_depends_on(define: &str) -> bool {
    let name = define.split('=').next().unwrap_or(define);
    !([
        "NDEBUG",
        "_DEBUG",
        "_MT",
        "_DLL",
        "__linux__",
        "__x86_64__",
        "__aarch64__",
        "_WIN32",
        "_WIN64",
    ]
    .contains(&name)
        || name.starts_with("_LIBCPP")
        || name.starts_with("_LIBCXXABI"))
}

impl ToolchainInfo {
    fn new(
        c: Option<&anubis::toolchain::CcToolchain>,
        cpp: Option<&anubis::toolchain::CcToolchain>,
    ) -> anyhow::Result<ToolchainInfo> {
        let mut info = ToolchainInfo::default();
        for (li, cc) in [c, cpp].into_iter().enumerate() {
            let Some(cc) = cc else { continue };
            for item in group_flags(&cc.compiler_flags) {
                if code_depends_on(&item[0]) {
                    if item[0] == "-fPIC" || item[0] == "-fpic" {
                        info.pic = true;
                    }
                    info.flags[li].push(item);
                } else {
                    info.dropped.insert(item.join(" "));
                }
            }
            for d in &cc.defines {
                if define_code_depends_on(d) {
                    info.defines[li].push(d.clone());
                } else if d.starts_with("_LIBCPP") || d.starts_with("_LIBCXXABI") {
                    info.dropped.insert("its libc++ configuration (-D_LIBCPP_*)".to_string());
                } else {
                    info.dropped.insert(format!("-D{d}"));
                }
            }
            if !cc.linker_flags.is_empty() {
                info.dropped.insert("its linker flags".to_string());
            }
            if !cc.libraries.is_empty() {
                info.dropped.insert("its libraries and C runtime".to_string());
            }
            if !cc.system_include_dirs.is_empty() {
                info.dropped.insert("its system include dirs".to_string());
            }
            if !cc.library_dirs.is_empty() {
                info.dropped.insert("its library dirs".to_string());
            }
        }
        Ok(info)
    }
}

/// One list from a C list and a C++ list: values in both apply to every
/// language, the rest to theirs, each language keeping its order.
fn by_language<T: Clone + Eq>(lists: &[Vec<T>; 2]) -> Vec<(T, Option<&'static str>)> {
    let [c, cpp] = lists;
    let mut out = vec![];
    let mut ci = 0;
    for v in cpp {
        if let Some(pos) = c[ci..].iter().position(|x| x == v) {
            for x in &c[ci..ci + pos] {
                if !cpp.contains(x) {
                    out.push((x.clone(), Some(LANGS[0])));
                }
            }
            ci += pos + 1;
            out.push((v.clone(), None));
        } else {
            out.push((v.clone(), Some(LANGS[1])));
        }
    }
    for x in &c[ci..] {
        if !cpp.contains(x) {
            out.push((x.clone(), Some(LANGS[0])));
        }
    }
    // Check each language sees its own list; otherwise keep them apart.
    let sees = |li: usize| -> Vec<T> {
        out.iter().filter(|(_, l)| l.is_none() || *l == Some(LANGS[li])).map(|(v, _)| v.clone()).collect()
    };
    if sees(0) == *c && sees(1) == *cpp {
        return out;
    }
    c.iter()
        .map(|v| (v.clone(), Some(LANGS[0])))
        .chain(cpp.iter().map(|v| (v.clone(), Some(LANGS[1]))))
        .collect()
}

// ----------------------------------------------------------------------------
// What a target is in one mode
// ----------------------------------------------------------------------------
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Static,
    Shared,
    Exe,
    /// `ifs_stub_library`: link the system library by name.
    System,
    /// `--system`: the consumer's system provides it; nothing to build.
    Provided,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Fields {
    flags: Vec<String>,
    defines: Vec<String>,
    include_dirs: Vec<Utf8PathBuf>,
    libraries: Vec<Utf8PathBuf>,
    library_dirs: Vec<Utf8PathBuf>,
}

#[derive(Clone, Debug)]
struct TargetData {
    kind: Kind,
    lang: Option<CcLanguage>,
    name: String,
    output_name: String,
    srcs: Vec<Utf8PathBuf>,
    deps: Vec<AnubisTarget>,
    public: Fields,
    private: Fields,
    /// `ifs_stub_library`: the system library name (`X11` for libX11.so.6).
    system_lib: Option<String>,
}

impl TargetData {
    fn provided(t: &AnubisTarget) -> TargetData {
        TargetData {
            kind: Kind::Provided,
            lang: None,
            name: t.target_name().to_string(),
            output_name: t.target_name().to_string(),
            srcs: vec![],
            deps: vec![],
            public: Fields::default(),
            private: Fields::default(),
            system_lib: None,
        }
    }

    fn from_rule(t: &AnubisTarget, rule: Arc<dyn anubis::Rule>) -> anyhow::Result<Option<TargetData>> {
        let rule = match rule.downcast_arc::<CcLibrary>() {
            Ok(lib) => {
                return Ok(Some(TargetData {
                    kind: match lib.kind {
                        CcLibraryKind::Static => Kind::Static,
                        CcLibraryKind::Shared => Kind::Shared,
                    },
                    lang: Some(lib.lang),
                    name: lib.name.clone(),
                    output_name: lib.name.clone(),
                    srcs: lib.srcs.clone(),
                    deps: lib.deps.clone(),
                    public: Fields {
                        flags: lib.public_compiler_flags.clone(),
                        defines: lib.public_defines.clone(),
                        include_dirs: lib.public_include_dirs.clone(),
                        libraries: lib.public_libraries.clone(),
                        library_dirs: lib.public_library_dirs.clone(),
                    },
                    private: Fields {
                        flags: lib.private_compiler_flags.clone(),
                        defines: lib.private_defines.clone(),
                        include_dirs: lib.private_include_dirs.clone(),
                        libraries: vec![],
                        library_dirs: vec![],
                    },
                    system_lib: None,
                }))
            }
            Err(rule) => rule,
        };
        let rule = match rule.downcast_arc::<CcBinary>() {
            Ok(bin) => {
                return Ok(Some(TargetData {
                    kind: Kind::Exe,
                    lang: Some(bin.lang),
                    name: bin.name.clone(),
                    output_name: bin.exe_name.clone().unwrap_or_else(|| bin.name.clone()),
                    srcs: bin.srcs.clone(),
                    deps: bin.deps.clone(),
                    public: Fields::default(),
                    private: Fields {
                        flags: bin.compiler_flags.clone(),
                        defines: bin.compiler_defines.clone(),
                        include_dirs: bin.include_dirs.clone(),
                        libraries: bin.libraries.clone(),
                        library_dirs: bin.library_dirs.clone(),
                    },
                    system_lib: None,
                }))
            }
            Err(rule) => rule,
        };
        let rule = match rule.downcast_arc::<IfsStubLibrary>() {
            Ok(ifs) => {
                let lib = system_lib_from_ifs(&ifs.ifs)?;
                return Ok(Some(TargetData {
                    kind: Kind::System,
                    lang: None,
                    name: ifs.name.clone(),
                    output_name: ifs.name.clone(),
                    srcs: vec![],
                    deps: vec![],
                    public: Fields::default(),
                    private: Fields::default(),
                    system_lib: Some(lib),
                }));
            }
            Err(rule) => rule,
        };
        if rule.downcast_arc::<ZigGlibc>().is_ok() {
            // The libc a toolchain links; the consumer's toolchain has its own.
            return Ok(None);
        }
        bail_loc!(
            "[{}] is a rule type it can't export (only cc_binary, cc_static_library, \
             cc_shared_library and ifs_stub_library are supported; mark targets the \
             consumer's system provides with --system)",
            t
        )
    }

    fn is_library(&self) -> bool {
        matches!(self.kind, Kind::Static | Kind::Shared)
    }
}

/// `SoName: libX11.so.6` in a `.ifs` file -> `X11`.
fn system_lib_from_ifs(ifs: &Utf8Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(ifs).map_err(|e| anyhow_loc!("reading [{}]: {}", ifs, e))?;
    let soname = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("SoName:"))
        .map(|s| s.trim().trim_matches('\'').trim_matches('"').to_string())
        .ok_or_else(|| anyhow_loc!("no SoName in [{}]", ifs))?;
    let stem = soname.split(".so").next().unwrap_or(&soname);
    Ok(stem.strip_prefix("lib").unwrap_or(stem).to_string())
}

// ----------------------------------------------------------------------------
// Resolving args the way cc_rules.rs does
// ----------------------------------------------------------------------------
type ModeGraph<'a> = HashMap<&'a AnubisTarget, &'a TargetData>;

/// Args accumulated like Anubis's `CcExtraArgs`: each list keeps the first
/// occurrence of a value.
#[derive(Clone, Debug, Default, PartialEq)]
struct FlatArgs {
    flags: IndexSet<String>,
    defines: IndexSet<String>,
    include_dirs: IndexSet<Utf8PathBuf>,
    libraries: IndexSet<Utf8PathBuf>,
    library_dirs: IndexSet<Utf8PathBuf>,
}

impl FlatArgs {
    fn extend(&mut self, f: &Fields) {
        self.flags.extend(f.flags.iter().cloned());
        self.defines.extend(f.defines.iter().cloned());
        self.include_dirs.extend(f.include_dirs.iter().cloned());
        self.libraries.extend(f.libraries.iter().cloned());
        self.library_dirs.extend(f.library_dirs.iter().cloned());
    }
}

/// One entry on a link line.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LinkItem {
    Target(AnubisTarget),
    Library(Utf8PathBuf),
}

/// A target's args in one mode.
#[derive(Clone, Debug, Default)]
struct Resolved {
    /// How its own sources compile (targets with sources).
    compile: FlatArgs,
    /// What a dependent gets from it (libraries).
    interface: FlatArgs,
    /// Its own link line, deps then libraries (executables, shared libraries).
    link: Vec<LinkItem>,
    /// What a consumer that links it must link too (libraries).
    interface_link: Vec<LinkItem>,
    /// Shared libraries: the static libraries linked into it.
    linked_into_shared: Option<Vec<AnubisTarget>>,
}

/// Anubis's `collect_transitive_public_args`: the public args of every
/// library reachable from `deps`, pre-order, each library once.
fn public_closure(deps: &[AnubisTarget], g: &ModeGraph) -> FlatArgs {
    let mut out = FlatArgs::default();
    let mut visited: HashSet<&AnubisTarget> = HashSet::new();
    let mut stack: Vec<&AnubisTarget> = deps.iter().rev().collect();
    while let Some(dep) = stack.pop() {
        if !visited.insert(dep) {
            continue;
        }
        let Some(d) = g.get(dep) else { continue };
        if d.is_library() {
            out.extend(&d.public);
            stack.extend(d.deps.iter().rev());
        }
    }
    out
}

/// What linking `t` puts on a dependent's link line, in Anubis's order: a
/// static library's own archive then its deps' libraries; a shared or system
/// library only itself (it already contains its static deps).
fn libs_of(
    t: &AnubisTarget,
    g: &ModeGraph,
    memo: &mut HashMap<AnubisTarget, Vec<LinkItem>>,
) -> Vec<LinkItem> {
    if let Some(v) = memo.get(t) {
        return v.clone();
    }
    // Cycles are rejected by topo_order; this guards the recursion meanwhile.
    memo.insert(t.clone(), vec![]);
    let Some(d) = g.get(t) else { return vec![] };
    let mut out: IndexSet<LinkItem> = IndexSet::new();
    match d.kind {
        Kind::Static => {
            if !d.srcs.is_empty() {
                out.insert(LinkItem::Target(t.clone()));
            }
            for dep in &d.deps {
                out.extend(libs_of(dep, g, memo));
            }
        }
        Kind::Shared | Kind::System => {
            out.insert(LinkItem::Target(t.clone()));
        }
        Kind::Exe | Kind::Provided => {}
    }
    let v: Vec<LinkItem> = out.into_iter().collect();
    memo.insert(t.clone(), v.clone());
    v
}

fn resolve(d: &TargetData, g: &ModeGraph, memo: &mut HashMap<AnubisTarget, Vec<LinkItem>>) -> Resolved {
    let mut r = Resolved::default();
    let closure = public_closure(&d.deps, g);

    // Compile: deps' public args, then our own (public, then private).
    let mut compile = closure.clone();
    compile.extend(&d.public);
    compile.extend(&d.private);
    r.compile = compile;

    if d.is_library() {
        let mut iface = closure;
        iface.extend(&d.public);
        r.interface = iface;
    }

    // The link line of anything that links: deps' libraries in order, then
    // libraries by name.
    let link_line = |memo: &mut HashMap<AnubisTarget, Vec<LinkItem>>, libraries: &IndexSet<Utf8PathBuf>| {
        let mut items: IndexSet<LinkItem> = IndexSet::new();
        for dep in &d.deps {
            items.extend(libs_of(dep, g, memo));
        }
        items.extend(libraries.iter().cloned().map(LinkItem::Library));
        items.into_iter().collect::<Vec<_>>()
    };
    match d.kind {
        Kind::Exe => r.link = link_line(memo, &r.compile.libraries),
        Kind::Shared => {
            r.link = link_line(memo, &r.compile.libraries);
            r.linked_into_shared = Some(
                r.link
                    .iter()
                    .filter_map(|i| match i {
                        LinkItem::Target(t) if g.get(t).is_some_and(|d| d.kind == Kind::Static) => {
                            Some(t.clone())
                        }
                        _ => None,
                    })
                    .collect(),
            );
            r.interface_link = r.interface.libraries.iter().cloned().map(LinkItem::Library).collect();
        }
        Kind::Static => {
            // What a consumer linking us must also link: our deps' libraries,
            // then libraries by name.
            let mut items: IndexSet<LinkItem> = IndexSet::new();
            for dep in &d.deps {
                items.extend(libs_of(dep, g, memo));
            }
            items.extend(r.interface.libraries.iter().cloned().map(LinkItem::Library));
            r.interface_link = items.into_iter().collect();
        }
        Kind::System | Kind::Provided => {}
    }
    r
}

// ----------------------------------------------------------------------------
// Mode dimensions -> CMake conditions
// ----------------------------------------------------------------------------
/// The mode variables that differ between the exported modes, and how each
/// maps to a CMake condition.
struct Dims {
    /// (variable, value per mode); only variables that vary.
    vars: Vec<(String, Vec<String>)>,
    /// Every mode's target_platform and target_arch, varying or not.
    platforms: BTreeSet<String>,
    arches: BTreeSet<String>,
    platform_per_mode: Vec<String>,
}

impl Dims {
    fn new(modes: &[Arc<Mode>]) -> anyhow::Result<Dims> {
        let mut keys: BTreeSet<&String> = BTreeSet::new();
        for m in modes {
            keys.extend(m.vars.keys());
        }
        // Platform first: it's the most common reason values differ.
        let mut keys: Vec<&String> = keys.into_iter().collect();
        keys.sort_by_key(|k| match k.as_str() {
            "target_platform" => 0,
            "build_type" => 1,
            _ => 2,
        });
        let get = |m: &Mode, k: &str| m.vars.get(k).cloned().unwrap_or_default();
        let mut vars = vec![];
        for k in keys {
            // build_platform/build_arch follow target_* (see `as_native`).
            if k == "build_platform" || k == "build_arch" {
                continue;
            }
            let values: Vec<String> = modes.iter().map(|m| get(m, k)).collect();
            if values.iter().all(|v| v == &values[0]) {
                continue;
            }
            match k.as_str() {
                "target_platform" | "build_type" => {}
                _ => bail_loc!(
                    "mode variable [{}] differs between the modes ({}); only target_platform \
                     and build_type can become CMake conditions. Export modes that agree on it.",
                    k,
                    values.join(", ")
                ),
            }
            vars.push((k.clone(), values));
        }
        let mut platforms = BTreeSet::new();
        let mut arches = BTreeSet::new();
        let mut platform_per_mode = vec![];
        for m in modes {
            let p = get(m, "target_platform");
            platform_id(&p)?;
            platforms.insert(p.clone());
            platform_per_mode.push(p);
            let a = get(m, "target_arch");
            arch_regex(&a)?;
            arches.insert(a);
            if let Some(b) = m.vars.get("build_type") {
                if b != "debug" && b != "release" {
                    bail_loc!(
                        "mode [{}] has build_type [{}]; only debug and release map to CMake configs",
                        m.name,
                        b
                    );
                }
            }
        }
        Ok(Dims {
            vars,
            platforms,
            arches,
            platform_per_mode,
        })
    }

    /// The generator-expression condition for one variable's value.
    fn genex(&self, var: &str, value: &str) -> String {
        match var {
            "target_platform" => format!("$<PLATFORM_ID:{}>", platform_id(value).unwrap()),
            "build_type" => match value {
                // CMake has four configs; every non-Debug one is release.
                "release" => "$<NOT:$<CONFIG:Debug>>".to_string(),
                _ => "$<CONFIG:Debug>".to_string(),
            },
            _ => unreachable!("Dims::new rejects other variables"),
        }
    }

    /// The condition, if any, that holds in exactly the modes `subset` of the
    /// modes `within` (where the target exists). None means "always".
    fn condition(&self, subset: &BTreeSet<usize>, within: &BTreeSet<usize>) -> Option<String> {
        if subset == within {
            return None;
        }
        // One variable whose values pick out exactly `subset`?
        for (var, values) in &self.vars {
            let picked: BTreeSet<&String> = subset.iter().map(|&m| &values[m]).collect();
            let matches: BTreeSet<usize> =
                within.iter().copied().filter(|&m| picked.contains(&values[m])).collect();
            if &matches == subset {
                let conds: Vec<String> = picked.iter().map(|v| self.genex(var, v)).collect();
                return Some(or(conds));
            }
        }
        // Otherwise list the modes.
        let conds = subset
            .iter()
            .map(|&m| and(self.vars.iter().map(|(var, values)| self.genex(var, &values[m])).collect()))
            .collect();
        Some(or(conds))
    }

    /// Configure-time condition for a target that only exists in `subset`.
    fn target_if(
        &self,
        t: &AnubisTarget,
        subset: &BTreeSet<usize>,
        all: usize,
    ) -> anyhow::Result<Option<String>> {
        let all: BTreeSet<usize> = (0..all).collect();
        if subset == &all {
            return Ok(None);
        }
        let platforms = self.platforms_of(subset);
        let matches: BTreeSet<usize> =
            all.iter().copied().filter(|&m| platforms.contains(&self.platform_per_mode[m])).collect();
        if &matches == subset {
            return Ok(Some(platform_if(&platforms)));
        }
        bail_loc!(
            "[{}] is a dependency in some modes but not others, and target_platform alone \
             doesn't explain which; CMake can't make a target exist per build type",
            t
        )
    }

    fn platforms_of(&self, subset: &BTreeSet<usize>) -> BTreeSet<String> {
        subset.iter().map(|&m| self.platform_per_mode[m].clone()).collect()
    }
}

fn platform_id(target_platform: &str) -> anyhow::Result<&'static str> {
    Ok(match target_platform {
        "windows" => "Windows",
        "linux" => "Linux",
        "macos" => "Darwin",
        other => bail_loc!("no CMake platform for target_platform [{}]", other),
    })
}

/// CMAKE_SYSTEM_PROCESSOR spellings for a target_arch.
fn arch_regex(target_arch: &str) -> anyhow::Result<&'static str> {
    Ok(match target_arch {
        "x64" => "x86_64|AMD64|amd64|x64",
        "arm64" => "aarch64|arm64|ARM64",
        other => bail_loc!("no CMake processor for target_arch [{}]", other),
    })
}

fn platform_if(platforms: &BTreeSet<String>) -> String {
    platforms
        .iter()
        .map(|p| format!("CMAKE_SYSTEM_NAME STREQUAL \"{}\"", platform_id(p).unwrap()))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn or(mut conds: Vec<String>) -> String {
    if conds.len() == 1 {
        conds.pop().unwrap()
    } else {
        format!("$<OR:{}>", conds.join(","))
    }
}

fn and(mut conds: Vec<String>) -> String {
    match conds.len() {
        0 => "1".to_string(),
        1 => conds.pop().unwrap(),
        _ => format!("$<AND:{}>", conds.join(",")),
    }
}

/// Merge one ordered list across modes. Each mode's list must come out in
/// its own order: the result orders values so that, in every mode, the
/// values that apply appear exactly as that mode listed them. Values are
/// unique within a mode's list. When the modes order values incompatibly,
/// each distinct list is written whole under its own condition.
fn merge<T: Clone + Eq + Hash>(
    per_mode: &[(usize, Vec<T>)],
    within: &BTreeSet<usize>,
    dims: &Dims,
) -> Vec<(Option<String>, T)> {
    // Modes with each value, in first-seen order.
    let mut modes_of: IndexMap<T, BTreeSet<usize>> = IndexMap::new();
    for (m, values) in per_mode {
        for v in values {
            modes_of.entry(v.clone()).or_default().insert(*m);
        }
    }
    match order_compatibly(per_mode, &modes_of) {
        Some(order) => order.into_iter().map(|v| (dims.condition(&modes_of[&v], within), v)).collect(),
        None => {
            // One complete list per group of modes that agree.
            let mut groups: IndexMap<&Vec<T>, BTreeSet<usize>> = IndexMap::new();
            for (m, values) in per_mode {
                groups.entry(values).or_default().insert(*m);
            }
            let mut out = vec![];
            for (values, ms) in groups {
                let c = dims.condition(&ms, within);
                out.extend(values.iter().map(|v| (c.clone(), v.clone())));
            }
            out
        }
    }
}

/// Topological order of all values with an edge between each pair of
/// neighbors in every mode's list; ties go to first-seen. None on a cycle.
fn order_compatibly<T: Clone + Eq + Hash>(
    per_mode: &[(usize, Vec<T>)],
    modes_of: &IndexMap<T, BTreeSet<usize>>,
) -> Option<Vec<T>> {
    let n = modes_of.len();
    let idx = |v: &T| modes_of.get_index_of(v).unwrap();
    let mut succ: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    let mut indegree = vec![0usize; n];
    for (_, values) in per_mode {
        for w in values.windows(2) {
            let (a, b) = (idx(&w[0]), idx(&w[1]));
            if succ[a].insert(b) {
                indegree[b] += 1;
            }
        }
    }
    let mut ready: BTreeSet<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(&i) = ready.iter().next() {
        ready.remove(&i);
        order.push(modes_of.get_index(i).unwrap().0.clone());
        for &j in &succ[i] {
            indegree[j] -= 1;
            if indegree[j] == 0 {
                ready.insert(j);
            }
        }
    }
    (order.len() == n).then_some(order)
}

// ----------------------------------------------------------------------------
// Names, paths, quoting
// ----------------------------------------------------------------------------
struct CmakeName {
    real: String,
    alias: String,
    /// The name is unique among the exported targets, so the output file
    /// can keep the Anubis name (`libfoo.a`, not `lib<ns>_foo.a`).
    unique: bool,
}

/// `<ns>_<name>`, aliased as `<ns>::<name>`. Names that occur in more than
/// one directory get the directory mangled in. Two targets that still end up
/// with the same CMake name are an error.
fn cmake_names<'a>(
    targets: impl Iterator<Item = &'a AnubisTarget>,
    ns: &str,
) -> anyhow::Result<HashMap<AnubisTarget, CmakeName>> {
    let targets: Vec<&AnubisTarget> = targets.collect();
    let mut count: HashMap<&str, usize> = HashMap::new();
    for t in &targets {
        *count.entry(t.target_name()).or_default() += 1;
    }
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
    };
    let mut out = HashMap::new();
    let mut taken: HashMap<String, &AnubisTarget> = HashMap::new();
    for t in targets {
        let unique = count[t.target_name()] == 1;
        let base = if unique {
            sanitize(t.target_name())
        } else {
            sanitize(&format!(
                "{}_{}",
                t.get_relative_dir().trim_start_matches('/'),
                t.target_name()
            ))
        };
        if let Some(other) = taken.insert(base.clone(), t) {
            bail_loc!(
                "[{}] and [{}] would both be CMake target {}_{}; rename one",
                other,
                t,
                ns,
                base
            );
        }
        out.insert(
            t.clone(),
            CmakeName {
                real: format!("{}_{}", ns, base),
                alias: format!("{}::{}", ns, base),
                unique,
            },
        );
    }
    Ok(out)
}

struct PathRenderer {
    root: Utf8PathBuf,
    /// `<root>/toolchains`: installed toolchains, which a consumer doesn't have.
    toolchains: Utf8PathBuf,
    /// How the root is spelled in the generated file.
    root_expr: String,
}

impl PathRenderer {
    fn new(root: &Utf8Path, out_dir: &Utf8Path) -> anyhow::Result<PathRenderer> {
        let root = normalize(root);
        let out_dir = normalize(out_dir);
        let rel = out_dir
            .strip_prefix(&root)
            .map_err(|_| anyhow_loc!("output [{}] must be inside the project root [{}]", out_dir, root))?;
        let ups = rel.components().count();
        let root_expr = if ups == 0 {
            "${CMAKE_CURRENT_LIST_DIR}".to_string()
        } else {
            format!("${{CMAKE_CURRENT_LIST_DIR}}{}", "/..".repeat(ups))
        };
        let toolchains = root.join("toolchains");
        Ok(PathRenderer {
            root,
            root_expr,
            toolchains,
        })
    }

    /// A project path as `${_anubis_root}/rel` (unescaped).
    fn path(&self, p: &Utf8Path) -> anyhow::Result<String> {
        let p = normalize(p);
        if p.starts_with(&self.toolchains) {
            bail_loc!(
                "[{}] is inside the installed toolchains, which a CMake consumer doesn't have; \
                 if their system provides it, mark the target that uses it with --system",
                p
            );
        }
        let rel = p.strip_prefix(&self.root).map_err(|_| {
            anyhow_loc!(
                "[{}] is outside the project root; the export can only refer to project files",
                p
            )
        })?;
        if rel.as_str().contains(['"', ';', '$', '\\']) {
            bail_loc!("path [{}] has a character the export can't quote (\" ; $ \\)", p);
        }
        Ok(format!("${{_anubis_root}}/{}", rel))
    }

    /// A flag token, unescaped: absolute project paths become root-relative.
    fn flag_token(&self, tok: &str) -> anyhow::Result<String> {
        if Utf8Path::new(tok).is_absolute() {
            return self.path(Utf8Path::new(tok));
        }
        Ok(tok.to_string())
    }
}

fn normalize(p: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(anubis::util::normalize_path(p).as_str().replace('\\', "/"))
}

/// A value to write into a quoted CMake argument, possibly under a condition.
/// `text` is already escaped for a quoted argument and holds no `;`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Value {
    text: String,
    /// Written by the exporter as a generator expression: not escaped again.
    genex: bool,
}

impl Value {
    /// User text (a flag, a define, a path). `${_anubis_*}` references are
    /// kept; everything else is escaped.
    fn user(s: &str) -> anyhow::Result<Value> {
        if s.contains(';') {
            bail_loc!("[{}] contains ';', which CMake would split into two values", s);
        }
        Ok(Value {
            text: escape(s),
            genex: false,
        })
    }

    fn render(&self, cond: Option<&str>) -> String {
        match cond {
            None => format!("\"{}\"", self.text),
            Some(c) if self.genex => format!("\"$<{}:{}>\"", c, self.text),
            Some(c) => format!("\"$<{}:{}>\"", c, escape_genex(&self.text)),
        }
    }
}

/// Escape for a quoted CMake argument, keeping `${_anubis_root}` and
/// `${_anubis_flag}` as variable references.
fn escape(s: &str) -> String {
    const KEEP: [&str; 2] = ["${_anubis_root}", "${_anubis_flag}"];
    let mut out = String::new();
    let mut rest = s;
    'outer: while !rest.is_empty() {
        for k in KEEP {
            if let Some(r) = rest.strip_prefix(k) {
                out.push_str(k);
                rest = r;
                continue 'outer;
            }
        }
        let c = rest.chars().next().unwrap();
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            _ => out.push(c),
        }
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// Escape characters that end or split a generator expression.
fn escape_genex(s: &str) -> String {
    s.replace('>', "$<ANGLE-R>").replace(',', "$<COMMA>")
}

/// Keep a flag together with a separate argument (`-include foo.h`,
/// `-Xclang -foo`), so CMake's option de-duplication can't split them.
fn group_flags(flags: &[String]) -> Vec<Vec<String>> {
    const TAKES_ARG: &[&str] = &[
        "-include",
        "-imacros",
        "-isystem",
        "-iquote",
        "-idirafter",
        "-iprefix",
        "-iwithprefix",
        "-isysroot",
        "--sysroot",
        "-Xclang",
        "-Xlinker",
        "-Xassembler",
        "-Xpreprocessor",
        "-mllvm",
        "-x",
        "-target",
        "-arch",
        "-I",
        "-D",
        "-U",
        "-MT",
        "-MQ",
        "-MF",
        "-F",
        "-framework",
    ];
    let mut out: Vec<Vec<String>> = vec![];
    let mut iter = flags.iter();
    while let Some(f) = iter.next() {
        let mut item = vec![f.clone()];
        if TAKES_ARG.contains(&f.as_str()) {
            if let Some(arg) = iter.next() {
                item.push(arg.clone());
            }
        }
        out.push(item);
    }
    out
}

/// One compile option (a flag with its argument, if any) as a CMake value.
/// Several tokens go behind `SHELL:` with each token quoted, so paths with
/// spaces stay whole.
fn flag_value(item: &[String], paths: &PathRenderer) -> anyhow::Result<Value> {
    let toks = item
        .iter()
        .map(|t| Ok(format!("${{_anubis_flag}}{}", paths.flag_token(t)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if toks.len() == 1 {
        return Value::user(&toks[0]);
    }
    for t in &toks {
        if t.contains(['"', '\\', ';']) {
            bail_loc!("flag [{}] has a character the export can't quote (\" \\ ;)", t);
        }
    }
    Ok(Value {
        text: escape(&format!(
            "SHELL:{}",
            toks.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(" ")
        )),
        genex: false,
    })
}

/// A define as a `-D` option (clang-cl takes `-D` too).
fn define_value(d: &str) -> anyhow::Result<Value> {
    Value::user(&format!("-D{d}"))
}

/// `-std=c++20` applies to C++ only, `-std=c11` to C only. Other flags apply
/// to every language, as in Anubis.
fn flag_language(item: &[String]) -> Option<&'static str> {
    let f = item.first()?.strip_prefix("-std=")?;
    Some(if f.contains("++") { "CXX" } else { "C" })
}

// ----------------------------------------------------------------------------
// Writing
// ----------------------------------------------------------------------------
fn is_asm(p: &Utf8Path) -> bool {
    matches!(p.extension(), Some("S" | "s" | "sx"))
}

const CXX_EXTS: &[&str] = &["cpp", "cc", "cxx", "c++", "C"];

/// Which languages the export needs, and on which platforms ASM is needed.
struct Languages {
    c: bool,
    cxx: bool,
    asm: Option<BTreeSet<String>>,
    all_platforms: BTreeSet<String>,
}

impl Languages {
    fn new(targets: &IndexMap<AnubisTarget, Vec<Option<TargetData>>>, dims: &Dims) -> Languages {
        let mut l = Languages {
            c: false,
            cxx: false,
            asm: None,
            all_platforms: dims.platforms.clone(),
        };
        let mut asm = BTreeSet::new();
        for pm in targets.values() {
            for (mi, d) in pm.iter().enumerate() {
                let Some(d) = d else { continue };
                for s in &d.srcs {
                    if is_asm(s) {
                        asm.insert(dims.platform_per_mode[mi].clone());
                    } else if d.lang == Some(CcLanguage::Cpp)
                        || s.extension().is_some_and(|e| CXX_EXTS.contains(&e))
                    {
                        l.cxx = true;
                    } else {
                        l.c = true;
                    }
                }
            }
        }
        if !asm.is_empty() {
            l.asm = Some(asm);
        }
        l
    }
}

fn write_header(
    out: &mut String,
    opts: &ExportOptions,
    modes: &[Arc<Mode>],
    paths: &PathRenderer,
    langs: &Languages,
    dims: &Dims,
    toolchains: &[ToolchainInfo],
) {
    let ns = &opts.namespace;
    let mode_names: Vec<&str> = modes.iter().map(|m| m.target.target_path()).collect();
    let mut dropped: IndexSet<&String> = IndexSet::new();
    for t in toolchains {
        dropped.extend(t.dropped.iter());
    }
    let _ = writeln!(
        out,
        "# GENERATED by `anubis-cmake`. Do not edit; regenerate with:"
    );
    let _ = writeln!(out, "#   {}", opts.command_line);
    let _ = writeln!(out, "# Modes merged: {}", mode_names.join(" "));
    let _ = writeln!(out, "#");
    let _ = writeln!(
        out,
        "# Builds with your own GCC or Clang (clang-cl too). Use as a subdirectory"
    );
    let _ = writeln!(
        out,
        "# (add_subdirectory / FetchContent) and link the `{ns}::` targets."
    );
    let _ = writeln!(out, "#");
    let _ = writeln!(
        out,
        "# Targets build with the Anubis toolchain settings their code depends on;"
    );
    let _ = writeln!(out, "# your compiler and CMake configuration decide the rest.");
    let _ = writeln!(out, "# Not exported from the Anubis toolchain:");
    let wrap = |label: &str, items: Vec<&String>| -> String {
        let mut s = String::new();
        let mut line = format!("#   {label}:");
        if items.is_empty() {
            line.push_str(" (none)");
        }
        for i in items {
            if line.len() + i.len() + 1 > 78 {
                s.push_str(&line);
                s.push('\n');
                line = "#    ".to_string();
            }
            line.push(' ');
            line.push_str(i);
        }
        s.push_str(&line);
        s.push('\n');
        s
    };
    out.push_str(&wrap("dropped", dropped.into_iter().collect()));
    let _ = writeln!(out);
    let _ = writeln!(out, "cmake_minimum_required(VERSION 3.21)");
    let mut langs_list = vec![];
    if langs.c {
        langs_list.push("C");
    }
    if langs.cxx {
        langs_list.push("CXX");
    }
    let _ = writeln!(
        out,
        "project({ns} LANGUAGES {})",
        if langs_list.is_empty() {
            "NONE".to_string()
        } else {
            langs_list.join(" ")
        }
    );
    let _ = writeln!(out);

    // Only the platforms and processors the modes describe.
    let platform_list: Vec<&str> = dims.platforms.iter().map(|p| platform_id(p).unwrap()).collect();
    let arch_re: Vec<&str> = dims.arches.iter().map(|a| arch_regex(a).unwrap()).collect();
    let _ = write!(
        out,
        r#"# Exported for {platforms} on {arches} only; values for any other platform
# would be a guess.
set(_anubis_platforms {platforms})
if(NOT CMAKE_SYSTEM_NAME IN_LIST _anubis_platforms OR NOT CMAKE_SYSTEM_PROCESSOR MATCHES "^({arch_re})$")
  if({ns}_ALLOW_ANY_PLATFORM)
    message(WARNING "{ns}: exported for {platforms} ({arches}), not ${{CMAKE_SYSTEM_NAME}} (${{CMAKE_SYSTEM_PROCESSOR}}); {ns}_ALLOW_ANY_PLATFORM is set, so building anyway.")
  else()
    message(FATAL_ERROR "{ns}: exported for {platforms} ({arches}), not ${{CMAKE_SYSTEM_NAME}} (${{CMAKE_SYSTEM_PROCESSOR}}). Export a mode for this platform, or set {ns}_ALLOW_ANY_PLATFORM to try anyway.")
  endif()
endif()

set(_anubis_root "{root}")
"#,
        platforms = platform_list.join(" "),
        arches = dims.arches.iter().cloned().collect::<Vec<_>>().join(" "),
        arch_re = arch_re.join("|"),
        root = paths.root_expr,
    );

    if !langs_list.is_empty() {
        let langs_str = langs_list.join(" ");
        let _ = write!(
            out,
            r#"
# Flags are GCC/Clang flags. clang-cl gets each behind /clang:. cl.exe takes
# different flags altogether, so it isn't supported.
set(_anubis_flag "")
foreach(_lang {langs_str})
  if(CMAKE_${{_lang}}_COMPILER_ID STREQUAL "MSVC")
    message(FATAL_ERROR "{ns}: MSVC's cl.exe isn't supported; use Clang (Visual Studio: -T ClangCL).")
  endif()
  if(CMAKE_${{_lang}}_COMPILER_FRONTEND_VARIANT STREQUAL "MSVC")
    set(_anubis_${{_lang}}_flag "/clang:")
  else()
    set(_anubis_${{_lang}}_flag "")
  endif()
endforeach()
set(_anubis_flag "${{_anubis_{first}_flag}}")
foreach(_lang {langs_str})
  if(NOT _anubis_${{_lang}}_flag STREQUAL _anubis_flag)
    message(FATAL_ERROR "{ns}: the C and C++ compilers must both be clang-cl or both not.")
  endif()
endforeach()
"#,
            first = langs_list[0],
        );
    }

    // Preprocessed assembly (`.S`) needs CMake's ASM language.
    if let Some(asm) = &langs.asm {
        if asm == &langs.all_platforms {
            let _ = writeln!(out, "\nenable_language(ASM)");
        } else {
            let _ = writeln!(out, "\nif({})\n  enable_language(ASM)\nendif()", platform_if(asm));
        }
    }
}

/// The toolchain's settings, for every target in this directory, before each
/// target's own (as Anubis orders them).
fn write_toolchain(
    out: &mut String,
    dims: &Dims,
    paths: &PathRenderer,
    langs: &Languages,
    toolchains: &[ToolchainInfo],
) -> anyhow::Result<()> {
    if !langs.c && !langs.cxx {
        return Ok(());
    }
    let all: BTreeSet<usize> = (0..toolchains.len()).collect();
    // Merge (value, condition fragment) across modes and fold the fragment in.
    let merged = |per: PerMode<(Value, Option<String>)>, within: &BTreeSet<usize>| -> Vec<String> {
        merge(&per, within, dims)
            .into_iter()
            .map(|(c, (v, frag))| {
                let cond = match (frag, c) {
                    (None, c) => c,
                    (Some(f), None) => Some(f),
                    (Some(f), Some(c)) => Some(format!("$<AND:{f},{c}>")),
                };
                v.render(cond.as_deref())
            })
            .collect()
    };
    let lang_frag = |genex: &str, l: Option<&str>| l.map(|l| format!("$<{genex}:{l}>"));

    let _ = write!(
        out,
        r#"
# The Anubis toolchain settings the code depends on: language standard,
# instruction sets, PIC, feature defines. Optimization, debug info, the C
# runtime and linker settings come from your CMake configuration.
"#
    );
    let mut per: PerMode<(Value, Option<String>)> = vec![];
    for (m, t) in toolchains.iter().enumerate() {
        let mut v = vec![];
        for (item, l) in by_language(&t.flags) {
            // A -std= in both lists still only fits its own language.
            let l = flag_language(&item).or(l);
            v.push((flag_value(&item, paths)?, lang_frag("COMPILE_LANGUAGE", l)));
        }
        per.push((m, v));
    }
    let items = merged(per, &all);
    if !items.is_empty() {
        let _ = writeln!(out, "add_compile_options(\n  {}\n)", items.join("\n  "));
    }

    let mut per: PerMode<(Value, Option<String>)> = vec![];
    for (m, t) in toolchains.iter().enumerate() {
        let v = by_language(&t.defines)
            .into_iter()
            .map(|(d, l)| Ok((define_value(&d)?, lang_frag("COMPILE_LANGUAGE", l))))
            .collect::<anyhow::Result<Vec<_>>>()?;
        per.push((m, v));
    }
    let items = merged(per, &all);
    if !items.is_empty() {
        let _ = writeln!(out, "add_compile_options(\n  {}\n)", items.join("\n  "));
    }

    Ok(())
}

/// Deps before dependents, so the file reads bottom-up like a build. A cycle
/// is an error.
fn topo_order(
    targets: &IndexMap<AnubisTarget, Vec<Option<TargetData>>>,
) -> anyhow::Result<Vec<AnubisTarget>> {
    fn visit(
        t: &AnubisTarget,
        targets: &IndexMap<AnubisTarget, Vec<Option<TargetData>>>,
        path: &mut Vec<AnubisTarget>,
        done: &mut IndexSet<AnubisTarget>,
    ) -> anyhow::Result<()> {
        if done.contains(t) {
            return Ok(());
        }
        if let Some(i) = path.iter().position(|p| p == t) {
            let cycle: Vec<String> =
                path[i..].iter().chain(std::iter::once(t)).map(|t| t.to_string()).collect();
            bail_loc!("dependency cycle: {}", cycle.join(" -> "));
        }
        path.push(t.clone());
        let mut deps: IndexSet<&AnubisTarget> = IndexSet::new();
        for d in targets[t].iter().flatten() {
            deps.extend(d.deps.iter().filter(|d| targets.contains_key(*d)));
        }
        for d in deps {
            visit(d, targets, path, done)?;
        }
        path.pop();
        done.insert(t.clone());
        Ok(())
    }
    let mut done = IndexSet::new();
    for t in targets.keys() {
        visit(t, targets, &mut vec![], &mut done)?;
    }
    Ok(done.into_iter().collect())
}

struct Ctx<'a> {
    dims: &'a Dims,
    names: &'a HashMap<AnubisTarget, CmakeName>,
    paths: &'a PathRenderer,
    pic: &'a HashSet<AnubisTarget>,
    shared: &'a HashSet<AnubisTarget>,
    n_modes: usize,
}

/// A list to merge across modes: (mode, values).
type PerMode<T> = Vec<(usize, Vec<T>)>;

fn write_target(
    out: &mut String,
    t: &AnubisTarget,
    per_mode: &[Option<TargetData>],
    resolved: &[Option<Resolved>],
    ctx: &Ctx,
    source_langs: &mut HashMap<Utf8PathBuf, (CcLanguage, AnubisTarget)>,
) -> anyhow::Result<()> {
    let present: Vec<(usize, &TargetData, &Resolved)> = per_mode
        .iter()
        .zip(resolved)
        .enumerate()
        .filter_map(|(i, (d, r))| Some((i, d.as_ref()?, r.as_ref()?)))
        .collect();
    let within: BTreeSet<usize> = present.iter().map(|(i, _, _)| *i).collect();
    let first = present[0].1;
    for (_, d, _) in &present {
        if d.kind != first.kind
            || d.lang != first.lang
            || d.output_name != first.output_name
            || d.system_lib != first.system_lib
        {
            bail_loc!(
                "[{}] changes kind, lang, name or system library between modes, which CMake can't express",
                t
            );
        }
    }
    let name = &ctx.names[t];
    let real = &name.real;
    let any_srcs = present.iter().any(|(_, d, _)| !d.srcs.is_empty());
    if first.kind == Kind::Shared && !any_srcs {
        bail_loc!(
            "[{}] is a shared library without sources, which CMake can't build",
            t
        );
    }
    // A library without sources is only usage requirements (and links).
    let interface =
        matches!(first.kind, Kind::System | Kind::Provided) || (first.kind == Kind::Static && !any_srcs);
    let builds = !interface;
    let is_lib = first.kind != Kind::Exe;

    let _ = writeln!(out, "\n# {}", t);
    let guard = ctx.dims.target_if(t, &within, ctx.n_modes)?;
    if let Some(g) = &guard {
        let _ = writeln!(out, "if({})", g);
    }

    match first.kind {
        Kind::Exe => {
            let _ = writeln!(out, "add_executable({})", real);
        }
        _ if interface => {
            let _ = writeln!(out, "add_library({} INTERFACE)", real);
        }
        Kind::Static => {
            let _ = writeln!(out, "add_library({} STATIC)", real);
        }
        Kind::Shared => {
            let _ = writeln!(out, "add_library({} SHARED)", real);
            // CMake would add -D<target>_EXPORTS; Anubis defines nothing.
            let _ = writeln!(
                out,
                "set_target_properties({} PROPERTIES DEFINE_SYMBOL \"\")",
                real
            );
        }
        Kind::System | Kind::Provided => unreachable!(),
    }
    if is_lib {
        let _ = writeln!(out, "add_library({} ALIAS {})", name.alias, real);
    }
    if builds && (name.unique || first.output_name != first.name) {
        let _ = writeln!(
            out,
            "set_target_properties({} PROPERTIES OUTPUT_NAME \"{}\")",
            real,
            escape(&first.output_name)
        );
    }
    if builds && ctx.pic.contains(t) {
        let _ = writeln!(
            out,
            "set_target_properties({} PROPERTIES POSITION_INDEPENDENT_CODE ON)",
            real
        );
    }
    if first.kind == Kind::Provided {
        let _ = writeln!(out, "# Provided by the consumer's system (--system).");
    }
    if let Some(lib) = &first.system_lib {
        let _ = writeln!(
            out,
            "target_link_libraries({} INTERFACE \"{}\")",
            real,
            escape(lib)
        );
    }

    let lang = first.lang.unwrap_or(CcLanguage::Cpp);
    let merged = |per: PerMode<Value>| -> Vec<String> {
        merge(&per, &within, ctx.dims).into_iter().map(|(c, v)| v.render(c.as_deref())).collect()
    };

    // Sources. clang++ compiles a `.c` file as C++; CMake would use the C
    // compiler, so mark it. LANGUAGE is per directory, so a file can't be C++
    // in one target and C in another.
    if builds {
        let mut per: PerMode<Value> = vec![];
        let mut relang = BTreeSet::new();
        for (i, d, _) in &present {
            let mut v = vec![];
            for s in &d.srcs {
                let is_cxx_ext = s.extension().is_some_and(|e| CXX_EXTS.contains(&e));
                if lang == CcLanguage::C && is_cxx_ext {
                    bail_loc!(
                        "C target [{}] has C++ source [{}]; Anubis would compile it with C settings",
                        t,
                        s
                    );
                }
                if !is_asm(s) {
                    let src_lang = if lang == CcLanguage::Cpp {
                        CcLanguage::Cpp
                    } else {
                        CcLanguage::C
                    };
                    let key = normalize(s);
                    match source_langs.get(&key) {
                        Some((l, other))
                            if *l != src_lang && !(is_cxx_ext && src_lang == CcLanguage::Cpp) =>
                        {
                            bail_loc!("[{}] compiles as C in one target and C++ in another ([{}], [{}]); CMake can't", s, other, t);
                        }
                        _ => {
                            source_langs.insert(key, (src_lang, t.clone()));
                        }
                    }
                    if lang == CcLanguage::Cpp && !is_cxx_ext {
                        relang.insert(ctx.paths.path(s)?);
                    }
                }
                v.push(Value::user(&ctx.paths.path(s)?)?);
            }
            per.push((*i, v));
        }
        write_list(out, "target_sources", real, &[("PRIVATE", merged(per))]);
        if !relang.is_empty() {
            let files: Vec<String> = relang.iter().map(|p| format!("\"{}\"", escape(p))).collect();
            let _ = writeln!(
                out,
                "set_source_files_properties(\n  {}\n  PROPERTIES LANGUAGE CXX)",
                files.join("\n  ")
            );
        }
    }

    // Compile options, definitions, include dirs: PRIVATE is exactly what
    // Anubis passes this target's compiles; INTERFACE is what a dependent
    // gets from it.
    // Defines go in as -D options: CMake sorts COMPILE_DEFINITIONS, which
    // changes which value wins when a macro is defined twice.
    let flags = |args: &FlatArgs| -> anyhow::Result<Vec<(Value, Option<&'static str>)>> {
        let items = group_flags(&args.flags.iter().cloned().collect::<Vec<_>>());
        let mut v = items
            .iter()
            .map(|i| Ok((flag_value(i, ctx.paths)?, flag_language(i))))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for d in &args.defines {
            v.push((define_value(d)?, None));
        }
        Ok(v)
    };
    let flag_section = |get: &dyn Fn(
        &TargetData,
        &Resolved,
    ) -> anyhow::Result<Vec<(Value, Option<&'static str>)>>|
     -> anyhow::Result<Vec<String>> {
        // Merge (value, language) pairs, then fold the language into the condition.
        let mut per: PerMode<(Value, Option<&'static str>)> = vec![];
        for (i, d, r) in &present {
            per.push((*i, get(d, r)?));
        }
        Ok(merge(&per, &within, ctx.dims)
            .into_iter()
            .map(|(c, (v, l))| {
                let cond = match (l, c) {
                    (None, c) => c,
                    (Some(l), None) => Some(format!("$<COMPILE_LANGUAGE:{l}>")),
                    (Some(l), Some(c)) => Some(format!("$<AND:$<COMPILE_LANGUAGE:{l}>,{c}>")),
                };
                v.render(cond.as_deref())
            })
            .collect())
    };
    let mut sections = vec![];
    if builds {
        sections.push(("PRIVATE", flag_section(&|_, r| flags(&r.compile))?));
    }
    if is_lib {
        sections.push(("INTERFACE", flag_section(&|_, r| flags(&r.interface))?));
    }
    write_list(out, "target_compile_options", real, &sections);

    let section = |get: &dyn Fn(&Resolved) -> anyhow::Result<Vec<Value>>| -> anyhow::Result<Vec<String>> {
        let mut per: PerMode<Value> = vec![];
        for (i, _, r) in &present {
            per.push((*i, get(r)?));
        }
        Ok(merged(per))
    };

    let dirs = |set: &IndexSet<Utf8PathBuf>| -> anyhow::Result<Vec<Value>> {
        set.iter().map(|p| Value::user(&ctx.paths.path(p)?)).collect()
    };
    let mut sections = vec![];
    if builds {
        sections.push(("PRIVATE", section(&|r| dirs(&r.compile.include_dirs))?));
    }
    if is_lib {
        sections.push(("INTERFACE", section(&|r| dirs(&r.interface.include_dirs))?));
    }
    write_list(out, "target_include_directories", real, &sections);

    // Links. Deps go in as $<LINK_ONLY:..>: their args are already in the
    // lists above, in Anubis's order.
    let links = first.kind == Kind::Exe || first.kind == Kind::Shared;
    let link_value = |item: &LinkItem| -> anyhow::Result<Value> {
        match item {
            LinkItem::Target(dep) => Ok(Value {
                text: format!("$<LINK_ONLY:{}>", ctx.names[dep].real),
                genex: true,
            }),
            LinkItem::Library(p) => render_library(p, ctx.paths),
        }
    };
    let link_section = |get: &dyn Fn(&Resolved) -> &Vec<LinkItem>| -> anyhow::Result<Vec<String>> {
        let mut per: PerMode<Value> = vec![];
        for (i, _, r) in &present {
            per.push((
                *i,
                get(r).iter().map(link_value).collect::<anyhow::Result<Vec<_>>>()?,
            ));
        }
        Ok(merged(per))
    };
    let mut sections = vec![];
    if links {
        sections.push(("PRIVATE", section(&|r| dirs(&r.compile.library_dirs))?));
    }
    if is_lib {
        sections.push(("INTERFACE", section(&|r| dirs(&r.interface.library_dirs))?));
    }
    write_list(out, "target_link_directories", real, &sections);

    let mut sections = vec![];
    if links {
        sections.push(("PRIVATE", link_section(&|r| &r.link)?));
    }
    if is_lib {
        sections.push(("INTERFACE", link_section(&|r| &r.interface_link)?));
    }
    write_list(out, "target_link_libraries", real, &sections);

    // Windows finds DLLs next to the exe (Linux uses the build RPATH).
    let reaches_shared = present
        .iter()
        .any(|(_, _, r)| r.link.iter().any(|i| matches!(i, LinkItem::Target(d) if ctx.shared.contains(d))));
    if first.kind == Kind::Exe && reaches_shared {
        let _ = writeln!(
            out,
            "if(WIN32)\n  add_custom_command(TARGET {real} POST_BUILD\n    COMMAND \"${{CMAKE_COMMAND}}\" -E copy_if_different \"$<TARGET_RUNTIME_DLLS:{real}>\" \"$<TARGET_FILE_DIR:{real}>\"\n    COMMAND_EXPAND_LISTS)\nendif()"
        );
    }

    if guard.is_some() {
        let _ = writeln!(out, "endif()");
    }
    Ok(())
}

/// `ws2_32.lib` -> `ws2_32`; project files -> root-relative paths; other
/// names pass through for the linker to find.
fn render_library(p: &Utf8Path, paths: &PathRenderer) -> anyhow::Result<Value> {
    if p.is_absolute() {
        return Value::user(&paths.path(p)?);
    }
    let s = p.as_str();
    if !s.contains('/') && !s.contains('\\') {
        if let Some(stem) = s.strip_suffix(".lib") {
            return Value::user(stem);
        }
    }
    Value::user(s)
}

fn write_list(out: &mut String, cmd: &str, target: &str, sections: &[(&str, Vec<String>)]) {
    if sections.iter().all(|(_, v)| v.is_empty()) {
        return;
    }
    let _ = writeln!(out, "{}({}", cmd, target);
    for (kw, items) in sections {
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(out, "  {}", kw);
        for i in items {
            let _ = writeln!(out, "    {}", i);
        }
    }
    let _ = writeln!(out, ")");
}

#[cfg(test)]
mod tests;
