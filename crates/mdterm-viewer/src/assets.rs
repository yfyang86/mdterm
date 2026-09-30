//! Vendored (compile-time embedded) frontend assets.
//!
//! Downloaded once during development from cdn.jsdelivr.net:
//!   - markdown-it 14.1.0      (MIT)  https://github.com/markdown-it/markdown-it
//!   - highlight.js 11.10.0    (BSD-3) https://highlightjs.org + github-dark theme
//!   - KaTeX 0.16.11           (MIT)  https://katex.org (+ woff2 fonts)
//!   - mermaid 11.4.1          (MIT)  https://mermaid.js.org
//!
//! No CDN is contacted at runtime; everything is served from `/assets/...`.

/// Look up an embedded asset by its path under `/assets/`.
/// Returns `(content_type, body)`.
pub fn lookup(path: &str) -> Option<(&'static str, &'static [u8])> {
    let (ct, body): (&str, &[u8]) = match path.trim_start_matches('/') {
        "markdown-it.min.js" => (JS, include_bytes!("../assets/markdown-it.min.js")),
        "highlight.min.js" => (JS, include_bytes!("../assets/highlight.min.js")),
        "katex.min.js" => (JS, include_bytes!("../assets/katex.min.js")),
        "mermaid.min.js" => (JS, include_bytes!("../assets/mermaid.min.js")),
        "katex.min.css" => (CSS, include_bytes!("../assets/katex.min.css")),
        "hljs-github-dark.min.css" => (CSS, include_bytes!("../assets/hljs-github-dark.min.css")),

        // KaTeX woff2 fonts (referenced relatively as fonts/... by katex.min.css)
        "fonts/KaTeX_AMS-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_AMS-Regular.woff2")),
        "fonts/KaTeX_Caligraphic-Bold.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Caligraphic-Bold.woff2")),
        "fonts/KaTeX_Caligraphic-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Caligraphic-Regular.woff2")),
        "fonts/KaTeX_Fraktur-Bold.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Fraktur-Bold.woff2")),
        "fonts/KaTeX_Fraktur-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Fraktur-Regular.woff2")),
        "fonts/KaTeX_Main-Bold.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Main-Bold.woff2")),
        "fonts/KaTeX_Main-BoldItalic.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Main-BoldItalic.woff2")),
        "fonts/KaTeX_Main-Italic.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Main-Italic.woff2")),
        "fonts/KaTeX_Main-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Main-Regular.woff2")),
        "fonts/KaTeX_Math-BoldItalic.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Math-BoldItalic.woff2")),
        "fonts/KaTeX_Math-Italic.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Math-Italic.woff2")),
        "fonts/KaTeX_SansSerif-Bold.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_SansSerif-Bold.woff2")),
        "fonts/KaTeX_SansSerif-Italic.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_SansSerif-Italic.woff2")),
        "fonts/KaTeX_SansSerif-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_SansSerif-Regular.woff2")),
        "fonts/KaTeX_Script-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Script-Regular.woff2")),
        "fonts/KaTeX_Size1-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Size1-Regular.woff2")),
        "fonts/KaTeX_Size2-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Size2-Regular.woff2")),
        "fonts/KaTeX_Size3-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Size3-Regular.woff2")),
        "fonts/KaTeX_Size4-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Size4-Regular.woff2")),
        "fonts/KaTeX_Typewriter-Regular.woff2" => (WOFF2, include_bytes!("../assets/fonts/KaTeX_Typewriter-Regular.woff2")),
        _ => return None,
    };
    Some((ct, body))
}

const JS: &str = "text/javascript; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const WOFF2: &str = "font/woff2";
