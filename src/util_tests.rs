//! Tests for util.rs

use crate::util::{format_duration, lexical_path};
use std::time::Duration;

#[test]
fn format_duration_milliseconds() {
    // Test values under 1 second show as milliseconds
    assert_eq!(format_duration(Duration::from_millis(0)), "0ms");
    assert_eq!(format_duration(Duration::from_millis(1)), "1ms");
    assert_eq!(format_duration(Duration::from_millis(50)), "50ms");
    assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
    assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
}

#[test]
fn format_duration_seconds() {
    // Test values between 1 second and 1 minute show as seconds with 1 decimal
    assert_eq!(format_duration(Duration::from_millis(1000)), "1.0s");
    assert_eq!(format_duration(Duration::from_millis(1500)), "1.5s");
    assert_eq!(format_duration(Duration::from_millis(2300)), "2.3s");
    assert_eq!(format_duration(Duration::from_millis(10000)), "10.0s");
    assert_eq!(format_duration(Duration::from_millis(45678)), "45.7s");
    assert_eq!(format_duration(Duration::from_millis(59999)), "60.0s");
}

#[test]
fn format_duration_minutes() {
    // Test values at or above 1 minute show as minutes and seconds
    assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
    assert_eq!(format_duration(Duration::from_secs(61)), "1m 1s");
    assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
    assert_eq!(format_duration(Duration::from_secs(120)), "2m 0s");
    assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
    assert_eq!(format_duration(Duration::from_secs(3661)), "61m 1s");
}

#[test]
fn path_macros_report_path_and_call_site() {
    use camino::Utf8Path;
    assert_eq!(crate::parent_dir!(Utf8Path::new("a/b.c")).unwrap(), "a");
    assert_eq!(crate::file_name!(Utf8Path::new("a/b.c")).unwrap(), "b.c");
    let err = crate::parent_dir!(Utf8Path::new("/")).unwrap_err().to_string();
    assert!(err.contains("util_tests.rs") && err.contains("No parent dir for [\"/\"]"), "{err}");
    let err = crate::file_name!(Utf8Path::new("a/..")).unwrap_err().to_string();
    assert!(err.contains("No file name for"), "{err}");
}

#[test]
fn lexical_path_folds_dots_and_keeps_roots() {
    let l = |p: &str| {
        let s = lexical_path(p);
        if cfg!(windows) { assert_eq!(s, s.to_lowercase(), "Windows lowercases"); }
        s
    };
    let lc = |s: &str| if cfg!(windows) { s.to_lowercase() } else { s.to_string() };
    assert_eq!(l("/ws/src/../inc/./a.h"), lc("/ws/inc/a.h"));
    assert_eq!(l("/ws//inc/a.h"), lc("/ws/inc/a.h"));
    assert_eq!(l("/../x.h"), lc("/x.h"), "'..' at the root stays at the root");
    assert_eq!(l("C:\\Ws\\src\\..\\A.h"), lc("C:/Ws/A.h"));
    assert_eq!(l("C:/.."), lc("C:"), "'..' doesn't pop the drive");
    assert_eq!(l("//server/share/dir/../a.h"), lc("//server/share/a.h"));
    assert_eq!(l("//server/share/.."), lc("//server/share"), "'..' doesn't pop the share");
    assert_eq!(l("a/../../b"), lc("../b"), "relative paths keep leading '..'");
}
