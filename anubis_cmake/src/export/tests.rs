use super::*;

fn mode(name: &str, platform: &str, build_type: &str) -> Arc<Mode> {
    let vars = [
        ("target_platform", platform),
        ("build_type", build_type),
        ("target_arch", "x64"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Arc::new(Mode {
        name: name.into(),
        vars,
        target: AnubisTarget::default(),
    })
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

fn t(s: &str) -> AnubisTarget {
    AnubisTarget::new(s).unwrap()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn lib(kind: Kind, srcs: &[&str], deps: &[&str], public: Fields) -> TargetData {
    TargetData {
        kind,
        lang: Some(CcLanguage::Cpp),
        name: "x".into(),
        output_name: "x".into(),
        srcs: srcs.iter().map(Utf8PathBuf::from).collect(),
        deps: deps.iter().map(|d| t(d)).collect(),
        public,
        private: Fields::default(),
        toolchain_std: None,
        system_lib: None,
    }
}

fn flags(f: &[&str]) -> Fields {
    Fields {
        flags: strs(f),
        ..Default::default()
    }
}

// --- conditions -------------------------------------------------------------

#[test]
fn condition_picks_the_variable_that_explains_the_modes() {
    let d = four_modes();
    let all = set(&[0, 1, 2, 3]);
    assert_eq!(d.condition(&all, &all), None);
    assert_eq!(
        d.condition(&set(&[2, 3]), &all).as_deref(),
        Some("$<PLATFORM_ID:Windows>")
    );
    assert_eq!(
        d.condition(&set(&[0, 2]), &all).as_deref(),
        Some("$<CONFIG:Debug>")
    );
    assert_eq!(
        d.condition(&set(&[1, 3]), &all).as_deref(),
        Some("$<NOT:$<CONFIG:Debug>>")
    );
    assert_eq!(
        d.condition(&set(&[3]), &all).as_deref(),
        Some("$<AND:$<PLATFORM_ID:Windows>,$<NOT:$<CONFIG:Debug>>>")
    );
    // Within the Linux-only modes, a debug-only value needs only CONFIG.
    assert_eq!(
        d.condition(&set(&[0]), &set(&[0, 1])).as_deref(),
        Some("$<CONFIG:Debug>")
    );
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
fn user_variables_named_build_something_are_not_ignored() {
    // Only build_platform/build_arch follow target_*; build_flavor is the user's.
    let mut a = (*mode("a", "linux", "debug")).clone();
    a.vars.insert("build_flavor".into(), "a".into());
    let mut b = (*mode("b", "linux", "debug")).clone();
    b.vars.insert("build_flavor".into(), "b".into());
    assert!(Dims::new(&[Arc::new(a), Arc::new(b)]).is_err());
}

#[test]
fn build_types_other_than_debug_and_release_are_refused() {
    assert!(Dims::new(&[mode("a", "linux", "profile"), mode("b", "linux", "debug")]).is_err());
}

#[test]
fn targets_only_on_one_platform_get_an_if() {
    let d = four_modes();
    assert_eq!(
        d.target_if(&t("//a:b"), &set(&[0, 1]), 4).unwrap().as_deref(),
        Some("CMAKE_SYSTEM_NAME STREQUAL \"Linux\"")
    );
    assert!(d.target_if(&t("//a:b"), &set(&[0, 2]), 4).is_err());
}

// --- merging keeps every mode's order ----------------------------------------

#[test]
fn merge_keeps_each_modes_order() {
    let d = four_modes();
    let all = set(&[0, 1, 2, 3]);
    // Windows lists w before a; Linux lists a before l. Merged: w, a, l.
    let per = vec![
        (0, vec!["a", "l"]),
        (1, vec!["a", "l"]),
        (2, vec!["w", "a"]),
        (3, vec!["w", "a"]),
    ];
    let m = merge(&per, &all, &d);
    assert_eq!(
        m,
        vec![
            (Some("$<PLATFORM_ID:Windows>".to_string()), "w"),
            (None, "a"),
            (Some("$<PLATFORM_ID:Linux>".to_string()), "l"),
        ]
    );
}

#[test]
fn merge_last_flag_wins_the_same_in_every_mode() {
    // Linux: -funsigned-char -fsigned-char; Windows: -fsigned-char. The
    // order of -m must not matter.
    let d = Dims::new(&[mode("w", "windows", "debug"), mode("l", "linux", "debug")]).unwrap();
    let per = vec![
        (0, vec!["-fsigned-char"]),
        (1, vec!["-funsigned-char", "-fsigned-char"]),
    ];
    let m = merge(&per, &set(&[0, 1]), &d);
    assert_eq!(
        m,
        vec![
            (Some("$<PLATFORM_ID:Linux>".to_string()), "-funsigned-char"),
            (None, "-fsigned-char")
        ]
    );
}

#[test]
fn merge_writes_whole_lists_when_modes_disagree_on_order() {
    let d = Dims::new(&[mode("w", "windows", "debug"), mode("l", "linux", "debug")]).unwrap();
    let per = vec![(0, vec!["a", "b"]), (1, vec!["b", "a"])];
    let m = merge(&per, &set(&[0, 1]), &d);
    let w = Some("$<PLATFORM_ID:Windows>".to_string());
    let l = Some("$<PLATFORM_ID:Linux>".to_string());
    assert_eq!(m, vec![(w.clone(), "a"), (w, "b"), (l.clone(), "b"), (l, "a")]);
}

// --- args in Anubis's order --------------------------------------------------

#[test]
fn deps_public_args_come_before_the_targets_own() {
    let dep = lib(
        Kind::Static,
        &["d.cpp"],
        &[],
        Fields {
            include_dirs: vec!["/r/dep_inc".into()],
            ..flags(&["-funsigned-char"])
        },
    );
    let mut exe = lib(Kind::Exe, &["m.cpp"], &["//p:dep"], Fields::default());
    exe.private = Fields {
        include_dirs: vec!["/r/own_inc".into()],
        ..flags(&["-fsigned-char"])
    };
    let (td, te) = (t("//p:dep"), t("//p:exe"));
    let g: ModeGraph = [(&td, &dep), (&te, &exe)].into_iter().collect();
    let r = resolve(&exe, &g, &mut HashMap::new());
    assert_eq!(
        r.compile.flags.iter().collect::<Vec<_>>(),
        vec!["-funsigned-char", "-fsigned-char"]
    );
    assert_eq!(
        r.compile.include_dirs.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        vec!["/r/dep_inc", "/r/own_inc"]
    );
}

#[test]
fn public_args_are_collected_pre_order_through_shared_libraries() {
    // exe -> a -> c, exe -> b; a is shared. Anubis: a, c, b.
    let a = lib(Kind::Shared, &["a.cpp"], &["//p:c"], flags(&["-DA"]));
    let b = lib(Kind::Static, &["b.cpp"], &[], flags(&["-DB"]));
    let c = lib(Kind::Static, &["c.cpp"], &[], flags(&["-DC"]));
    let exe = lib(Kind::Exe, &["m.cpp"], &["//p:a", "//p:b"], Fields::default());
    let (ta, tb, tc, te) = (t("//p:a"), t("//p:b"), t("//p:c"), t("//p:exe"));
    let g: ModeGraph = [(&ta, &a), (&tb, &b), (&tc, &c), (&te, &exe)].into_iter().collect();
    let r = resolve(&exe, &g, &mut HashMap::new());
    assert_eq!(
        r.compile.flags.iter().collect::<Vec<_>>(),
        vec!["-DA", "-DC", "-DB"]
    );
    // The shared library contains c; the exe links a and b only.
    assert_eq!(
        r.link,
        vec![LinkItem::Target(ta.clone()), LinkItem::Target(tb.clone())]
    );
    let ra = resolve(&a, &g, &mut HashMap::new());
    assert_eq!(ra.linked_into_shared, Some(vec![tc.clone()]));
}

#[test]
fn static_deps_through_a_header_only_library_link_once() {
    // s (shared) -> hdr (no sources) -> u (static); exe -> s.
    let s = lib(Kind::Shared, &["s.cpp"], &["//p:hdr"], Fields::default());
    let hdr = lib(Kind::Static, &[], &["//p:u"], Fields::default());
    let u = lib(Kind::Static, &["u.cpp"], &[], Fields::default());
    let exe = lib(Kind::Exe, &["m.cpp"], &["//p:s"], Fields::default());
    let (ts, th, tu, te) = (t("//p:s"), t("//p:hdr"), t("//p:u"), t("//p:exe"));
    let g: ModeGraph = [(&ts, &s), (&th, &hdr), (&tu, &u), (&te, &exe)].into_iter().collect();
    assert_eq!(
        resolve(&s, &g, &mut HashMap::new()).link,
        vec![LinkItem::Target(tu.clone())]
    );
    assert_eq!(
        resolve(&exe, &g, &mut HashMap::new()).link,
        vec![LinkItem::Target(ts.clone())]
    );
}

#[test]
fn flags_are_deduplicated_like_anubis() {
    // Anubis keeps the first occurrence of each token.
    let dep = lib(Kind::Static, &["d.cpp"], &[], flags(&["-O2", "-Wall"]));
    let mut exe = lib(Kind::Exe, &["m.cpp"], &["//p:dep"], Fields::default());
    exe.private = flags(&["-Wall", "-O1"]);
    let (td, te) = (t("//p:dep"), t("//p:exe"));
    let g: ModeGraph = [(&td, &dep), (&te, &exe)].into_iter().collect();
    let r = resolve(&exe, &g, &mut HashMap::new());
    assert_eq!(
        r.compile.flags.iter().collect::<Vec<_>>(),
        vec!["-O2", "-Wall", "-O1"]
    );
}

// --- graph errors ------------------------------------------------------------

#[test]
fn a_dependency_cycle_is_an_error() {
    let x = lib(Kind::Static, &["x.cpp"], &["//p:y"], Fields::default());
    let y = lib(Kind::Static, &["y.cpp"], &["//p:x"], Fields::default());
    let mut targets: IndexMap<AnubisTarget, Vec<Option<TargetData>>> = IndexMap::new();
    targets.insert(t("//p:x"), vec![Some(x)]);
    targets.insert(t("//p:y"), vec![Some(y)]);
    let e = topo_order(&targets).unwrap_err().to_string();
    assert!(e.contains("dependency cycle"), "{e}");
}

#[test]
fn names_that_collide_after_sanitizing_are_an_error() {
    let a = t("//a-b:x");
    let b = t("//a_b:x");
    assert!(cmake_names([&a, &b].into_iter(), "ns").is_err());
    let names = cmake_names([&t("//p:util"), &t("//q:other")].into_iter(), "ns").unwrap();
    assert_eq!(names[&t("//p:util")].alias, "ns::util");
}

// --- quoting -----------------------------------------------------------------

#[test]
fn escaping_keeps_variable_references() {
    assert_eq!(escape(r#"A="b""#), r#"A=\"b\""#);
    assert_eq!(escape("${_anubis_root}/x$y"), "${_anubis_root}/x\\$y");
    let v = Value::user("x,y>z").unwrap();
    assert_eq!(v.render(Some("$<C>")), "\"$<$<C>:x$<COMMA>y$<ANGLE-R>z>\"");
}

#[test]
fn semicolons_are_refused() {
    assert!(Value::user("MSG=\"a;b\"").is_err());
}

#[test]
fn flag_pairs_are_quoted_so_paths_with_spaces_stay_whole() {
    let paths = PathRenderer::new(Utf8Path::new("/r"), Utf8Path::new("/r/out")).unwrap();
    let v = flag_value(&strs(&["-include", "/r/sp ace/pre.h"]), &paths).unwrap();
    assert_eq!(
        v.text,
        r#"SHELL:\"${_anubis_flag}-include\" \"${_anubis_flag}${_anubis_root}/sp ace/pre.h\""#
    );
}

#[test]
fn std_flags_apply_to_their_language_only() {
    assert_eq!(flag_language(&strs(&["-std=c++20"])), Some("CXX"));
    assert_eq!(flag_language(&strs(&["-std=gnu11"])), Some("C"));
    assert_eq!(flag_language(&strs(&["-fno-exceptions"])), None);
}

#[test]
fn toolchain_paths_are_refused() {
    let paths = PathRenderer::new(Utf8Path::new("/r"), Utf8Path::new("/r/out")).unwrap();
    assert!(paths.path(Utf8Path::new("/r/toolchains/x11/include")).is_err());
    assert!(paths.path(Utf8Path::new("/elsewhere/x")).is_err());
    assert_eq!(
        paths.path(Utf8Path::new("/r/src/a.c")).unwrap(),
        "${_anubis_root}/src/a.c"
    );
}
