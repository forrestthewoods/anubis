//! Tests for util.rs

use crate::util::format_duration;
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
