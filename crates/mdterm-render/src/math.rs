//! TeX math rendering: image renderers (typst, latex+dvipng) with terminal
//! graphics escapes, or a pure-Unicode approximation. Never panics; every
//! failure degrades one step down the chain:
//!
//! ```text
//! typst (latex-ish -> typst translation, `typst compile` -> PNG)
//!   -> latex + dvipng (`latex` -> DVI, `dvipng` -> PNG)
//!     -> Unicode approximation
//! ```
//!
//! PNG display follows [`TerminalCaps`]: Kitty APC graphics, iTerm2
//! OSC 1337, sixel (only when an `img2sixel` converter exists), otherwise
//! the Unicode approximation.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, Result};
use base64::Engine as _;

use crate::caps::{GraphicsProto, TerminalCaps};

/// Render a TeX math fragment for the terminal described by `caps`.
///
/// Tries external image renderers in order (`typst`, then `latex`+`dvipng`)
/// and, on success, emits the graphics escape sequence matching `caps`.
/// Falls back to the Unicode approximation when no renderer is available,
/// rendering fails, or the terminal has no usable graphics protocol.
pub fn render_math(tex: &str, caps: &TerminalCaps) -> Result<String> {
    if tex.trim().is_empty() {
        return Ok(String::new());
    }
    if let Some(png) = render_math_png(tex) {
        if let Some(escape) = png_escape(&png, caps) {
            return Ok(escape);
        }
    }
    Ok(unicode_math(tex))
}

// ---------------------------------------------------------------------------
// external renderers
// ---------------------------------------------------------------------------

/// Try `typst`, then `latex`+`dvipng`; return PNG bytes on success.
fn render_math_png(tex: &str) -> Option<Vec<u8>> {
    if which("typst").is_some() {
        match typst_png(tex) {
            Ok(png) => return Some(png),
            Err(e) => tracing::debug!("typst math render failed: {e:#}"),
        }
    }
    if which("latex").is_some() && which("dvipng").is_some() {
        match latex_dvipng(tex) {
            Ok(png) => return Some(png),
            Err(e) => tracing::debug!("latex+dvipng math render failed: {e:#}"),
        }
    }
    None
}

/// typst math syntax is not LaTeX; do a best-effort translation of common
/// LaTeX constructs before compiling a one-line math doc.
fn typst_png(tex: &str) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let doc = format!(
        "#set page(width: auto, height: auto, margin: 4pt)\n$ {} $\n",
        latex_to_typst(tex)
    );
    let src = dir.path().join("math.typ");
    std::fs::write(&src, doc)?;
    let out = dir.path().join("math.png");
    let status = Command::new("typst")
        .arg("compile")
        .arg("--format")
        .arg("png")
        .arg(&src)
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !status.success() || !out.is_file() {
        return Err(anyhow!("typst compile failed"));
    }
    Ok(std::fs::read(out)?)
}

fn latex_dvipng(tex: &str) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let doc = format!(
        "\\documentclass[preview]{{standalone}}\n\\usepackage{{amsmath,amssymb}}\n\\begin{{document}}\n${}$\n\\end{{document}}\n",
        tex
    );
    std::fs::write(dir.path().join("math.tex"), doc)?;
    let ok = Command::new("latex")
        .args(["-interaction=nonstopmode", "-halt-on-error", "math.tex"])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success();
    if !ok || !dir.path().join("math.dvi").is_file() {
        return Err(anyhow!("latex failed"));
    }
    let ok = Command::new("dvipng")
        .args(["-T", "tight", "-D", "150", "-png", "-o", "math.png", "math.dvi"])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success();
    let png = dir.path().join("math.png");
    if !ok || !png.is_file() {
        return Err(anyhow!("dvipng failed"));
    }
    Ok(std::fs::read(png)?)
}

// ---------------------------------------------------------------------------
// graphics escapes
// ---------------------------------------------------------------------------

/// Encode PNG bytes as the graphics escape sequence for `caps`. Returns
/// `None` when the protocol is unavailable (caller falls back to Unicode).
fn png_escape(png: &[u8], caps: &TerminalCaps) -> Option<String> {
    match caps.graphics {
        GraphicsProto::Kitty => Some(kitty_escape(png)),
        GraphicsProto::ITerm2 => Some(iterm2_escape(png)),
        GraphicsProto::Sixel => sixel_escape(png),
        GraphicsProto::None => None,
    }
}

/// Kitty graphics protocol: transmit-and-display (`a=T`), PNG (`f=100`),
/// base64 payload chunked into <=4096-byte segments with `m=` continuation.
///
/// Protocol rules (F4): only the FIRST chunk carries the action/format keys
/// (`a=T,f=100`); continuation chunks must carry only `m` (plus optionally
/// `q`). Every chunk also carries `q=2` so the terminal does not send APC
/// responses at all — error replies would otherwise be delivered as input
/// to whatever is reading the terminal (e.g. the pager).
fn kitty_escape(png: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    let mut out = String::new();
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            let more = if chunks.len() > 1 { ",m=1" } else { "" };
            out.push_str(&format!("\x1b_Gq=2,a=T,f=100{more};{chunk}\x1b\\"));
        } else {
            let more = if i + 1 < chunks.len() { 1 } else { 0 };
            out.push_str(&format!("\x1b_Gq=2,m={more};{chunk}\x1b\\"));
        }
    }
    out.push('\n');
    out
}

/// iTerm2 OSC 1337 inline image: base64 PNG, terminated by BEL.
fn iterm2_escape(png: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png);
    format!("\x1b]1337;File=inline=1:{b64}\x07\n")
}

/// sixel has no direct PNG embedding; convert via `img2sixel` when present,
/// otherwise signal unavailability (Unicode fallback).
fn sixel_escape(png: &[u8]) -> Option<String> {
    which("img2sixel")?;
    let mut file = tempfile::Builder::new().suffix(".png").tempfile().ok()?;
    file.write_all(png).ok()?;
    file.flush().ok()?;
    let out = Command::new("img2sixel")
        .arg(file.path())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !s.ends_with('\n') {
        s.push('\n');
    }
    Some(s)
}

/// Minimal `which(1)`: executable named `name` on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join(name);
        let exec = candidate.is_file() && is_executable(&candidate);
        exec.then_some(candidate)
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod kitty_tests {
    use super::kitty_escape;
    use base64::Engine as _;

    #[test]
    fn single_chunk_carries_action_format_and_quiet() {
        let out = kitty_escape(b"\x89PNG\r\n\x1a\n");
        assert!(out.starts_with("\x1b_Gq=2,a=T,f=100;"), "{out:?}");
        assert!(!out.contains("m="), "no m= for a single chunk: {out:?}");
        assert_eq!(out.matches("\x1b_G").count(), 1, "one APC: {out:?}");
        assert!(out.ends_with("\x1b\\\n"), "terminated: {out:?}");
    }

    #[test]
    fn continuation_chunks_carry_only_m_and_q() {
        // 8192 bytes -> 10924 base64 chars -> 3 chunks (4096/4096/2732).
        let payload = vec![b'x'; 8192];
        let out = kitty_escape(&payload);
        let chunks: Vec<&str> = out
            .split("\x1b\\")
            .filter(|s| s.starts_with("\x1b_G"))
            .collect();
        assert_eq!(chunks.len(), 3, "expected 3 chunks: {out:?}");
        assert!(
            chunks[0].starts_with("\x1b_Gq=2,a=T,f=100,m=1;"),
            "first chunk: {:?}",
            chunks[0]
        );
        for c in &chunks[1..] {
            assert!(
                !c.contains("a=T") && !c.contains("f=100"),
                "continuation chunk must not repeat action/format keys: {c:?}"
            );
            assert!(c.starts_with("\x1b_Gq=2,m="), "continuation keys: {c:?}");
        }
        assert!(chunks[1].starts_with("\x1b_Gq=2,m=1;"), "{:?}", chunks[1]);
        assert!(
            chunks[2].starts_with("\x1b_Gq=2,m=0;"),
            "last chunk ends the transfer: {:?}",
            chunks[2]
        );
        // Payloads reassemble to the base64 of the original bytes.
        let b64: String = chunks
            .iter()
            .map(|c| c.splitn(2, ';').nth(1).unwrap())
            .collect();
        assert_eq!(b64, base64::engine::general_purpose::STANDARD.encode(&payload));
    }
}

// ---------------------------------------------------------------------------
// LaTeX -> typst translation (best effort)
// ---------------------------------------------------------------------------

/// Translate common LaTeX math into typst math syntax. Unknown commands are
/// passed through with the backslash stripped (typst often has a same-named
/// symbol); anything that still fails to compile just degrades to the next
/// renderer in the chain.
fn latex_to_typst(tex: &str) -> String {
    let mut out = unicode_transform(tex, &TypstSink);
    // unicode_transform leaves plain names in place for TypstSink; nothing
    // further needed here.
    std::mem::take(&mut out)
}

// ---------------------------------------------------------------------------
// Unicode approximation
// ---------------------------------------------------------------------------

/// Pure-Rust Unicode approximation of a TeX math fragment. Handles greek
/// letters, common operators/relations, `\frac{a}{b}` -> `a/b`, `\sqrt{x}`
/// -> `√(x)`, super/subscripts (with real Unicode sup/sub characters where
/// they exist), and drops sizing/spacing commands. Anything unrecognized is
/// passed through with the backslash stripped.
pub fn unicode_math(tex: &str) -> String {
    unicode_transform(tex, &UnicodeSink)
}

/// Output-flavor abstraction shared by the typst translator and the
/// Unicode approximation: both walk the same token stream but emit either
/// typst names or Unicode glyphs.
trait MathSink {
    fn command(&self, name: &str) -> Option<String>;
    fn frac(&self, num: &str, den: &str) -> String;
    fn sqrt(&self, arg: &str) -> String;
    fn sup(&self, base_end: &mut String, arg: &str) -> String;
    fn sub(&self, base_end: &mut String, arg: &str) -> String;
}

struct UnicodeSink;
struct TypstSink;

/// Map a single char to its Unicode superscript form.
fn sup_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'i' => 'ⁱ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'n' => 'ⁿ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        _ => return None,
    })
}

/// Map a single char to its Unicode subscript form.
fn sub_char(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

fn to_sup(s: &str) -> Option<String> {
    s.chars().map(sup_char).collect()
}

fn to_sub(s: &str) -> Option<String> {
    s.chars().map(sub_char).collect()
}

/// Parenthesize a group when it contains more than one "atom".
fn paren_if_complex(s: &str) -> String {
    if s.chars().count() <= 1 {
        s.to_string()
    } else {
        format!("({s})")
    }
}

impl MathSink for UnicodeSink {
    fn command(&self, name: &str) -> Option<String> {
        Some(
            match name {
                // greek lowercase
                "alpha" => "α",
                "beta" => "β",
                "gamma" => "γ",
                "delta" => "δ",
                "epsilon" | "varepsilon" => "ε",
                "zeta" => "ζ",
                "eta" => "η",
                "theta" | "vartheta" => "θ",
                "iota" => "ι",
                "kappa" => "κ",
                "lambda" => "λ",
                "mu" => "μ",
                "nu" => "ν",
                "xi" => "ξ",
                "pi" => "π",
                "rho" | "varrho" => "ρ",
                "sigma" | "varsigma" => "σ",
                "tau" => "τ",
                "upsilon" => "υ",
                "phi" | "varphi" => "φ",
                "chi" => "χ",
                "psi" => "ψ",
                "omega" => "ω",
                // greek uppercase
                "Gamma" => "Γ",
                "Delta" => "Δ",
                "Theta" => "Θ",
                "Lambda" => "Λ",
                "Xi" => "Ξ",
                "Pi" => "Π",
                "Sigma" => "Σ",
                "Phi" => "Φ",
                "Psi" => "Ψ",
                "Omega" => "Ω",
                // big operators & relations
                "sum" => "∑",
                "prod" => "∏",
                "int" => "∫",
                "iint" => "∬",
                "oint" => "∮",
                "infty" => "∞",
                "partial" => "∂",
                "nabla" => "∇",
                "pm" => "±",
                "mp" => "∓",
                "times" => "×",
                "div" => "÷",
                "cdot" => "·",
                "ast" => "∗",
                "circ" => "∘",
                "bullet" => "∙",
                "le" | "leq" => "≤",
                "ge" | "geq" => "≥",
                "ne" | "neq" => "≠",
                "equiv" => "≡",
                "approx" => "≈",
                "sim" => "∼",
                "simeq" => "≃",
                "propto" => "∝",
                "ll" => "≪",
                "gg" => "≫",
                "in" => "∈",
                "notin" => "∉",
                "ni" => "∋",
                "subset" => "⊂",
                "supset" => "⊃",
                "subseteq" => "⊆",
                "supseteq" => "⊇",
                "cup" => "∪",
                "cap" => "∩",
                "setminus" => "∖",
                "emptyset" => "∅",
                "forall" => "∀",
                "exists" => "∃",
                "neg" | "lnot" => "¬",
                "land" | "wedge" => "∧",
                "lor" | "vee" => "∨",
                "oplus" => "⊕",
                "otimes" => "⊗",
                "perp" => "⊥",
                "parallel" => "∥",
                "angle" => "∠",
                "degree" => "°",
                "prime" => "′",
                "rightarrow" | "to" => "→",
                "leftarrow" | "gets" => "←",
                "leftrightarrow" => "↔",
                "Rightarrow" => "⇒",
                "Leftarrow" => "⇐",
                "Leftrightarrow" => "⇔",
                "mapsto" => "↦",
                "uparrow" => "↑",
                "downarrow" => "↓",
                "ldots" | "dots" => "…",
                "cdots" => "⋯",
                "vdots" => "⋮",
                "ddots" => "⋱",
                "hbar" => "ℏ",
                "ell" => "ℓ",
                "Re" => "ℜ",
                "Im" => "ℑ",
                "aleph" => "ℵ",
                "mathbb{R}" => "ℝ",
                "mathbb{Z}" => "ℤ",
                "mathbb{N}" => "ℕ",
                "mathbb{Q}" => "ℚ",
                "mathbb{C}" => "ℂ",
                // spacing / styling: drop
                "," | ";" | "!" | " " | "quad" | "qquad" | "displaystyle" | "limits"
                | "left" | "right" | "big" | "Big" | "bigg" | "Bigg" | "textstyle" => "",
                _ => return None,
            }
            .to_string(),
        )
    }

    fn frac(&self, num: &str, den: &str) -> String {
        format!("{}/{}", paren_if_complex(num), paren_if_complex(den))
    }

    fn sqrt(&self, arg: &str) -> String {
        format!("√{}", paren_if_complex(arg))
    }

    fn sup(&self, _base: &mut String, arg: &str) -> String {
        match to_sup(arg) {
            Some(s) => s,
            None => format!("^{}", paren_if_complex(arg)),
        }
    }

    fn sub(&self, _base: &mut String, arg: &str) -> String {
        match to_sub(arg) {
            Some(s) => s,
            None => format!("_{}", paren_if_complex(arg)),
        }
    }
}

impl MathSink for TypstSink {
    fn command(&self, name: &str) -> Option<String> {
        Some(
            match name {
                // typst uses the same names for greek and most operators.
                "varepsilon" => "epsilon",
                "vartheta" => "theta",
                "le" | "leq" => "<=",
                "ge" | "geq" => ">=",
                "ne" | "neq" => "!=",
                "times" => "times",
                "cdot" => "dot",
                "rightarrow" | "to" => "->",
                "leftarrow" | "gets" => "<-",
                "Rightarrow" => "=>",
                "Leftarrow" => "<=",
                "mathbb{R}" => "RR",
                "mathbb{Z}" => "ZZ",
                "mathbb{N}" => "NN",
                "mathbb{Q}" => "QQ",
                "mathbb{C}" => "CC",
                "," | ";" | " " | "quad" => " ",
                "!" | "displaystyle" | "limits" | "left" | "right" | "big" | "Big"
                | "bigg" | "Bigg" | "textstyle" | "qquad" => "",
                other => return Some(other.to_string()),
            }
            .to_string(),
        )
    }

    fn frac(&self, num: &str, den: &str) -> String {
        format!("frac({num}, {den})")
    }

    fn sqrt(&self, arg: &str) -> String {
        format!("sqrt({arg})")
    }

    fn sup(&self, _base: &mut String, arg: &str) -> String {
        format!("^({arg})")
    }

    fn sub(&self, _base: &mut String, arg: &str) -> String {
        format!("_({arg})")
    }
}

// ---------------------------------------------------------------------------
// shared TeX-ish token walker
// ---------------------------------------------------------------------------

/// One-level recursive walker over a TeX-ish token stream, emitting via
/// `sink`. Handles `\command`, `{groups}`, `^{...}`/`_{...}` scripts,
/// `\frac`, `\sqrt`, `\mathbb{X}`, and passes anything else through.
fn unicode_transform(tex: &str, sink: &dyn MathSink) -> String {
    let chars: Vec<char> = tex.chars().collect();
    let mut pos = 0usize;
    transform_group(&chars, &mut pos, sink, false)
}

/// Transform tokens until end of input or (when `in_group`) the closing
/// brace that ends the current group.
fn transform_group(
    chars: &[char],
    pos: &mut usize,
    sink: &dyn MathSink,
    in_group: bool,
) -> String {
    let mut out = String::new();
    while *pos < chars.len() {
        let c = chars[*pos];
        match c {
            '}' if in_group => {
                *pos += 1;
                return out;
            }
            '{' => {
                *pos += 1;
                out.push_str(&transform_group(chars, pos, sink, true));
            }
            '}' => {
                *pos += 1; // stray brace: drop
            }
            '^' | '_' => {
                *pos += 1;
                let arg = read_script_arg(chars, pos, sink);
                let rendered = if c == '^' {
                    sink.sup(&mut out, &arg)
                } else {
                    sink.sub(&mut out, &arg)
                };
                out.push_str(&rendered);
            }
            '\\' => {
                *pos += 1;
                transform_command(chars, pos, sink, &mut out);
            }
            c if c.is_whitespace() => {
                *pos += 1;
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
            }
            _ => {
                *pos += 1;
                out.push(c);
            }
        }
    }
    out
}

/// Read a script argument: either `{...}` or a single token.
fn read_script_arg(chars: &[char], pos: &mut usize, sink: &dyn MathSink) -> String {
    while *pos < chars.len() && chars[*pos].is_whitespace() {
        *pos += 1;
    }
    if *pos >= chars.len() {
        return String::new();
    }
    if chars[*pos] == '{' {
        *pos += 1;
        transform_group(chars, pos, sink, true)
    } else {
        // single token (char or command)
        let mut one = String::new();
        if chars[*pos] == '\\' {
            *pos += 1;
            transform_command(chars, pos, sink, &mut one);
        } else {
            one.push(chars[*pos]);
            *pos += 1;
        }
        one
    }
}

/// Handle a `\command` at `pos` (just past the backslash).
fn transform_command(chars: &[char], pos: &mut usize, sink: &dyn MathSink, out: &mut String) {
    if *pos >= chars.len() {
        return;
    }
    // Control symbols like \, \; \{ \} are single non-letter chars.
    if !chars[*pos].is_ascii_alphabetic() {
        let sym = chars[*pos];
        *pos += 1;
        match sym {
            '{' => out.push('{'),
            '}' => out.push('}'),
            '$' | '%' | '&' | '#' | '_' => out.push(sym),
            other => {
                if let Some(s) = sink.command(&other.to_string()) {
                    out.push_str(&s);
                }
            }
        }
        return;
    }
    let start = *pos;
    while *pos < chars.len() && chars[*pos].is_ascii_alphabetic() {
        *pos += 1;
    }
    let name: String = chars[start..*pos].iter().collect();
    match name.as_str() {
        "frac" | "dfrac" | "tfrac" => {
            let num = read_braced_group(chars, pos, sink);
            let den = read_braced_group(chars, pos, sink);
            out.push_str(&sink.frac(&num, &den));
        }
        "sqrt" => {
            skip_optional_bracket(chars, pos);
            let arg = read_braced_group(chars, pos, sink);
            out.push_str(&sink.sqrt(&arg));
        }
        "mathbb" | "mathcal" | "mathrm" | "mathbf" | "mathit" | "operatorname" | "text"
        | "textrm" | "textbf" => {
            let arg = read_braced_group(chars, pos, sink);
            let key = format!("mathbb{{{arg}}}");
            if name == "mathbb" {
                if let Some(s) = sink.command(&key) {
                    out.push_str(&s);
                    return;
                }
            }
            out.push_str(&arg);
        }
        _ => match sink.command(&name) {
            Some(s) => out.push_str(&s),
            None => out.push_str(&name),
        },
    }
}

/// Read a `{...}` group (or a single token) as an argument.
fn read_braced_group(chars: &[char], pos: &mut usize, sink: &dyn MathSink) -> String {
    while *pos < chars.len() && chars[*pos].is_whitespace() {
        *pos += 1;
    }
    if *pos < chars.len() && chars[*pos] == '{' {
        *pos += 1;
        transform_group(chars, pos, sink, true)
    } else {
        read_script_arg(chars, pos, sink)
    }
}

/// Skip an optional `[...]` argument (e.g. `\sqrt[3]{x}` — the index is
/// dropped in the approximation).
fn skip_optional_bracket(chars: &[char], pos: &mut usize) {
    while *pos < chars.len() && chars[*pos].is_whitespace() {
        *pos += 1;
    }
    if *pos < chars.len() && chars[*pos] == '[' {
        let mut depth = 0usize;
        while *pos < chars.len() {
            match chars[*pos] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        *pos += 1;
                        return;
                    }
                }
                _ => {}
            }
            *pos += 1;
        }
    }
}
