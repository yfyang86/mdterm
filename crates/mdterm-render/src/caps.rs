//! Terminal capability detection (graphics protocol, truecolor).
//!
//! Detection is purely env-based so it works without ioctl access and can
//! be driven hermetically in tests via [`TerminalCaps::detect_with`].
//!
//! Rules (first match wins for graphics):
//!   - `TERM_PROGRAM=iTerm.app`            → iTerm2 inline images
//!   - `TERM_PROGRAM=WezTerm` / `WEZTERM_EXECUTABLE` set → Kitty protocol
//!   - `TERM_PROGRAM=ghostty` / `GHOSTTY_*` set          → Kitty protocol
//!   - `KITTY_WINDOW_ID` set / TERM contains `kitty`     → Kitty protocol
//!   - TERM contains `foot`                              → sixel
//!   - TERM contains `sixel`                             → sixel
//!   - otherwise                                          → no graphics
//! Truecolor: `COLORTERM` is `truecolor` or `24bit`.

/// Terminal graphics image protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphicsProto {
    /// Kitty's APC graphics protocol (`\x1b_G...`).
    Kitty,
    /// iTerm2's OSC 1337 inline images.
    ITerm2,
    /// DEC sixel graphics.
    Sixel,
    /// No graphics protocol.
    #[default]
    None,
}

/// Detected terminal capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TerminalCaps {
    pub graphics: GraphicsProto,
    pub truecolor: bool,
}

impl TerminalCaps {
    /// Detect capabilities from the current process environment.
    pub fn detect() -> Self {
        Self::detect_with(&|k| std::env::var(k).ok())
    }

    /// Detect capabilities using an injected environment lookup. Exposed so
    /// tests can simulate any terminal without touching process env.
    pub fn detect_with(get: &dyn Fn(&str) -> Option<String>) -> Self {
        let term = get("TERM").unwrap_or_default().to_lowercase();
        let term_program = get("TERM_PROGRAM").unwrap_or_default();
        let has = |k: &str| get(k).map(|v| !v.is_empty()).unwrap_or(false);

        let graphics = if term_program == "iTerm.app" {
            GraphicsProto::ITerm2
        } else if has("KITTY_WINDOW_ID") || term.contains("kitty") {
            GraphicsProto::Kitty
        } else if term_program == "WezTerm"
            || has("WEZTERM_EXECUTABLE")
            || has("WEZTERM_PANE")
            || term_program == "ghostty"
            || has("GHOSTTY_RESOURCES_DIR")
            || has("GHOSTTY_BIN_DIR")
        {
            // wezterm and ghostty both implement the kitty graphics protocol.
            GraphicsProto::Kitty
        } else if term.contains("foot") {
            // foot supports sixel.
            GraphicsProto::Sixel
        } else if term.contains("sixel") {
            GraphicsProto::Sixel
        } else {
            GraphicsProto::None
        };

        let colorterm = get("COLORTERM").unwrap_or_default().to_lowercase();
        let truecolor = colorterm == "truecolor" || colorterm == "24bit";

        TerminalCaps { graphics, truecolor }
    }

    /// Human-readable name of the detected terminal program (best effort).
    pub fn terminal_program() -> String {
        std::env::var("TERM_PROGRAM")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("TERM").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "unknown".to_string())
    }
}
