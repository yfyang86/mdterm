//! Golden-style ANSI tests for render_markdown (insta-free assertions).

use mdterm_render::{
    render_markdown, GraphicsProto, MathMode, RenderOptions, TerminalCaps, Theme,
};

fn opts(width: usize) -> RenderOptions {
    RenderOptions {
        width,
        theme: Theme::Dark,
        math: MathMode::Unicode,
        caps: TerminalCaps {
            graphics: GraphicsProto::None,
            truecolor: false,
        },
    }
}

/// Strip ANSI escapes for plain-text assertions.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut prev_esc = false;
                for c in chars.by_ref() {
                    if c == '\x07' || (prev_esc && c == '\\') {
                        break;
                    }
                    prev_esc = c == '\x1b';
                }
            }
            _ => {}
        }
    }
    out
}

#[test]
fn heading_is_bold_and_colored() {
    let out = render_markdown("# Hello\n", &opts(80)).unwrap();
    assert_eq!(out, "\x1b[36m\x1b[1mHello\x1b[0m\n");
}

#[test]
fn bold_and_italic() {
    let out = render_markdown("a **b** *c*\n", &opts(80)).unwrap();
    assert!(out.contains("\x1b[1mb\x1b[0m"), "bold: {out:?}");
    assert!(out.contains("\x1b[3mc\x1b[0m"), "italic: {out:?}");
    assert!(out.starts_with("a "), "plain prefix: {out:?}");
}

#[test]
fn inline_code_is_highlighted() {
    let out = render_markdown("use `foo()` now\n", &opts(80)).unwrap();
    assert!(out.contains("\x1b[33mfoo()\x1b[0m"), "inline code: {out:?}");
}

#[test]
fn gfm_table_is_aligned() {
    let md = "| Name | Value |\n|:-----|------:|\n| foo  | 1     |\n| bar  | 200   |\n";
    let out = render_markdown(md, &opts(80)).unwrap();
    let plain = strip_ansi(&out);
    let lines: Vec<&str> = plain.lines().collect();
    assert_eq!(lines.len(), 6, "table line count: {plain:?}");
    assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'), "{plain:?}");
    assert!(lines[2].starts_with('├'), "header separator: {plain:?}");
    assert!(lines[5].starts_with('└') && lines[5].ends_with('┘'), "{plain:?}");
    // All rows must have identical visible width (column alignment).
    let widths: Vec<usize> = lines.iter().map(|l| l.chars().count()).collect();
    assert!(
        widths.iter().all(|w| *w == widths[0]),
        "misaligned rows: {widths:?} in {plain:?}"
    );
    // Right-aligned "Value" column: 1 and 200 right-aligned.
    assert!(lines[1].contains("Value"), "{plain:?}");
    assert!(lines[3].contains("    1"), "right align: {plain:?}");
    assert!(lines[4].contains("  200"), "right align: {plain:?}");
}

#[test]
fn fenced_code_has_ansi_colors() {
    let md = "```rust\nfn main() { let x = 1; }\n```\n";
    let out = render_markdown(md, &opts(80)).unwrap();
    // syntect truecolor foreground escapes (or, if assets are unavailable,
    // the dim fallback) — either way the code text is present.
    assert!(
        out.contains("\x1b[38;2;") || out.contains("\x1b[2m"),
        "expected color codes: {out:?}"
    );
    assert!(out.contains("main"), "{out:?}");
}

#[test]
fn blockquote_has_prefix() {
    let out = render_markdown("> quoted text\n", &opts(80)).unwrap();
    let plain = strip_ansi(&out);
    assert!(plain.contains("│ quoted text"), "blockquote: {plain:?}");
}

#[test]
fn link_text_and_url() {
    // No truecolor: text + URL in parens.
    let out = render_markdown("[site](https://example.com)\n", &opts(80)).unwrap();
    let plain = strip_ansi(&out);
    assert!(plain.contains("site"), "{plain:?}");
    assert!(plain.contains("https://example.com"), "{plain:?}");
    assert!(!out.contains("\x1b]8;;"), "no OSC 8 without truecolor: {out:?}");

    // Truecolor: OSC 8 hyperlink.
    let mut o = opts(80);
    o.caps.truecolor = true;
    let out = render_markdown("[site](https://example.com)\n", &o).unwrap();
    assert!(
        out.contains("\x1b]8;;https://example.com\x1b\\"),
        "OSC 8: {out:?}"
    );
    assert!(out.contains("\x1b]8;;\x1b\\"), "OSC 8 terminator: {out:?}");
}

#[test]
fn unordered_and_ordered_lists() {
    let out = render_markdown("- one\n- two\n\n1. a\n2. b\n", &opts(80)).unwrap();
    let plain = strip_ansi(&out);
    assert!(plain.contains("• one"), "{plain:?}");
    assert!(plain.contains("1. a"), "{plain:?}");
    assert!(plain.contains("2. b"), "{plain:?}");
}

#[test]
fn wraps_at_width_40_without_splitting_escapes() {
    let md = "This is a fairly long paragraph of text that must be wrapped at \
              forty columns without breaking any ANSI escape sequences in \
              the **bold middle** of the text body.";
    let out = render_markdown(md, &opts(40)).unwrap();
    // Every line fits in 40 visible columns.
    for line in out.lines() {
        let w = strip_ansi(line).chars().count();
        assert!(w <= 40, "line too wide ({w}): {line:?}");
    }
    // No escape sequence is split across lines: every '\x1b[' on a line is
    // terminated on the same line.
    for line in out.lines() {
        let mut rest = line;
        while let Some(pos) = rest.find('\x1b') {
            let tail = &rest[pos..];
            if let Some(kind) = tail.chars().nth(1) {
                match kind {
                    '[' => assert!(
                        tail.chars().any(|c| ('\x40'..='\x7e').contains(&c)),
                        "unterminated CSI in {line:?}"
                    ),
                    ']' => assert!(
                        tail.contains('\x07') || tail.contains("\x1b\\"),
                        "unterminated OSC in {line:?}"
                    ),
                    _ => {}
                }
            }
            rest = &rest[pos + 1..];
        }
    }
    // Bold styling survived wrapping.
    assert!(out.contains("\x1b[1m"), "bold present: {out:?}");
    assert!(strip_ansi(&out).contains("bold middle"), "{out:?}");
}

#[test]
fn math_spans_are_rendered_and_code_is_untouched() {
    let md = "Inline $\\alpha$ math.\n\n```\n$not_math$\n```\n\n$$\n\\beta\n$$\n";
    let out = render_markdown(md, &opts(80)).unwrap();
    let plain = strip_ansi(&out);
    assert!(plain.contains('α'), "inline math: {plain:?}");
    assert!(plain.contains('β'), "display math: {plain:?}");
    assert!(
        plain.contains("$not_math$"),
        "code block must stay literal: {plain:?}"
    );
}

/// F5 regression: `$...$` inside an inline link destination is not math.
#[test]
fn math_not_extracted_from_link_urls() {
    let md = "[click](https://example.com/$v$/page) and real math $\\alpha$\n";
    let out = render_markdown(md, &opts(120)).unwrap();
    let plain = strip_ansi(&out);
    assert!(
        plain.contains("https://example.com/$v$/page"),
        "link URL must stay literal: {plain:?}"
    );
    assert!(plain.contains('α'), "real math still renders: {plain:?}");
}

/// F5 regression: `$...$` inside a link reference definition is not math.
#[test]
fn math_not_extracted_from_link_reference_definitions() {
    let md = "[docs]: https://example.com/$u$/ref\n\nsee [docs] and $\\beta$\n";
    let out = render_markdown(md, &opts(120)).unwrap();
    let plain = strip_ansi(&out);
    // The reference destination must not be math-extracted (which would
    // rewrite the URL the link points at).
    assert!(
        plain.contains("https://example.com/$u$/ref"),
        "reference URL must stay literal: {plain:?}"
    );
    assert!(plain.contains('β'), "real math still renders: {plain:?}");
    assert!(!out.contains('\u{E000}'), "no placeholder leakage: {out:?}");
}

/// F5 regression: `$...$` inside a 4-space indented code block is not math.
#[test]
fn math_not_extracted_from_indented_code_blocks() {
    let md = "paragraph\n\n    code $not_math$ here\n\nagain $\\gamma$\n";
    let out = render_markdown(md, &opts(120)).unwrap();
    let plain = strip_ansi(&out);
    assert!(
        plain.contains("$not_math$"),
        "indented code must stay literal: {plain:?}"
    );
    assert!(plain.contains('γ'), "real math still renders: {plain:?}");
}

#[test]
fn math_mode_off_leaves_source() {
    let mut o = opts(80);
    o.math = MathMode::Off;
    let out = render_markdown("value $\\alpha$ end\n", &o).unwrap();
    let plain = strip_ansi(&out);
    assert!(plain.contains("$\\alpha$"), "math off: {plain:?}");
}

#[test]
fn strikethrough_works() {
    let out = render_markdown("~~gone~~\n", &opts(80)).unwrap();
    assert!(out.contains("\x1b[9mgone\x1b[0m"), "strike: {out:?}");
}
