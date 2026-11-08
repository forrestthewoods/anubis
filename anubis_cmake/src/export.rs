//! Write a CMakeLists.txt that builds the given C/C++
//! targets with the consumer's own compiler, for projects that want to ship
//! Anubis builds to CMake users without maintaining CMake by hand.
//!
//! Experimental, and deliberately its own crate: Anubis itself knows
//! nothing about CMake. The exporter only reads rules the way a build
//! would (`Anubis::get_rule` for each mode) and never runs a build.
//!
//! What it does:
//! - Resolves the targets and their deps once per mode (`-m`, e.g. the four
//!   linux/windows × dev/release modes), then merges the modes: a value found
//!   in every mode is unconditional, the rest get a generator expression on
//!   the mode variables that tell them apart (`target_platform` becomes
//!   `$<PLATFORM_ID:..>`, `build_type` becomes `$<CONFIG:..>`).
//! - `cc_static_library` / `cc_shared_library` / `cc_binary` become
//!   `add_library` / `add_executable`; public fields become PUBLIC (or
//!   INTERFACE for a library without sources), so they propagate to deps
//!   transitively the way Anubis does. `ifs_stub_library` becomes a link to
//!   the system library it stands for. `zig_glibc` (toolchain plumbing) is
//!   skipped. Every other rule type is an error.
//! - The toolchain is not exported: the consumer brings their own compiler,
//!   sysroot, optimization and CRT. The one exception is its `-std=` flag,
//!   since sources depend on it.
//! - Flags pass through unchanged for GCC/Clang. For clang-cl each flag gets
//!   `/clang:` in front. MSVC's own cl.exe takes different flags, so the
//!   generated file stops with an error there rather than guess.
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
use std::sync::Arc;

pub struct ExportOptions {
    pub modes: Vec<String>,
    pub targets: Vec<String>,
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
    let modes = opts
        .modes
        .iter()
        .map(|m| anubis.get_mode(&AnubisTarget::new(m)?))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let dims = Dims::new(&modes)?;
    let roots = anubis.expand_targets(&opts.targets)?;

    // Resolve every reachable target once per mode.
    let mut targets: IndexMap<AnubisTarget, Vec<Option<TargetData>>> = IndexMap::new();
    let mut dropped_defines: IndexSet<String> = IndexSet::new();
    for (mi, mode) in modes.iter().enumerate() {
        let toolchain = anubis.get_toolchain(mode, &AnubisTarget::new(DEFAULT_TOOLCHAIN)?)?;
        for cc in toolchain.c.iter().chain(toolchain.cpp.iter()) {
            dropped_defines.extend(cc.defines.iter().cloned());
        }
        let std_flag = |lang: CcLanguage| -> Option<String> {
            let cc = match lang {
                CcLanguage::C => toolchain.c.as_ref(),
                CcLanguage::Cpp => toolchain.cpp.as_ref(),
            }?;
            cc.compiler_flags.iter().filter(|f| f.starts_with("-std=")).last().cloned()
        };

        let mut stack: Vec<AnubisTarget> = roots.iter().rev().cloned().collect();
        let mut seen: HashSet<AnubisTarget> = HashSet::new();
        while let Some(t) = stack.pop() {
            if !seen.insert(t.clone()) {
                continue;
            }
            let rule = anubis.get_rule(&t, mode)?;
            let Some(data) = TargetData::from_rule(&t, rule, &std_flag)? else {
                continue;
            };
            stack.extend(data.deps.iter().rev().cloned());
            targets.entry(t.clone()).or_insert_with(|| vec![None; modes.len()])[mi] = Some(data);
        }
    }

    // Skipped targets (zig_glibc) never got data; drop them from dep lists.
    let exported: HashSet<AnubisTarget> = targets.keys().cloned().collect();
    let names = cmake_names(targets.keys(), &opts.namespace);

    let out_dir = opts
        .output
        .parent()
        .ok_or_else(|| anyhow_loc!("no parent dir for [{}]", opts.output))?;
    let paths = PathRenderer::new(&anubis.root, out_dir)?;

    let mut out = String::new();
    // Preprocessed assembly (`.S`) needs CMake's ASM language.
    let asm = targets.values().flatten().flatten().any(|d| d.srcs.iter().any(|s| is_asm(s)));
    write_header(&mut out, opts, &modes, &paths, asm);

    // Static libraries (with sources), for shared-library link decisions.
    let statics: HashSet<AnubisTarget> = targets
        .iter()
        .filter(|(_, pm)| pm.iter().flatten().next().is_some_and(|d| d.kind == Kind::Static && !d.srcs.is_empty()))
        .map(|(t, _)| t.clone())
        .collect();

    let shared: HashSet<AnubisTarget> = targets
        .iter()
        .filter(|(_, pm)| pm.iter().flatten().any(|d| d.kind == Kind::Shared))
        .map(|(t, _)| t.clone())
        .collect();

    let deps = targets
        .iter()
        .map(|(t, pm)| (t.clone(), pm.iter().flatten().flat_map(|d| d.deps.iter().cloned()).collect()))
        .collect();
    let graph = Graph { names, statics, shared, deps };

    // Deps before dependents, so the file reads bottom-up like a build.
    for t in topo_order(&targets, &exported) {
        let per_mode = &targets[&t];
        write_target(&mut out, &t, per_mode, &dims, &graph, &paths)?;
    }
    check_std_conflicts(&targets, &modes)?;
    if !dropped_defines.is_empty() {
        // Not an error: most are the toolchain's own business (libc++
        // configuration, CRT, NDEBUG). But code that needs one (say
        // `_GNU_SOURCE`) should state it on its target.
        eprintln!(
            "note: toolchain defines are not exported; if a target needs one, add it to the target: {}",
            dropped_defines.iter().cloned().collect::<Vec<_>>().join(" ")
        );
    }
    Ok(out)
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
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Fields {
    /// Compiler flags, one entry per flag; a flag and its separate argument
    /// (`-include foo.h`) stay together as one entry.
    flags: Vec<Vec<String>>,
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
    /// The toolchain's `-std=` for this target's language.
    toolchain_std: Option<String>,
    /// `ifs_stub_library`: the system library name (`X11` for libX11.so.6).
    system_lib: Option<String>,
}

impl TargetData {
    fn from_rule(
        t: &AnubisTarget,
        rule: Arc<dyn anubis::Rule>,
        std_flag: &dyn Fn(CcLanguage) -> Option<String>,
    ) -> anyhow::Result<Option<TargetData>> {
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
                        flags: group_flags(&lib.public_compiler_flags),
                        defines: lib.public_defines.clone(),
                        include_dirs: lib.public_include_dirs.clone(),
                        libraries: lib.public_libraries.clone(),
                        library_dirs: lib.public_library_dirs.clone(),
                    },
                    private: Fields {
                        flags: group_flags(&lib.private_compiler_flags),
                        defines: lib.private_defines.clone(),
                        include_dirs: lib.private_include_dirs.clone(),
                        libraries: vec![],
                        library_dirs: vec![],
                    },
                    toolchain_std: std_flag(lib.lang),
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
                        flags: group_flags(&bin.compiler_flags),
                        defines: bin.compiler_defines.clone(),
                        include_dirs: bin.include_dirs.clone(),
                        libraries: bin.libraries.clone(),
                        library_dirs: bin.library_dirs.clone(),
                    },
                    toolchain_std: std_flag(bin.lang),
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
                    toolchain_std: None,
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
             cc_shared_library and ifs_stub_library are supported)",
            t
        )
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

/// Keep a flag together with a separate argument (`-include foo.h`,
/// `-Xclang -foo`), so merging and CMake's option de-duplication can't
/// split them.
fn group_flags(flags: &[String]) -> Vec<Vec<String>> {
    const TAKES_ARG: &[&str] = &[
        "-include", "-imacros", "-isystem", "-iquote", "-idirafter", "-Xclang", "-mllvm", "-x", "-target",
        "-Xpreprocessor", "-arch",
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

// ----------------------------------------------------------------------------
// Mode dimensions -> CMake conditions
// ----------------------------------------------------------------------------
/// The mode variables that differ between the exported modes, and how each
/// maps to a CMake condition.
struct Dims {
    /// (variable, value per mode); only variables that vary.
    vars: Vec<(String, Vec<String>)>,
    build_types: BTreeSet<String>,
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
        let mut vars = vec![];
        for k in keys {
            let values: Vec<String> = modes.iter().map(|m| m.vars.get(k).cloned().unwrap_or_default()).collect();
            if values.iter().all(|v| v == &values[0]) {
                continue;
            }
            match k.as_str() {
                "target_platform" => {
                    for v in &values {
                        platform_id(v)?;
                    }
                }
                "build_type" => {}
                _ => bail_loc!(
                    "mode variable [{}] differs between the modes ({}); only target_platform \
                     and build_type can become CMake conditions. Export modes that agree on it.",
                    k,
                    values.join(", ")
                ),
            }
            vars.push((k.clone(), values));
        }
        let build_types = modes.iter().filter_map(|m| m.vars.get("build_type").cloned()).collect();
        Ok(Dims { vars, build_types })
    }

    /// The generator-expression condition for one variable's value.
    fn genex(&self, var: &str, value: &str) -> String {
        match var {
            "target_platform" => format!("$<PLATFORM_ID:{}>", platform_id(value).unwrap()),
            "build_type" => {
                let only_debug_release =
                    self.build_types.iter().all(|b| b == "debug" || b == "release") && self.build_types.len() == 2;
                match value {
                    // CMake has four configs; treat every non-Debug one as release.
                    "release" if only_debug_release => "$<NOT:$<CONFIG:Debug>>".to_string(),
                    _ => format!("$<CONFIG:{}>", capitalize(value)),
                }
            }
            _ => unreachable!("Dims::new rejects other variables"),
        }
    }

    /// A configure-time `if()` condition for a value, where one exists.
    fn if_cond(&self, var: &str, value: &str) -> Option<String> {
        match var {
            "target_platform" => Some(format!("CMAKE_SYSTEM_NAME STREQUAL \"{}\"", platform_id(value).unwrap())),
            _ => None,
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
            let matches: BTreeSet<usize> = within.iter().copied().filter(|&m| picked.contains(&values[m])).collect();
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
    fn target_if(&self, t: &AnubisTarget, subset: &BTreeSet<usize>, all: usize) -> anyhow::Result<Option<String>> {
        let all: BTreeSet<usize> = (0..all).collect();
        if subset == &all {
            return Ok(None);
        }
        if let Some((var, values)) = self.vars.iter().find(|(v, _)| v == "target_platform") {
            let picked: BTreeSet<&String> = subset.iter().map(|&m| &values[m]).collect();
            let matches: BTreeSet<usize> = all.iter().copied().filter(|&m| picked.contains(&values[m])).collect();
            if &matches == subset {
                let conds: Vec<String> = picked.iter().filter_map(|v| self.if_cond(var, v)).collect();
                return Ok(Some(conds.join(" OR ")));
            }
        }
        bail_loc!(
            "[{}] is a dependency in some modes but not others, and target_platform alone \
             doesn't explain which; CMake can't make a target exist per build type",
            t
        )
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

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn or(mut conds: Vec<String>) -> String {
    if conds.len() == 1 {
        conds.pop().unwrap()
    } else {
        format!("$<OR:{}>", conds.join(","))
    }
}

fn and(mut conds: Vec<String>) -> String {
    if conds.len() == 1 {
        conds.pop().unwrap()
    } else {
        format!("$<AND:{}>", conds.join(","))
    }
}

/// Merge one field across modes: each distinct value once, in first-seen
/// order, with the condition under which it applies (None = always).
fn merge<T: Clone + Eq + std::hash::Hash>(
    per_mode: &[(usize, Vec<T>)],
    within: &BTreeSet<usize>,
    dims: &Dims,
) -> Vec<(Option<String>, T)> {
    let mut seen: IndexMap<T, BTreeSet<usize>> = IndexMap::new();
    for (m, values) in per_mode {
        for v in values {
            seen.entry(v.clone()).or_default().insert(*m);
        }
    }
    seen.into_iter().map(|(v, modes)| (dims.condition(&modes, within), v)).collect()
}

// ----------------------------------------------------------------------------
// Names, paths, quoting
// ----------------------------------------------------------------------------
/// What writing one target needs to know about the others.
struct Graph {
    names: HashMap<AnubisTarget, CmakeName>,
    /// Static libraries with sources.
    statics: HashSet<AnubisTarget>,
    shared: HashSet<AnubisTarget>,
    /// Deps in any mode.
    deps: HashMap<AnubisTarget, Vec<AnubisTarget>>,
}

struct CmakeName {
    real: String,
    alias: String,
    /// The name is unique among the exported targets, so the output file
    /// can keep the Anubis name (`libfoo.a`, not `lib<ns>_foo.a`).
    unique: bool,
}

/// `<ns>_<name>`, aliased as `<ns>::<name>`. Names that occur in more than
/// one directory get the directory mangled in.
fn cmake_names<'a>(targets: impl Iterator<Item = &'a AnubisTarget>, ns: &str) -> HashMap<AnubisTarget, CmakeName> {
    let targets: Vec<&AnubisTarget> = targets.collect();
    let mut count: HashMap<&str, usize> = HashMap::new();
    for t in &targets {
        *count.entry(t.target_name()).or_default() += 1;
    }
    let sanitize = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect::<String>();
    targets
        .into_iter()
        .map(|t| {
            let unique = count[t.target_name()] == 1;
            let base = if !unique {
                format!("{}_{}", sanitize(t.get_relative_dir().trim_start_matches('/')), t.target_name())
            } else {
                t.target_name().to_string()
            };
            let base = sanitize(&base);
            (t.clone(), CmakeName { real: format!("{}_{}", ns, base), alias: format!("{}::{}", ns, base), unique })
        })
        .collect()
}

struct PathRenderer {
    root: Utf8PathBuf,
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
        Ok(PathRenderer { root, root_expr })
    }

    /// A path under the project root, as `${_anubis_root}/rel`.
    fn path(&self, p: &Utf8Path) -> anyhow::Result<String> {
        let p = normalize(p);
        let rel = p.strip_prefix(&self.root).map_err(|_| {
            anyhow_loc!("[{}] is outside the project root; the export can only refer to project files", p)
        })?;
        Ok(format!("${{_anubis_root}}/{}", escape(rel.as_str())))
    }

    /// A flag token: absolute project paths become root-relative; other
    /// absolute paths (toolchain files) can't be exported.
    fn flag_token(&self, tok: &str) -> anyhow::Result<String> {
        let p = Utf8Path::new(tok);
        if p.is_absolute() {
            return self.path(p);
        }
        Ok(escape(tok))
    }
}

fn normalize(p: &Utf8Path) -> Utf8PathBuf {
    Utf8PathBuf::from(anubis::util::normalize_path(p).as_str().replace('\\', "/"))
}

/// Escape for a quoted CMake argument, keeping it one list element.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('$', "\\$").replace(';', "\\;")
}

/// Escape characters that end or split a generator expression.
fn escape_genex(s: &str) -> String {
    s.replace('>', "$<ANGLE-R>").replace(',', "$<COMMA>").replace("\\;", "$<SEMICOLON>")
}

fn conditional(cond: &Option<String>, value: String) -> String {
    match cond {
        None => format!("\"{}\"", value),
        Some(c) => format!("\"$<{}:{}>\"", c, escape_genex(&value)),
    }
}

// ----------------------------------------------------------------------------
// Writing
// ----------------------------------------------------------------------------
fn is_asm(p: &Utf8Path) -> bool {
    matches!(p.extension(), Some("S" | "s" | "sx"))
}

fn write_header(out: &mut String, opts: &ExportOptions, modes: &[Arc<Mode>], paths: &PathRenderer, asm: bool) {
    let mode_names: Vec<&str> = modes.iter().map(|m| m.target.target_path()).collect();
    let _ = write!(
        out,
        r#"# GENERATED by `anubis-cmake`. Do not edit; regenerate with:
#   {cmd}
# Modes merged: {modes}
#
# Builds with your own GCC or Clang (clang-cl too). Use as a subdirectory
# (add_subdirectory / FetchContent) and link the `{ns}::` targets.

cmake_minimum_required(VERSION 3.21)
project({ns} LANGUAGES C CXX{asm})

set(_anubis_root "{root}")

# Flags are GCC/Clang flags. clang-cl gets each behind /clang:. cl.exe takes
# different flags altogether, so it isn't supported.
foreach(_lang C CXX)
  if(CMAKE_${{_lang}}_COMPILER_ID STREQUAL "MSVC")
    message(FATAL_ERROR "{ns}: MSVC's cl.exe isn't supported; use Clang (Visual Studio: -T ClangCL).")
  endif()
  if(CMAKE_${{_lang}}_COMPILER_FRONTEND_VARIANT STREQUAL "MSVC")
    set(_anubis_${{_lang}}_flag "/clang:")
  else()
    set(_anubis_${{_lang}}_flag "")
  endif()
endforeach()
"#,
        cmd = opts.command_line,
        modes = mode_names.join(" "),
        ns = opts.namespace,
        root = paths.root_expr,
        asm = if asm { " ASM" } else { "" },
    );
}

fn topo_order(targets: &IndexMap<AnubisTarget, Vec<Option<TargetData>>>, exported: &HashSet<AnubisTarget>) -> Vec<AnubisTarget> {
    fn visit(
        t: &AnubisTarget,
        targets: &IndexMap<AnubisTarget, Vec<Option<TargetData>>>,
        exported: &HashSet<AnubisTarget>,
        done: &mut IndexSet<AnubisTarget>,
    ) {
        if done.contains(t) {
            return;
        }
        let mut deps: IndexSet<&AnubisTarget> = IndexSet::new();
        for d in targets[t].iter().flatten() {
            deps.extend(d.deps.iter().filter(|d| exported.contains(*d)));
        }
        for d in deps {
            visit(d, targets, exported, done);
        }
        done.insert(t.clone());
    }
    let mut done = IndexSet::new();
    for t in targets.keys() {
        visit(t, targets, exported, &mut done);
    }
    done.into_iter().collect()
}

fn write_target(
    out: &mut String,
    t: &AnubisTarget,
    per_mode: &[Option<TargetData>],
    dims: &Dims,
    g: &Graph,
    paths: &PathRenderer,
) -> anyhow::Result<()> {
    let names = &g.names;
    let present: Vec<(usize, &TargetData)> = per_mode.iter().enumerate().filter_map(|(i, d)| d.as_ref().map(|d| (i, d))).collect();
    let within: BTreeSet<usize> = present.iter().map(|(i, _)| *i).collect();
    let first = present[0].1;
    for (_, d) in &present {
        if d.kind != first.kind || d.lang != first.lang || d.output_name != first.output_name {
            bail_loc!("[{}] changes kind, lang or name between modes, which CMake can't express", t);
        }
    }
    let name = &names[t];
    let real = &name.real;
    let reaches_shared = {
        let mut stack: Vec<&AnubisTarget> = present.iter().flat_map(|(_, d)| d.deps.iter()).collect();
        let mut seen = HashSet::new();
        let mut found = false;
        while let Some(d) = stack.pop() {
            if !seen.insert(d.clone()) {
                continue;
            }
            if g.shared.contains(d) {
                found = true;
                break;
            }
            if let Some(deps) = g.deps.get(d) {
                stack.extend(deps.iter());
            }
        }
        found
    };
    let any_srcs = present.iter().any(|(_, d)| !d.srcs.is_empty());
    let interface = first.kind == Kind::System || (first.kind != Kind::Exe && !any_srcs);
    let public_kw = if interface { "INTERFACE" } else { "PUBLIC" };

    let _ = writeln!(out, "\n# {}", t);
    let guard = dims.target_if(t, &within, per_mode.len())?;
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
        }
        Kind::System => unreachable!(),
    }
    if first.kind != Kind::Exe {
        let _ = writeln!(out, "add_library({} ALIAS {})", name.alias, real);
    }
    if !interface && (name.unique || first.output_name != first.name) {
        let _ = writeln!(out, "set_target_properties({} PROPERTIES OUTPUT_NAME \"{}\")", real, escape(&first.output_name));
    }

    if let Some(lib) = &first.system_lib {
        let _ = writeln!(out, "target_link_libraries({} INTERFACE \"{}\")", real, escape(lib));
    }

    let lang = first.lang.unwrap_or(CcLanguage::Cpp);
    let (cmake_lang, other_exts): (&str, &[&str]) = match lang {
        CcLanguage::C => ("C", &["cpp", "cc", "cxx", "c++", "C"]),
        CcLanguage::Cpp => ("CXX", &["c"]),
    };

    // Sources. Anubis compiles every source with the target's language.
    let srcs = merge(&present.iter().map(|(i, d)| (*i, d.srcs.clone())).collect::<Vec<_>>(), &within, dims);
    if !srcs.is_empty() && !interface {
        let mut lines = vec![];
        let mut relang = vec![];
        for (c, s) in &srcs {
            let p = paths.path(s)?;
            if s.extension().is_some_and(|e| other_exts.contains(&e)) {
                relang.push(format!("\"{}\"", p));
            }
            lines.push(conditional(c, p));
        }
        write_list(out, "target_sources", real, &[("PRIVATE", lines)]);
        if !relang.is_empty() {
            let _ = writeln!(out, "set_source_files_properties(\n  {}\n  PROPERTIES LANGUAGE {})", relang.join("\n  "), cmake_lang);
        }
    }

    // Each field, public then private.
    let field = |get: &dyn Fn(&TargetData) -> Vec<String>| -> Vec<(Option<String>, String)> {
        merge(&present.iter().map(|(i, d)| (*i, get(d))).collect::<Vec<_>>(), &within, dims)
    };
    let render = |items: Vec<(Option<String>, String)>| -> Vec<String> {
        items.into_iter().map(|(c, v)| conditional(&c, v)).collect()
    };

    // Compile options: owner's language only, so a C++ library's public
    // `-std=` doesn't reach a consumer's C files.
    let flag_items = |f: &Fields, extra: Option<&String>| -> anyhow::Result<Vec<String>> {
        let mut v = vec![];
        if let Some(s) = extra {
            v.push(format!("${{_anubis_{}_flag}}{}", cmake_lang, escape(s)));
        }
        for item in &f.flags {
            let toks = item
                .iter()
                .map(|tok| Ok(format!("${{_anubis_{}_flag}}{}", cmake_lang, paths.flag_token(tok)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            v.push(if toks.len() == 1 { toks[0].clone() } else { format!("SHELL:{}", toks.join(" ")) });
        }
        Ok(v)
    };
    let mut pub_flags = vec![];
    let mut priv_flags = vec![];
    for (i, d) in &present {
        pub_flags.push((*i, flag_items(&d.public, None)?));
        priv_flags.push((*i, flag_items(&d.private, d.toolchain_std.as_ref())?));
    }
    let lang_wrap = |items: Vec<(Option<String>, String)>| -> Vec<String> {
        items
            .into_iter()
            .map(|(c, v)| {
                let l = format!("$<COMPILE_LANGUAGE:{}>", cmake_lang);
                let cond = Some(match c {
                    None => l,
                    Some(c) => format!("$<AND:{},{}>", l, c),
                });
                conditional(&cond, v)
            })
            .collect()
    };
    let mut sections = vec![(public_kw, lang_wrap(merge(&pub_flags, &within, dims)))];
    if !interface {
        sections.push(("PRIVATE", lang_wrap(merge(&priv_flags, &within, dims))));
    }
    write_list(out, "target_compile_options", real, &sections);

    let defs = |f: fn(&TargetData) -> &Fields| field(&|d| f(d).defines.iter().map(|s| escape(s)).collect());
    let mut sections = vec![(public_kw, render(defs(|d| &d.public)))];
    if !interface {
        sections.push(("PRIVATE", render(defs(|d| &d.private))));
    }
    write_list(out, "target_compile_definitions", real, &sections);

    let incs = |f: fn(&TargetData) -> &Fields| -> anyhow::Result<Vec<(Option<String>, String)>> {
        let mut per = vec![];
        for (i, d) in &present {
            per.push((*i, f(d).include_dirs.iter().map(|p| paths.path(p)).collect::<anyhow::Result<Vec<_>>>()?));
        }
        Ok(merge(&per, &within, dims))
    };
    let mut sections = vec![(public_kw, render(incs(|d| &d.public)?))];
    if !interface {
        sections.push(("PRIVATE", render(incs(|d| &d.private)?)));
    }
    write_list(out, "target_include_directories", real, &sections);

    let lib_dirs = |f: fn(&TargetData) -> &Fields| -> anyhow::Result<Vec<(Option<String>, String)>> {
        let mut per = vec![];
        for (i, d) in &present {
            per.push((*i, f(d).library_dirs.iter().map(|p| paths.path(p)).collect::<anyhow::Result<Vec<_>>>()?));
        }
        Ok(merge(&per, &within, dims))
    };
    let mut sections = vec![(public_kw, render(lib_dirs(|d| &d.public)?))];
    if !interface {
        sections.push(("PRIVATE", render(lib_dirs(|d| &d.private)?)));
    }
    write_list(out, "target_link_directories", real, &sections);

    // Links: deps, then libraries. How a dep propagates depends on what we
    // are: everything a static library links reaches its dependents; a
    // shared library links its static deps into itself and passes on only
    // their usage requirements (COMPILE_ONLY, CMake 3.27+).
    let mut dep_lists: Vec<(usize, Vec<(AnubisTarget, bool)>)> = vec![];
    for (i, d) in &present {
        let deps = d
            .deps
            .iter()
            .filter(|dep| g.names.contains_key(*dep))
            .map(|dep| (dep.clone(), true))
            .collect();
        dep_lists.push((*i, deps));
    }
    let deps = merge(&dep_lists, &within, dims);
    let mut public_links = vec![];
    let mut private_links = vec![];
    let mut compile_only = vec![];
    for (c, (dep, _)) in deps {
        let dep_name = &names[&dep].real;
        match first.kind {
            Kind::Exe => private_links.push(conditional(&c, dep_name.clone())),
            Kind::Shared => {
                // A dep that is itself a static library is linked in privately.
                if g.statics.contains(&dep) {
                    private_links.push(conditional(&c, dep_name.clone()));
                    compile_only.push(conditional(&c, format!("$<COMPILE_ONLY:{}>", dep_name)));
                } else {
                    public_links.push(conditional(&c, dep_name.clone()));
                }
            }
            _ => public_links.push(conditional(&c, dep_name.clone())),
        }
    }
    let libs = |f: fn(&TargetData) -> &Fields| -> anyhow::Result<Vec<(Option<String>, String)>> {
        let mut per = vec![];
        for (i, d) in &present {
            per.push((*i, f(d).libraries.iter().map(|p| render_library(p, paths)).collect::<anyhow::Result<Vec<_>>>()?));
        }
        Ok(merge(&per, &within, dims))
    };
    public_links.extend(render(libs(|d| &d.public)?));
    private_links.extend(render(libs(|d| &d.private)?));
    let pk = if first.kind == Kind::Exe { "PRIVATE" } else { public_kw };
    let mut sections = vec![(pk, public_links)];
    if !interface {
        sections.push(("PRIVATE", private_links));
    }
    write_list(out, "target_link_libraries", real, &sections);
    if !compile_only.is_empty() {
        let _ = writeln!(out, "if(CMAKE_VERSION VERSION_GREATER_EQUAL 3.27)");
        write_list(out, "  target_link_libraries", real, &[("INTERFACE", compile_only)]);
        let _ = writeln!(out, "endif()");
    }

    // Windows finds DLLs next to the exe (Linux uses the build RPATH).
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
fn render_library(p: &Utf8Path, paths: &PathRenderer) -> anyhow::Result<String> {
    if p.is_absolute() {
        return paths.path(p);
    }
    let s = p.as_str();
    if !s.contains('/') && !s.contains('\\') {
        if let Some(stem) = s.strip_suffix(".lib") {
            return Ok(escape(stem));
        }
    }
    Ok(escape(s))
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

/// CMake puts a target's own options before those it gets from deps; Anubis
/// puts deps' public flags first. That only changes the result when both set
/// `-std=` differently, so refuse that case.
fn check_std_conflicts(targets: &IndexMap<AnubisTarget, Vec<Option<TargetData>>>, modes: &[Arc<Mode>]) -> anyhow::Result<()> {
    for (mi, mode) in modes.iter().enumerate() {
        for (t, per_mode) in targets {
            let Some(d) = &per_mode[mi] else { continue };
            let own: BTreeSet<&String> = d.public.flags.iter().chain(&d.private.flags).flatten().filter(|f| f.starts_with("-std=")).collect();
            if own.is_empty() {
                continue;
            }
            // Transitive public -std= from deps.
            let mut stack: Vec<&AnubisTarget> = d.deps.iter().collect();
            let mut seen = HashSet::new();
            while let Some(dep) = stack.pop() {
                if !seen.insert(dep) {
                    continue;
                }
                let Some(Some(dd)) = targets.get(dep).map(|v| &v[mi]) else { continue };
                for f in dd.public.flags.iter().flatten().filter(|f| f.starts_with("-std=")) {
                    if !own.contains(f) {
                        bail_loc!(
                            "in {} [{}] sets {} but its dep [{}] passes on {}; Anubis and CMake \
                             order those differently, so the export would compile with the wrong standard",
                            mode.name,
                            t,
                            own.iter().next().unwrap(),
                            dep,
                            f
                        );
                    }
                }
                stack.extend(dd.deps.iter());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(name: &str, platform: &str, build_type: &str) -> Arc<Mode> {
        let vars = [("target_platform", platform), ("build_type", build_type), ("target_arch", "x64")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(Mode { name: name.into(), vars, target: AnubisTarget::default() })
    }

    fn four_modes() -> Dims {
        Dims::new(&[
            mode("linux_dev", "linux", "debug"),
            mode("linux_release", "linux", "release"),
            mode("win_dev", "windows", "debug"),
            mode("win_release", "windows", "release"),
        ])
        .unwrap()
    }

    fn set(v: &[usize]) -> BTreeSet<usize> {
        v.iter().copied().collect()
    }

    #[test]
    fn condition_picks_the_variable_that_explains_the_modes() {
        let d = four_modes();
        let all = set(&[0, 1, 2, 3]);
        assert_eq!(d.condition(&all, &all), None);
        assert_eq!(d.condition(&set(&[2, 3]), &all).as_deref(), Some("$<PLATFORM_ID:Windows>"));
        assert_eq!(d.condition(&set(&[0, 2]), &all).as_deref(), Some("$<CONFIG:Debug>"));
        assert_eq!(d.condition(&set(&[1, 3]), &all).as_deref(), Some("$<NOT:$<CONFIG:Debug>>"));
        assert_eq!(
            d.condition(&set(&[3]), &all).as_deref(),
            Some("$<AND:$<PLATFORM_ID:Windows>,$<NOT:$<CONFIG:Debug>>>")
        );
        // Within the Linux-only modes, a debug-only value needs only CONFIG.
        assert_eq!(d.condition(&set(&[0]), &set(&[0, 1])).as_deref(), Some("$<CONFIG:Debug>"));
    }

    #[test]
    fn unknown_varying_mode_variables_are_refused() {
        let mut a = (*mode("a", "linux", "debug")).clone();
        a.vars.insert("fp".into(), "32".into());
        let mut b = (*mode("b", "linux", "debug")).clone();
        b.vars.insert("fp".into(), "64".into());
        assert!(Dims::new(&[Arc::new(a), Arc::new(b)]).is_err());
    }

    #[test]
    fn targets_only_on_one_platform_get_an_if() {
        let d = four_modes();
        let t = AnubisTarget::new("//a:b").unwrap();
        assert_eq!(d.target_if(&t, &set(&[0, 1]), 4).unwrap().as_deref(), Some("CMAKE_SYSTEM_NAME STREQUAL \"Linux\""));
        assert!(d.target_if(&t, &set(&[0, 2]), 4).is_err());
    }

    #[test]
    fn flags_keep_their_arguments() {
        let f: Vec<String> = ["-include", "a.h", "-O2", "-Xclang", "-foo"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            group_flags(&f),
            vec![vec!["-include".to_string(), "a.h".into()], vec!["-O2".into()], vec!["-Xclang".into(), "-foo".into()]]
        );
    }

    #[test]
    fn merge_keeps_first_seen_order() {
        let d = four_modes();
        let all = set(&[0, 1, 2, 3]);
        let per = vec![(0, vec!["a", "l"]), (1, vec!["a", "l"]), (2, vec!["w", "a"]), (3, vec!["w", "a"])];
        let m = merge(&per, &all, &d);
        assert_eq!(
            m,
            vec![
                (None, "a"),
                (Some("$<PLATFORM_ID:Linux>".to_string()), "l"),
                (Some("$<PLATFORM_ID:Windows>".to_string()), "w"),
            ]
        );
    }

    #[test]
    fn escaping() {
        assert_eq!(escape(r#"A="b;c""#), r#"A=\"b\;c\""#);
        assert_eq!(conditional(&Some("$<C>".into()), "x,y>z".into()), "\"$<$<C>:x$<COMMA>y$<ANGLE-R>z>\"");
    }
}
