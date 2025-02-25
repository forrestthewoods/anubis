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
        anyhow::bail!("[{}:{} - {}] {}", file!(), function_name!(), line!(), $msg)
    };
    ($fmt:expr, $($arg:tt)*) => {
        anyhow::bail!("[{}:{} - {}] {}", file!(), function_name!(), line!(), format!($fmt, $($arg)*))
    };
}

#[macro_export]
macro_rules! anyhow_loc {
    ($msg:expr) => {
        anyhow::anyhow!("[{}:{} - {}] {}", file!(), function_name!(), line!(), $msg)
    };
    ($fmt:expr, $($arg:tt)*) => {
        anyhow::anyhow!("[{}:{} - {}] {}", file!(), function_name!(), line!(), format!($fmt, $($arg)*))
    };
}
