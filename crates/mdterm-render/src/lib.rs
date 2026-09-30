//! mdterm-render — terminal ANSI renderer for mdterm (Sprint 2).
//!
//! Renders markdown (GFM tables, fenced code with syntect highlighting,
//! blockquotes, lists, OSC 8 hyperlinks) into width-aware ANSI-styled
//! strings, renders TeX math via external image renderers (typst / latex
//! + dvipng) with terminal graphics escapes (Kitty / iTerm2 / sixel) or a
//! pure-Unicode fallback, and detects terminal capabilities from the
//! environment.

pub mod caps;
pub mod math;
pub mod render;

pub use caps::{GraphicsProto, TerminalCaps};
pub use math::{render_math, unicode_math};
pub use render::render_markdown;

/// How math spans (`$...$` / `$$...$$`) are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MathMode {
    /// Try image renderers when the terminal supports a graphics protocol,
    /// otherwise Unicode approximation.
    #[default]
    Auto,
    /// Always use the Unicode approximation (no external tools).
    Unicode,
    /// Force image rendering; falls back to Unicode when no renderer or no
    /// graphics protocol is available.
    Image,
    /// Leave math spans as literal source text.
    Off,
}

/// Color theme for headings / code / accents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Options for [`render_markdown`].
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Wrap width in terminal columns.
    pub width: usize,
    /// Color theme.
    pub theme: Theme,
    /// How to render `$...$` / `$$...$$` math spans.
    pub math: MathMode,
    /// Terminal capabilities (graphics protocol for math images, truecolor
    /// gate for OSC 8 hyperlinks). Not in the SPEC's field list; needed so
    /// the renderer does not have to re-detect (and tests stay hermetic).
    pub caps: TerminalCaps,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            width: 100,
            theme: Theme::default(),
            math: MathMode::default(),
            caps: TerminalCaps::detect(),
        }
    }
}
