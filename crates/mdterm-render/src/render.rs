//! Markdown → ANSI rendering.
//!
//! Pipeline: extract `$...$` / `$$...$$` math spans into placeholders
//! (unless [`MathMode::Off`]) → parse with comrak (GFM tables enabled) →
//! walk the AST emitting width-aware ANSI (syntect for fenced code,
//! hand-rolled Unicode box tables, OSC 8 hyperlinks when truecolor) →
//! splice rendered math back in place of the placeholders.

use anyhow::Result;
use comrak::nodes::{AstNode, NodeValue, TableAlignment};
use comrak::{parse_document, Arena, Options};
use once_cell::sync::Lazy;
use unicode_width::UnicodeWidthChar;

use crate::caps::TerminalCaps;
use crate::math::{render_math, unicode_math};
use crate::{MathMode, RenderOptions, Theme};

// ANSI styles.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
const FG_CYAN: &str = "\x1b[36m";
const FG_GREEN: &str = "\x1b[32m";
const FG_YELLOW: &str = "\x1b[33m";
const FG_BLUE: &str = "\x1b[34m";
const FG_MAGENTA: &str = "\x1b[35m";
const FG_GRAY: &str = "\x1b[90m";

/// Render markdown into an ANSI-styled, width-wrapped string.
pub fn render_markdown(md: &str, opts: &RenderOptions) -> Result<String> {
    let (md, math_spans) = if opts.math == MathMode::Off {
        (md.to_string(), Vec::new())
    } else {
        extract_math(md)
    };

    let arena = Arena::new();
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    let root = parse_document(&arena, &md, &options);

    let mut r = Renderer {
        opts,
        out: String::new(),
    };
    for child in root.children() {
        r.block(child);
    }
    let mut out = r.out;
    // Collapse 3+ consecutive newlines (display-math splicing can create
    // them) and trim trailing blank lines to a single final newline.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    while out.ends_with("\n\n") {
        out.pop();
    }

    // Splice rendered math back in.
    for (idx, span) in math_spans.iter().enumerate() {
        let placeholder = placeholder(idx);
        // The placeholder sits in its own paragraph, so no extra newlines
        // are needed around the replacement (image escapes end with one).
        let rendered = render_span(span, opts);
        out = out.replace(&placeholder, &rendered);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// block rendering
// ---------------------------------------------------------------------------

struct Renderer<'a> {
    opts: &'a RenderOptions,
    out: String,
}

impl<'a> Renderer<'a> {
    fn block(&mut self, node: &'a AstNode<'a>) {
        match &node.data.borrow().value {
            NodeValue::Paragraph => {
                let text = self.inline_children(node);
                let wrapped = wrap_ansi(&text, self.opts.width);
                self.push_lines(&wrapped);
                self.blank();
            }
            NodeValue::Heading(h) => {
                let text = plain_children(node);
                let style = heading_style(h.level, self.opts.theme);
                self.push_line(&format!("{style}{BOLD}{text}{RESET}"));
                self.blank();
            }
            NodeValue::CodeBlock(cb) => {
                let lang = cb.info.split_whitespace().next().unwrap_or("");
                for line in highlight_code(&cb.literal, lang, self.opts.theme).lines() {
                    self.push_line(&format!("  {line}"));
                }
                self.blank();
            }
            NodeValue::BlockQuote => {
                let mut inner = Renderer {
                    opts: self.opts,
                    out: String::new(),
                };
                for child in node.children() {
                    inner.block(child);
                }
                for line in inner.out.trim_end_matches('\n').lines() {
                    if line.is_empty() {
                        self.push_line(&format!("{FG_GRAY}│{RESET}"));
                    } else {
                        self.push_line(&format!("{FG_GRAY}│{RESET} {line}"));
                    }
                }
                self.blank();
            }
            NodeValue::List(l) => {
                let ordered = l.list_type == comrak::nodes::ListType::Ordered;
                let mut n = l.start;
                for item in node.children() {
                    let marker = if ordered {
                        let m = format!("{n}. ");
                        n += 1;
                        m
                    } else {
                        "• ".to_string()
                    };
                    self.list_item(item, &marker);
                }
                self.blank();
            }
            NodeValue::Table(_) => {
                self.table(node);
                self.blank();
            }
            NodeValue::ThematicBreak => {
                let w = self.opts.width.min(72).max(8);
                self.push_line(&format!("{FG_GRAY}{}{RESET}", "─".repeat(w)));
                self.blank();
            }
            NodeValue::HtmlBlock(_) => {
                // Raw HTML is meaningless on a terminal; drop it.
            }
            _ => {
                // Footnotes, descriptions, etc.: render children generically.
                for child in node.children() {
                    self.block(child);
                }
            }
        }
    }

    fn list_item(&mut self, item: &'a AstNode<'a>, marker: &str) {
        let mut inner = Renderer {
            opts: self.opts,
            out: String::new(),
        };
        for child in item.children() {
            // Task list marker.
            if let NodeValue::TaskItem(Some(checked)) = &child.data.borrow().value {
                let done = *checked == 'x' || *checked == 'X';
                inner.out.push_str(if done { "[x] " } else { "[ ] " });
                continue;
            }
            inner.block(child);
        }
        let indent = " ".repeat(marker.chars().count());
        let width = self.opts.width.saturating_sub(marker.len()).max(16);
        // Re-wrap the already-wrapped inner text to account for the indent.
        let body: String = inner
            .out
            .trim_end_matches('\n')
            .lines()
            .flat_map(|l| wrap_ansi(l, width))
            .collect::<Vec<_>>()
            .join("\n");
        for (i, line) in body.lines().enumerate() {
            if i == 0 {
                self.push_line(&format!("{FG_GREEN}{marker}{RESET}{line}"));
            } else if line.is_empty() {
                self.out.push('\n');
            } else {
                self.push_line(&format!("{indent}{line}"));
            }
        }
    }

    fn table(&mut self, table: &'a AstNode<'a>) {
        let alignments: Vec<TableAlignment> = match &table.data.borrow().value {
            NodeValue::Table(t) => t.alignments.clone(),
            _ => return,
        };
        let mut rows: Vec<(bool, Vec<(String, String)>)> = Vec::new(); // (header, cells(styled, plain))
        for row in table.children() {
            let is_header = matches!(row.data.borrow().value, NodeValue::TableRow(true));
            let mut cells = Vec::new();
            for cell in row.children() {
                let styled = self.inline_children(cell);
                let plain = plain_children(cell);
                cells.push((styled, plain));
            }
            rows.push((is_header, cells));
        }
        if rows.is_empty() {
            return;
        }
        let ncols = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        // Column widths from plain text, shrunk to fit if necessary.
        let mut widths: Vec<usize> = vec![1; ncols];
        for (_, cells) in &rows {
            for (i, (_, plain)) in cells.iter().enumerate() {
                widths[i] = widths[i].max(display_width(plain));
            }
        }
        let overhead = 3 * ncols + 1; // "│ " + " │" per col + outer border
        let budget = self.opts.width.saturating_sub(overhead).max(ncols * 3);
        while widths.iter().sum::<usize>() > budget {
            let Some((i, _)) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| *w)
                .filter(|(_, w)| **w > 3)
            else {
                break;
            };
            widths[i] -= 1;
        }

        let border = |l: &str, m: &str, r: &str| -> String {
            let mut s = format!("{FG_GRAY}{l}");
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push_str(if i + 1 < ncols { m } else { r });
            }
            s.push_str(RESET);
            s
        };

        self.push_line(&border("┌", "┬", "┐"));
        for (ri, (is_header, cells)) in rows.iter().enumerate() {
            let mut line = format!("{FG_GRAY}│{RESET}");
            for ci in 0..ncols {
                let (styled, plain) = cells
                    .get(ci)
                    .map(|(s, p)| (s.as_str(), p.as_str()))
                    .unwrap_or(("", ""));
                let align = alignments.get(ci).copied().unwrap_or(TableAlignment::None);
                let pad = widths[ci].saturating_sub(display_width(plain));
                let (lpad, rpad) = match align {
                    TableAlignment::Right => (pad, 0),
                    TableAlignment::Center => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                let mut cell_text = styled.to_string();
                if display_width(plain) > widths[ci] {
                    // Plain width exceeds the (shrunk) column: truncate.
                    cell_text = truncate_plain(plain, widths[ci]);
                }
                line.push(' ');
                line.push_str(&" ".repeat(lpad));
                line.push_str(&cell_text);
                line.push_str(RESET);
                line.push_str(&" ".repeat(rpad));
                line.push_str(&format!(" {FG_GRAY}│{RESET}"));
            }
            self.push_line(&line);
            if *is_header {
                self.push_line(&border("├", "┼", "┤"));
            }
            let _ = ri;
        }
        self.push_line(&border("└", "┴", "┘"));
    }

    // -- inline ------------------------------------------------------------

    /// Render inline children to a styled (unwrapped) string.
    fn inline_children(&self, node: &'a AstNode<'a>) -> String {
        let mut s = String::new();
        for child in node.children() {
            self.inline(child, &mut s);
        }
        s
    }

    fn inline(&self, node: &'a AstNode<'a>, out: &mut String) {
        match &node.data.borrow().value {
            NodeValue::Text(t) => out.push_str(t),
            NodeValue::Code(c) => {
                out.push_str(FG_YELLOW);
                out.push_str(&c.literal);
                out.push_str(RESET);
            }
            NodeValue::Emph => {
                out.push_str(ITALIC);
                out.push_str(&self.inline_children(node));
                out.push_str(RESET);
            }
            NodeValue::Strong => {
                out.push_str(BOLD);
                out.push_str(&self.inline_children(node));
                out.push_str(RESET);
            }
            NodeValue::Strikethrough => {
                out.push_str(STRIKE);
                out.push_str(&self.inline_children(node));
                out.push_str(RESET);
            }
            NodeValue::Link(l) => {
                let text = self.inline_children(node);
                if self.opts.caps.truecolor {
                    out.push_str(&format!(
                        "\x1b]8;;{}\x1b\\{UNDERLINE}{FG_CYAN}{text}{RESET}\x1b]8;;\x1b\\",
                        l.url
                    ));
                } else {
                    out.push_str(&format!(
                        "{UNDERLINE}{FG_CYAN}{text}{RESET}{FG_GRAY} ({}){RESET}",
                        l.url
                    ));
                }
            }
            NodeValue::Image(l) => {
                let alt = plain_children(node);
                out.push_str(&format!("{FG_MAGENTA}[image: {alt}]{RESET}"));
                if !l.url.is_empty() {
                    out.push_str(&format!("{FG_GRAY} ({}){RESET}", l.url));
                }
            }
            NodeValue::SoftBreak | NodeValue::LineBreak => out.push('\n'),
            NodeValue::HtmlInline(_) => {}
            _ => out.push_str(&self.inline_children(node)),
        }
    }

    // -- output helpers ------------------------------------------------------

    fn push_line(&mut self, line: &str) {
        self.out.push_str(line);
        self.out.push('\n');
    }

    fn push_lines(&mut self, lines: &[String]) {
        for l in lines {
            self.push_line(l);
        }
    }

    fn blank(&mut self) {
        if !self.out.ends_with("\n\n") && !self.out.is_empty() {
            self.out.push('\n');
        }
    }
}

fn heading_style(level: u8, theme: Theme) -> &'static str {
    let _ = theme; // single accent palette works on both dark and light
    match level {
        1 => FG_CYAN,
        2 => FG_GREEN,
        3 => FG_YELLOW,
        4 => FG_MAGENTA,
        _ => FG_BLUE,
    }
}

// ---------------------------------------------------------------------------
// plain-text extraction
// ---------------------------------------------------------------------------

/// Concatenate the plain text of inline children (no styles).
fn plain_children<'a>(node: &'a AstNode<'a>) -> String {
    let mut s = String::new();
    for child in node.children() {
        collect_plain(child, &mut s);
    }
    s
}

fn collect_plain<'a>(node: &'a AstNode<'a>, out: &mut String) {
    match &node.data.borrow().value {
        NodeValue::Text(t) => out.push_str(t),
        NodeValue::Code(c) => out.push_str(&c.literal),
        NodeValue::SoftBreak | NodeValue::LineBreak => out.push(' '),
        _ => {
            for child in node.children() {
                collect_plain(child, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// width-aware ANSI wrapping
// ---------------------------------------------------------------------------

/// Visible display width of a string that may contain ANSI escapes.
pub(crate) fn display_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            skip_escape(&mut chars);
        } else {
            width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    width
}

/// Consume one escape sequence after the ESC char: CSI (`[...final`),
/// OSC (`]...BEL` or `]...ESC\`), or a two-byte sequence.
fn skip_escape(chars: &mut std::iter::Peekable<std::str::Chars>) {
    match chars.next() {
        Some('[') => {
            // CSI: parameter/intermediate bytes until final byte 0x40..=0x7E.
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            // OSC: until BEL or ESC \.
            let mut prev_esc = false;
            for c in chars.by_ref() {
                if c == '\x07' {
                    break;
                }
                if prev_esc && c == '\\' {
                    break;
                }
                prev_esc = c == '\x1b';
            }
        }
        Some(_) => {} // two-byte sequence: already consumed
        None => {}
    }
}

/// A wrapping token: visible text plus the escape sequences that precede
/// it. Escapes always attach to the *following* visible token, so e.g. a
/// RESET after a bold word is emitted right after that word and never
/// swallows the style of the next one.
struct Token {
    /// Escape sequences emitted before the visible text.
    prefix: String,
    text: String,
    width: usize,
    is_space: bool,
}

/// Tokenize `text` into words/spaces, attaching escape sequences to the
/// following token so wrapping never splits a sequence.
#[allow(unused_assignments)] // flush!() resets cur_width after the last call
fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens: Vec<Token> = Vec::new();
    let mut pending_esc = String::new();
    // Escapes claimed by the currently-open token (those seen before its
    // first char); escapes seen mid-token attach to the NEXT token, so a
    // RESET after a word stays after it.
    let mut cur_prefix = String::new();
    let mut cur = String::new();
    let mut cur_width = 0usize;
    let mut cur_space = false;
    let mut chars = text.chars().peekable();

    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                tokens.push(Token {
                    prefix: std::mem::take(&mut cur_prefix),
                    text: std::mem::take(&mut cur),
                    width: cur_width,
                    is_space: cur_space,
                });
                cur_width = 0;
            }
        };
    }

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            pending_esc.push('\x1b');
            match chars.next() {
                Some('[') => {
                    pending_esc.push('[');
                    for c in chars.by_ref() {
                        pending_esc.push(c);
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    pending_esc.push(']');
                    let mut prev_esc = false;
                    for c in chars.by_ref() {
                        pending_esc.push(c);
                        if c == '\x07' || (prev_esc && c == '\\') {
                            break;
                        }
                        prev_esc = c == '\x1b';
                    }
                }
                Some(other) => pending_esc.push(other),
                None => {}
            }
            continue;
        }
        let is_space = c.is_whitespace();
        if !cur.is_empty() && is_space != cur_space {
            flush!();
        }
        if cur.is_empty() {
            // Token starts here: claim escapes seen since the last token.
            cur_prefix = std::mem::take(&mut pending_esc);
            cur_space = is_space;
        }
        cur.push(c);
        if !is_space {
            cur_width += UnicodeWidthChar::width(c).unwrap_or(0);
        }
    }
    flush!();
    // Trailing escapes (e.g. a final RESET) become an empty token so they
    // are still emitted at end of line.
    if !pending_esc.is_empty() {
        tokens.push(Token {
            prefix: pending_esc,
            text: String::new(),
            width: 0,
            is_space: false,
        });
    }
    tokens
}

/// Wrap ANSI-styled text to `width` columns. Escape sequences are never
/// split and do not count toward the width. Existing newlines are hard
/// breaks. Words longer than the width are hard-split at char boundaries.
pub(crate) fn wrap_ansi(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out_lines: Vec<String> = Vec::new();
    for hard_line in text.split('\n') {
        let tokens = tokenize(hard_line);
        let mut line = String::new();
        let mut col = 0usize;
        // Escape sequences of a pending space (e.g. a RESET before a gap);
        // kept separate so they are never lost on a line break.
        let mut pending_esc = String::new();
        let mut pending_gap = false;
        macro_rules! break_line {
            () => {{
                // Close any dangling styles before the break.
                line.push_str(&pending_esc);
                pending_esc.clear();
                pending_gap = false;
                out_lines.push(std::mem::take(&mut line));
                col = 0;
            }};
        }
        for tok in tokens {
            if tok.text.is_empty() {
                // Pure-escape token (e.g. trailing RESET).
                line.push_str(&tok.prefix);
                continue;
            }
            if tok.is_space {
                pending_esc.push_str(&tok.prefix);
                pending_gap = true;
                continue;
            }
            if tok.width > width {
                // Hard-split an overlong word at char boundaries.
                if col > 0 {
                    break_line!();
                } else {
                    pending_esc.clear();
                    pending_gap = false;
                }
                line.push_str(&tok.prefix);
                for c in tok.text.chars() {
                    let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                    if col + cw > width {
                        break_line!();
                    }
                    line.push(c);
                    col += cw;
                }
                continue;
            }
            if col > 0 && col + 1 + tok.width > width {
                break_line!();
            }
            if col > 0 {
                if pending_gap {
                    line.push_str(&pending_esc);
                    line.push(' ');
                    col += 1;
                }
            } else {
                // Leading whitespace of a wrapped line is dropped, but its
                // escapes (style state) are kept.
                line.push_str(&pending_esc);
            }
            pending_esc.clear();
            pending_gap = false;
            line.push_str(&tok.prefix);
            line.push_str(&tok.text);
            col += tok.width;
        }
        line.push_str(&pending_esc);
        out_lines.push(std::mem::take(&mut line));
    }
    out_lines
}

/// Truncate plain text to at most `width` columns, appending '…' on cut.
fn truncate_plain(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut col = 0;
    for c in s.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if col + cw + 1 > width {
            break;
        }
        out.push(c);
        col += cw;
    }
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// syntect highlighting
// ---------------------------------------------------------------------------

static SYNTAXES: Lazy<Option<syntect::parsing::SyntaxSet>> =
    Lazy::new(|| {
        std::panic::catch_unwind(syntect::parsing::SyntaxSet::load_defaults_newlines).ok()
    });

fn theme_for(theme: Theme) -> Option<&'static syntect::highlighting::Theme> {
    static DARK: Lazy<Option<syntect::highlighting::ThemeSet>> =
        Lazy::new(|| std::panic::catch_unwind(syntect::highlighting::ThemeSet::load_defaults).ok());
    let set = DARK.as_ref()?;
    let name = match theme {
        Theme::Dark => "base16-ocean.dark",
        Theme::Light => "InspiredGitHub",
    };
    set.themes.get(name).or_else(|| set.themes.values().next())
}

/// Highlight `code` as `lang` using syntect's vendored defaults. Falls back
/// to dim plain text when syntect assets fail to load or the language is
/// unknown. Each emitted line is self-contained (reset at end).
fn highlight_code(code: &str, lang: &str, theme: Theme) -> String {
    let (Some(syntaxes), Some(theme)) = (SYNTAXES.as_ref(), theme_for(theme)) else {
        return plain_code(code);
    };
    let syntax = syntaxes
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| syntaxes.find_syntax_plain_text());
    let mut hl = syntect::easy::HighlightLines::new(syntax, theme);
    let mut out = String::new();
    for line in code.lines() {
        match hl.highlight_line(line, syntaxes) {
            Ok(regions) => {
                let mut last_fg = None;
                for (style, text) in regions {
                    let fg = style.foreground;
                    if Some((fg.r, fg.g, fg.b)) != last_fg {
                        out.push_str(&format!("\x1b[38;2;{};{};{}m", fg.r, fg.g, fg.b));
                        last_fg = Some((fg.r, fg.g, fg.b));
                    }
                    out.push_str(text);
                }
                out.push_str(RESET);
            }
            Err(_) => out.push_str(line),
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_string()
}

fn plain_code(code: &str) -> String {
    let mut out = String::new();
    for line in code.lines() {
        out.push_str(DIM);
        out.push_str(line);
        out.push_str(RESET);
        out.push('\n');
    }
    out.trim_end_matches('\n').to_string()
}

// ---------------------------------------------------------------------------
// math span extraction
// ---------------------------------------------------------------------------

pub(crate) struct MathSpan {
    pub tex: String,
    pub display: bool,
}

/// Private-use placeholder wrapping the span index; survives comrak
/// untouched and never occurs in real text.
fn placeholder(idx: usize) -> String {
    format!("\u{E000}{idx}\u{E001}")
}

fn render_span(span: &MathSpan, opts: &RenderOptions) -> String {
    let body = match opts.math {
        MathMode::Off => return span.tex.clone(),
        MathMode::Unicode => unicode_math(&span.tex),
        MathMode::Auto => {
            if opts.caps.graphics != crate::GraphicsProto::None {
                render_math(&span.tex, &opts.caps).unwrap_or_else(|_| unicode_math(&span.tex))
            } else {
                unicode_math(&span.tex)
            }
        }
        MathMode::Image => {
            render_math(&span.tex, &opts.caps).unwrap_or_else(|_| unicode_math(&span.tex))
        }
    };
    if span.display {
        format!("{FG_MAGENTA}{body}{RESET}")
    } else {
        body
    }
}

/// Pull `$...$` / `$$...$$` spans out of `md`, replacing them with
/// placeholders. Content inside fenced code blocks, inline code spans,
/// 4-space indented code blocks, link destinations (`[text](url)` and
/// `[label]: url` definitions) is left untouched (F5: `$` in a URL is not
/// math).
pub(crate) fn extract_math(md: &str) -> (String, Vec<MathSpan>) {
    let chars: Vec<char> = md.chars().collect();
    let mut out = String::new();
    let mut spans = Vec::new();
    let mut i = 0usize;
    let mut at_line_start = true;
    let mut fence: Option<(char, usize)> = None; // (char, run length)
    // Indented-code tracking: a line with >= 4 leading spaces is code when
    // the previous line was blank (or doc start) or itself indented code;
    // blank lines do not end the block. (Approximation of CommonMark: an
    // indented line inside a tight list continuation is treated as code.)
    let mut in_indented_code = false;
    let mut prev_blank = true; // doc start counts as blank
    let mut line_has_content = false;

    while i < chars.len() {
        // Fence tracking at line starts (up to 3 leading spaces).
        if at_line_start {
            let mut j = i;
            let mut spaces = 0;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
                spaces += 1;
            }
            let blank_line = j >= chars.len() || chars[j] == '\n';
            if fence.is_none() {
                // 4-space indented code block: copy the line verbatim.
                if spaces >= 4 && !blank_line && (prev_blank || in_indented_code) {
                    while i < chars.len() && chars[i] != '\n' {
                        out.push(chars[i]);
                        i += 1;
                    }
                    if i < chars.len() {
                        out.push('\n');
                        i += 1;
                    }
                    in_indented_code = true;
                    prev_blank = false;
                    line_has_content = false;
                    continue;
                }
                // Link reference definition `[label]: destination`: copy
                // the line verbatim (the URL may contain `$`).
                if !blank_line && chars[j] == '[' {
                    let mut k = j + 1;
                    while k < chars.len() && chars[k] != '\n' && chars[k] != ']' {
                        k += 1;
                    }
                    if k + 1 < chars.len() && chars[k] == ']' && chars[k + 1] == ':' {
                        while i < chars.len() && chars[i] != '\n' {
                            out.push(chars[i]);
                            i += 1;
                        }
                        in_indented_code = false;
                        line_has_content = true;
                        at_line_start = false;
                        continue;
                    }
                }
            }
            if j < chars.len() && (chars[j] == '`' || chars[j] == '~') && spaces < 4 {
                let fc = chars[j];
                let mut run = 0;
                while j + run < chars.len() && chars[j + run] == fc {
                    run += 1;
                }
                if run >= 3 {
                    match fence {
                        Some((c, n)) if c == fc && run >= n => fence = None,
                        None => fence = Some((fc, run)),
                        _ => {}
                    }
                }
            }
            at_line_start = false;
        }

        let c = chars[i];
        if c == '\n' {
            at_line_start = true;
            if line_has_content {
                in_indented_code = false; // a <4-indent content line ends the block
            }
            prev_blank = !line_has_content;
            line_has_content = false;
            out.push(c);
            i += 1;
            continue;
        }
        if c != ' ' {
            line_has_content = true;
        }
        if fence.is_some() {
            out.push(c);
            i += 1;
            continue;
        }

        // Inline code span: backtick run protected until matching run.
        if c == '`' {
            let mut run = 0;
            while i + run < chars.len() && chars[i + run] == '`' {
                run += 1;
            }
            let closing = find_backtick_run(&chars, i + run, run);
            let end = closing.map(|e| e + run).unwrap_or(i + run);
            for k in i..end {
                out.push(chars[k]);
                if chars[k] == '\n' {
                    at_line_start = true;
                }
            }
            i = end;
            continue;
        }

        // Inline link/image destination `](url)`: copy verbatim so a `$`
        // in the URL is not mistaken for math (F5). Balanced parens and
        // backslash escapes are honored; the scan stops at a newline.
        if c == ']' && i + 1 < chars.len() && chars[i + 1] == '(' {
            out.push(']');
            out.push('(');
            let mut j = i + 2;
            let mut depth = 1usize;
            while j < chars.len() && depth > 0 {
                let ch = chars[j];
                if ch == '\n' {
                    break; // leave the newline to the main loop
                }
                if ch == '\\' {
                    out.push(ch);
                    if j + 1 < chars.len() && chars[j + 1] != '\n' {
                        out.push(chars[j + 1]);
                        j += 2;
                    } else {
                        j += 1;
                    }
                    continue;
                }
                if ch == '(' {
                    depth += 1;
                }
                if ch == ')' {
                    depth -= 1;
                }
                out.push(ch);
                j += 1;
            }
            line_has_content = true;
            i = j;
            continue;
        }

        if c == '$' {
            let display = i + 1 < chars.len() && chars[i + 1] == '$';
            let start = i + if display { 2 } else { 1 };
            if let Some((end, tex)) = find_math_close(&chars, start, display) {
                let idx = spans.len();
                spans.push(MathSpan {
                    tex: tex.trim().to_string(),
                    display,
                });
                if display {
                    // Own paragraph so the placeholder lands on its own line.
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    out.push('\n');
                    out.push_str(&placeholder(idx));
                    out.push('\n');
                } else {
                    out.push_str(&placeholder(idx));
                }
                i = end + if display { 2 } else { 1 };
                continue;
            }
            out.push(c);
            i += 1;
            continue;
        }

        out.push(c);
        i += 1;
    }
    (out, spans)
}

/// Find the next backtick run of exactly `run` length starting at `from`;
/// returns the index where that run starts.
fn find_backtick_run(chars: &[char], from: usize, run: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '`' {
            let mut n = 0;
            while i + n < chars.len() && chars[i + n] == '`' {
                n += 1;
            }
            if n == run {
                return Some(i);
            }
            i += n;
        } else {
            i += 1;
        }
    }
    None
}

/// Find the closing `$`/`$$` for a math span whose content starts at
/// `from`. Returns (index of closing delimiter, content).
fn find_math_close(chars: &[char], from: usize, display: bool) -> Option<(usize, String)> {
    if from >= chars.len() {
        return None;
    }
    // Opening delimiter of *inline* math must not be followed by
    // whitespace; display math commonly starts with a newline.
    if !display && chars[from].is_whitespace() {
        return None;
    }
    let mut i = from;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            i += 2; // skip escaped char
            continue;
        }
        if c == '$' {
            let is_double = i + 1 < chars.len() && chars[i + 1] == '$';
            if is_double == display {
                let content: String = chars[from..i].iter().collect();
                if content.trim().is_empty() {
                    return None;
                }
                // Inline closing $ must not be preceded by whitespace.
                if !display && i > from && chars[i - 1].is_whitespace() {
                    return None;
                }
                return Some((i, content));
            }
            if !display && is_double {
                return None; // $$ closes nothing for an inline span
            }
            i += if is_double { 2 } else { 1 };
            continue;
        }
        if c == '\n' && !display {
            return None; // inline math cannot cross lines
        }
        i += 1;
    }
    None
}

/// Visible-caps helper used by tests and the CLI.
pub fn default_caps() -> TerminalCaps {
    TerminalCaps::detect()
}
