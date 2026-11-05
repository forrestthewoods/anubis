//! Error macros that prefix the message with file, function and line:
//! `bail_loc!`, `bail_loc_if!` and `anyhow_loc!`.

#[macro_export]
macro_rules! function_name {
    () => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        type_name_of(f)
            .rsplit("::")
            .find(|&part| part != "f" && part != "{{closure}}")
            .expect("Short function name")
    }};
}

#[macro_export]
macro_rules! bail_loc {
    ($msg:expr) => {
        anyhow::bail!("[{}:{} - {}] {}", file!(), $crate::function_name!(), line!(), $msg)
    };
    ($fmt:expr, $($arg:tt)*) => {
        anyhow::bail!("[{}:{} - {}] {}", file!(), $crate::function_name!(), line!(), format!($fmt, $($arg)*))
    };
}

#[macro_export]
macro_rules! bail_loc_if {
    ($cond:expr, $msg:expr) => {
        if $cond {
            anyhow::bail!("[{}:{} - {}] {}", file!(), $crate::function_name!(), line!(), $msg)
        }
    };
    ($cond:expr, $fmt:expr, $($arg:tt)*) => {
        if $cond {
            anyhow::bail!("[{}:{} - {}] {}", file!(), $crate::function_name!(), line!(), format!($fmt, $($arg)*))
        }
    };
}

#[macro_export]
macro_rules! anyhow_loc {
    ($msg:expr) => {
        anyhow::anyhow!("[{}:{} - {}] {}", file!(), $crate::function_name!(), line!(), $msg)
    };
    ($fmt:expr, $($arg:tt)*) => {
        anyhow::anyhow!("[{}:{} - {}] {}", file!(), $crate::function_name!(), line!(), format!($fmt, $($arg)*))
    };
}
