//! Env-injection tests for terminal capability detection.

use mdterm_render::{GraphicsProto, TerminalCaps};

fn detect(vars: &[(&str, &str)]) -> TerminalCaps {
    TerminalCaps::detect_with(&|k| {
        vars.iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.to_string())
    })
}

#[test]
fn iterm2_via_term_program() {
    let caps = detect(&[
        ("TERM_PROGRAM", "iTerm.app"),
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
    ]);
    assert_eq!(caps.graphics, GraphicsProto::ITerm2);
    assert!(caps.truecolor);
}

#[test]
fn kitty_via_window_id() {
    let caps = detect(&[("KITTY_WINDOW_ID", "1"), ("TERM", "xterm-kitty")]);
    assert_eq!(caps.graphics, GraphicsProto::Kitty);
    assert!(!caps.truecolor); // no COLORTERM
}

#[test]
fn wezterm_maps_to_kitty_protocol() {
    let caps = detect(&[("TERM_PROGRAM", "WezTerm"), ("TERM", "xterm-256color")]);
    assert_eq!(caps.graphics, GraphicsProto::Kitty);

    let caps = detect(&[("WEZTERM_EXECUTABLE", "/usr/bin/wezterm")]);
    assert_eq!(caps.graphics, GraphicsProto::Kitty);
}

#[test]
fn ghostty_maps_to_kitty_protocol() {
    let caps = detect(&[("TERM_PROGRAM", "ghostty"), ("TERM", "xterm-ghostty")]);
    assert_eq!(caps.graphics, GraphicsProto::Kitty);
}

#[test]
fn sixel_via_term_and_foot() {
    let caps = detect(&[("TERM", "foot")]);
    assert_eq!(caps.graphics, GraphicsProto::Sixel);

    let caps = detect(&[("TERM", "xterm-sixel")]);
    assert_eq!(caps.graphics, GraphicsProto::Sixel);
}

#[test]
fn plain_xterm_has_no_graphics() {
    let caps = detect(&[("TERM", "xterm"), ("COLORTERM", "24bit")]);
    assert_eq!(caps.graphics, GraphicsProto::None);
    assert!(caps.truecolor);
}

#[test]
fn empty_env_is_none_no_truecolor() {
    let caps = detect(&[]);
    assert_eq!(caps.graphics, GraphicsProto::None);
    assert!(!caps.truecolor);
}
