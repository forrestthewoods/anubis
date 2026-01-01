//! Shared test utilities and macros.

/// Asserts that a Result is Ok, printing the error if not.
#[macro_export]
macro_rules! assert_ok {
    ($result:expr) => {
        assert!($result.is_ok(), "Expected Ok, got Err: {:#?}", $result);
    };
}

/// Asserts that a Result is Err, printing the value if not.
#[macro_export]
macro_rules! assert_err {
    ($result:expr) => {
        assert!($result.is_err(), "Expected Err, got Ok: {:#?}", $result);
    };
}
