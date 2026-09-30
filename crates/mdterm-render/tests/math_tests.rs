//! Math rendering tests: Unicode fallback without external tools, and the
//! image-escape chain via a fake `typst` on PATH.

use std::sync::Mutex;

use mdterm_render::{render_math, unicode_math, GraphicsProto, TerminalCaps};

/// PATH is process-global; tests that mutate it must not run concurrently.
static PATH_LOCK: Mutex<()> = Mutex::new(());

fn caps(graphics: GraphicsProto) -> TerminalCaps {
    TerminalCaps {
        graphics,
        truecolor: true,
    }
}

/// Guard that restores PATH on drop.
struct PathGuard {
    saved: Option<std::ffi::OsString>,
}

impl PathGuard {
    fn set(path: &std::path::Path) -> Self {
        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", path);
        PathGuard { saved }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match &self.saved {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}

/// A fake `typst` that "compiles" by writing a fixed 8-byte PNG signature
/// to its last argument (the output path).
fn fake_typst_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let script = "#!/bin/sh\nfor last; do :; done\nprintf '\\211PNG\\r\\n\\032\\n' > \"$last\"\n";
    let path = dir.path().join("typst");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// base64 of the 8-byte PNG signature the fake typst emits.
const PNG_B64: &str = "iVBORw0KGgo=";

#[test]
fn unicode_math_sample() {
    let out = unicode_math("\\alpha + \\frac{1}{2} = \\sum_{i=1}^{n} x_i");
    assert_eq!(out, "α + 1/2 = ∑ᵢ₌₁ⁿ xᵢ");
}

#[test]
fn unicode_math_common_constructs() {
    assert_eq!(unicode_math("\\sqrt{x^2}"), "√(x²)");
    // π has no Unicode superscript, so the whole script falls back to ^(...).
    assert_eq!(unicode_math("e^{i\\pi} + 1 = 0"), "e^(iπ) + 1 = 0");
    assert_eq!(unicode_math("\\int_0^\\infty x dx"), "∫₀^∞ x dx");
    assert_eq!(unicode_math("a \\le b \\times c \\rightarrow d"), "a ≤ b × c → d");
    assert_eq!(unicode_math("\\frac{x+1}{y}"), "(x+1)/y");
}

#[test]
fn unicode_fallback_without_any_external_tool() {
    let _lock = PATH_LOCK.lock().unwrap();
    // Empty PATH: no typst/latex/dvipng can be found; even with a graphics
    // protocol available, render_math must degrade to Unicode, never fail.
    let dir = tempfile::tempdir().unwrap();
    let _path = PathGuard::set(dir.path());
    let out = render_math(
        "\\alpha + \\frac{1}{2} = \\sum_{i=1}^{n} x_i",
        &caps(GraphicsProto::Kitty),
    )
    .unwrap();
    assert!(!out.is_empty());
    assert_eq!(out, "α + 1/2 = ∑ᵢ₌₁ⁿ xᵢ");
    assert!(!out.contains("\x1b_G"), "no image escape without renderer");
}

#[test]
fn kitty_escape_via_fake_typst() {
    let _lock = PATH_LOCK.lock().unwrap();
    let dir = fake_typst_dir();
    let _path = PathGuard::set(dir.path());
    let out = render_math("\\alpha", &caps(GraphicsProto::Kitty)).unwrap();
    // First (and here only) chunk: quiet + transmit-and-display + PNG.
    assert!(out.contains("\x1b_Gq=2,a=T,f=100"), "kitty escape: {out:?}");
    assert!(out.contains(PNG_B64), "base64 payload: {out:?}");
}

#[test]
fn iterm2_escape_via_fake_typst() {
    let _lock = PATH_LOCK.lock().unwrap();
    let dir = fake_typst_dir();
    let _path = PathGuard::set(dir.path());
    let out = render_math("\\alpha", &caps(GraphicsProto::ITerm2)).unwrap();
    assert!(
        out.contains(&format!("\x1b]1337;File=inline=1:{PNG_B64}\x07")),
        "iterm2 escape: {out:?}"
    );
}

#[test]
fn graphics_none_degrades_to_unicode_even_with_renderer() {
    let _lock = PATH_LOCK.lock().unwrap();
    let dir = fake_typst_dir();
    let _path = PathGuard::set(dir.path());
    let out = render_math("\\beta", &caps(GraphicsProto::None)).unwrap();
    assert_eq!(out, "β");
}

#[test]
fn sixel_without_converter_degrades_to_unicode() {
    let _lock = PATH_LOCK.lock().unwrap();
    // typst exists but img2sixel does not → sixel path skipped → Unicode.
    let dir = fake_typst_dir();
    let _path = PathGuard::set(dir.path());
    let out = render_math("\\gamma", &caps(GraphicsProto::Sixel)).unwrap();
    assert_eq!(out, "γ");
}
