// ----------------------------------------------------------------------------
// Declarations
// ----------------------------------------------------------------------------
pub trait SlashFix {
    fn slash_fix(self) -> Self;
}

// ----------------------------------------------------------------------------
// Implementations
// ----------------------------------------------------------------------------
impl SlashFix for std::path::PathBuf {
    fn slash_fix(self) -> Self {
        self.to_string_lossy().to_string().slash_fix().into()
    }
}

impl SlashFix for String {
    fn slash_fix(self) -> Self {
        self.replace("\\", "/")
    }
}
