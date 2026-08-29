/// XKB RMLVO names used to compile a keymap.
///
/// Empty / `None` fields select libxkbcommon defaults (typically `evdev` / `pc105` / `us`).
/// Multi-layout setups use comma-separated `layout` / `variant` values with `options`
/// such as `grp:alt_shift_toggle`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XkbConfig {
    /// XKB rules file name (usually unset).
    pub rules: Option<String>,
    /// Keyboard model (e.g. `pc105`).
    pub model: Option<String>,
    /// Layout(s), e.g. `us` or `us,de`.
    pub layout: Option<String>,
    /// Variant(s) matching `layout`, e.g. `nodeadkeys` or `,nodeadkeys`.
    pub variant: Option<String>,
    /// Options, e.g. `grp:alt_shift_toggle,caps:escape`.
    pub options: Option<String>,
}
