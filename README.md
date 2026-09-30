# mdterm

A byte-transparent PTY proxy + markdown render surfaces for AI coding CLIs
(Claude Code, Kimi CLI, Codex, or any tool emitting mdterm's self-defined
JSONL). The wrapped CLI behaves exactly as if run directly;
its session transcript is rendered — live — onto surfaces mdterm owns
(browser pane, terminal pager, tmux popup), never injected into the CLI's
own screen.

## Workspace layout

```
mdterm/
├── crates/
│   ├── mdterm-core/     # transcript model, discovery, watch, parse
│   ├── mdterm-pty/      # PTY proxy + hotkey interception
│   ├── mdterm-viewer/   # HTML viewer server (axum + SSE)
│   ├── mdterm-render/   # terminal ANSI renderer (Sprint 2)
│   └── mdterm-cli/      # binary `mdterm`, subcommand wiring
```

## Build

Requires a stable Rust toolchain (developed against 1.98; edition 2021).

```bash
cargo build --release          # binary at target/release/mdterm-llm
cargo test --workspace         # run the test suite
```

## Install

```bash
cargo install --path crates/mdterm-cli
# or copy the binary somewhere on PATH:
cp target/release/mdterm-llm ~/.local/bin/
```

## Usage

### Claude Code

```bash
mdterm-llm wrap                       # auto-detects `claude` on PATH
mdterm-llm wrap -- claude --debug     # explicit command + args
```

While wrapped, press a hotkey chord (see below): `Ctrl-G b` opens the live
browser viewer for the current session; `Ctrl-G r` renders the last
assistant message on a terminal surface (tmux popup / `less -R` / direct
dump, see "Terminal render surface" below).

### Kimi CLI

```bash
mdterm-llm wrap -- kimi
```

### Codex

```bash
mdterm-llm wrap -- codex
```

`mdterm-llm wrap` with no command auto-detects `claude`, then `kimi`, then
`codex` on PATH.

### Providers and transcripts

All transcript-reading commands accept `--provider <claude|codex|kimi|
selfdefined>` and `--transcript <path>`:

- With neither, the **newest transcript across all providers** is used
  (by mtime; see roots below) and parsed with that provider's parser.
- With `--transcript` alone, the format is **sniffed from the file's
  contents** — provider flags are optional for explicit files.
- With `--provider` alone, discovery is restricted to that provider's
  roots. `--provider selfdefined` has no roots and requires
  `--transcript`.

Discovery roots (newest `*.jsonl` wins):

| Provider | Roots |
|----------|-------|
| claude | `~/.claude/projects/**/` |
| kimi | `~/.kimi-code/sessions/**/`, `~/.kimi/sessions/**/` (legacy probes: `~/.kimi/projects/**/`, `~/.config/kimi/**/sessions`) |
| codex | `~/.codex/sessions/**/` |

Parsed formats: Claude Code session JSONL; kimi-code wire
(`agent.message.appended`, protocol ≥ 1.5), legacy kimi wire
(`TurnBegin`/`ContentPart`) and kimi `context.jsonl`; Codex rollouts
(`response_item` messages + tool calls, `event_msg` fallback);
selfdefined (below). Kimi `think` blocks are skipped; tool calls are
summarized as fenced JSON; tool outputs render as fenced `Tool` messages.

### Self-defined transcript format

Any tool (or script) can feed mdterm by writing the canonical JSONL —
one JSON object per line:

```json
{"type":"metadata","sessionId":"my-session","title":"optional"}
{"type":"message","role":"user","content":"markdown text","timestamp":"2026-09-30T13:00:00Z"}
{"type":"message","role":"assistant","content":[{"type":"text","text":"..."}],"model":"..."}
```

- `type`: `metadata` (optional, sets the session id via `sessionId` /
  `session_id` / `id`) or `message`.
- `role`: `user` | `assistant` | `system` | `tool` (others are skipped).
- `content`: a markdown string, or an array of `{"type":"text","text":…}`
  blocks (concatenated with blank lines).
- `timestamp`, `model`: optional strings, passed through to the UI.
- Unknown or malformed lines are skipped, never fatal.

### Standalone viewer

```bash
mdterm-llm serve [--transcript <path>] [--port N]   # default port 0 = OS-assigned
```

Watches the transcript and serves the viewer at `http://127.0.0.1:<port>/`.
Routes: `/` (single-page app), `/events` (SSE stream of full-session JSON),
`/api/session` (current session snapshot).

### One-shot render

```bash
mdterm-llm render [--last|--all] [--transcript <path>]
              [--file <path.md>] [--math auto|unicode|image|off] [--width N]
```

Renders ANSI straight to stdout. Default is the whole session (`--all`);
`--last` selects only the last assistant message. `--file` renders a plain
markdown file instead of a transcript. `--width` defaults to the terminal
width (or 100 when not a TTY). `--math` controls `$...$` / `$$...$$` math
(see "Math rendering" below; default `auto`).

### Terminal capabilities

```bash
mdterm-llm caps
```

Prints the detected terminal capabilities: terminal program, graphics
protocol (`kitty` / `iterm2` / `sixel` / `none`), and truecolor support,
plus the raw `TERM` / `TERM_PROGRAM` / `COLORTERM` / `TMUX` values.

Detection is env-based (first match wins):

| Signal | Result |
|--------|--------|
| `TERM_PROGRAM=iTerm.app` | iTerm2 inline images (OSC 1337) |
| `KITTY_WINDOW_ID` set, or `TERM` contains `kitty` | Kitty graphics protocol |
| `TERM_PROGRAM=WezTerm` / `WEZTERM_*`, `TERM_PROGRAM=ghostty` / `GHOSTTY_*` | Kitty graphics protocol (wezterm and ghostty implement it) |
| `TERM` contains `foot` or `sixel` | sixel |
| `COLORTERM=truecolor` (or `24bit`) | truecolor |

## Terminal render surface (`Ctrl-G r`)

On `Ctrl-G r`, `wrap` renders the last assistant message from the watched
transcript with `mdterm-render` and shows it on the best available surface:

1. **tmux popup** — when `$TMUX` is set and `tmux` is on PATH:
   `tmux display-popup -E -w 90% -h 85% "less -R <file>"` (a `cat`-based
   prompt is used if `less` is missing). tmux owns the popup's terminal, so
   the proxy needs no special handling here.
2. **`less -R`** — otherwise, when `less` is on PATH. The rendered ANSI is
   written to a temp file; `less` runs as a child of the proxy on the real
   terminal. The proxy first **suspends** its raw mode (restores the cooked
   termios and pauses its stdin pump so `less` has the terminal's input to
   itself), then **resumes** raw mode when `less` exits. This is implemented
   via `PtyProxy::spawn_with_terminal_guard` / `TerminalGuard::{suspend,resume}`
   in `mdterm-pty`, so the pager can never corrupt the proxy's terminal
   state.
3. **Direct dump** — last resort when neither tmux nor less exists: the
   rendered text is printed to stdout between markers (with cooked mode
   restored for the duration); the child TUI redraws over it on its next
   frame.

## Math rendering

`$...$` (inline) and `$$...$$` (display) spans are intercepted before
markdown parsing (spans inside code blocks/inline code are left literal)
and rendered according to `--math` / `MathMode`:

- `auto` (default): image rendering when the terminal has a graphics
  protocol, else the Unicode approximation.
- `unicode`: always the Unicode approximation — no external tools needed.
- `image`: force the image chain; falls back to Unicode if it fails.
- `off`: leave the TeX source as literal text.

**Image fallback chain** (each failure degrades one step; never panics):

1. `typst` CLI — a one-line math doc (common LaTeX constructs translated to
   typst math syntax) compiled to PNG.
2. `latex` + `dvipng` — `standalone` doc → DVI → tight-cropped PNG.
3. Unicode approximation.

A rendered PNG is displayed per the detected graphics protocol: Kitty APC
graphics escape (base64, `a=T,f=100`, 4096-byte chunks), iTerm2 OSC 1337,
or sixel — sixel only when an `img2sixel` converter is on PATH, otherwise
the chain degrades to Unicode.

**Unicode approximation coverage**: greek letters, common
operators/relations (`\sum`→∑, `\int`→∫, `\le`→≤, `\times`→×, arrows, set
operators, ...), `\frac{a}{b}`→`a/b` (parenthesized when compound),
`\sqrt{x}`→`√(x)`, super/subscripts mapped to real Unicode sup/subscript
glyphs where they exist (else `^(...)` / `_(...)`), sizing/spacing commands
dropped.

## Hotkeys

Active while running under `mdterm-llm wrap` (disable with `--hotkeys off`).
A chord is only consumed when it fully matches; all other input passes
through to the CLI untouched.

| Chord       | Action                                             |
|-------------|----------------------------------------------------|
| `Ctrl-G r`  | Render last assistant message on the terminal surface (tmux popup / pager) |
| `Ctrl-G b`  | Open / focus the live browser viewer for this session |

## The browser viewer

Dark, low-saturation single-page app. Renders GFM markdown (tables,
fenced code with syntax highlighting), KaTeX math (`$...$`, `$$...$$`,
`\(...\)`, `\[...\]`) and mermaid diagrams. Auto-scrolls to the newest
message; scroll up to pause following, scroll back to the bottom to resume.
Re-renders live on every transcript change via SSE; the header shows the
connection state (`live` / `reconnecting`).

All frontend assets are **vendored** (embedded in the binary at compile
time; no CDN is contacted at runtime):

| Asset | Version | License |
|-------|---------|---------|
| markdown-it | 14.1.0 | MIT |
| highlight.js (+ github-dark theme) | 11.10.0 | BSD-3-Clause |
| KaTeX (+ woff2 fonts) | 0.16.11 | MIT |
| mermaid | 11.4.1 | MIT |

## Troubleshooting

- **`no transcript found ... pass --transcript <path>`** — mdterm looks for
  the newest `*.jsonl` under the provider roots (see "Providers and
  transcripts"). If no CLI has written a session yet (or `$HOME` differs),
  pass the file explicitly; the format is sniffed automatically.
- **Wrong provider picked up** — pass `--provider <claude|codex|kimi|
  selfdefined>` to pin the parser. Sniffing keys on distinctive line
  shapes, so a file whose first 200 lines contain no recognizable line
  falls back to the Claude parser (which tolerantly skips unknown lines).
- **Browser does not open** — `wrap`/`render` use `open` on macOS and
  `xdg-open` on Linux. If neither works (headless/SSH), copy the printed
  `http://127.0.0.1:<port>/` URL into any browser; for remote sessions use
  SSH port forwarding: `ssh -L <port>:127.0.0.1:<port> host`.
- **Viewer shows an old session** — discovery picks the *newest* transcript;
  an older session from another project may win. Use `--transcript`.
- **Hotkeys do nothing** — check they were not disabled (`--hotkeys off`),
  and that the chord is pressed as a sequence (Ctrl-G, release, then `r`/`b`).
- **Debug logging** — set `RUST_LOG=mdterm_llm=debug` (logs go to stderr).

## Development

```bash
cargo build --workspace
cargo test --workspace
```

Sprint status: **Sprint 1** (core/pty/viewer/cli + browser surface) and
**Sprint 2** (`mdterm-render` terminal ANSI renderer with math/images,
capability detection, tmux/pager terminal surface, `render --file/--math/
--width`, `caps`) complete. Post-Sprint: multi-provider transcript support
(kimi/codex/selfdefined parsers, format sniffing, cross-provider discovery)
and the F7 dead-keyboard fix.

## Author

Yifan Yang <yfyang.86@hotmail.com>

## License

MIT — see [LICENSE](LICENSE).

Vendored frontend assets (see "The browser viewer") retain their own
licenses, all permissive and MIT-compatible: markdown-it (MIT),
highlight.js (BSD-3-Clause), KaTeX (MIT), mermaid (MIT).
