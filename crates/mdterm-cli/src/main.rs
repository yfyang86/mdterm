//! mdterm: byte-transparent PTY proxy + markdown render surfaces for AI
//! coding CLIs (Claude Code, Kimi CLI). See SPEC.md / README.md.

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context};
use clap::{Args, Parser, Subcommand, ValueEnum};
use mdterm_core::{
    discover_latest_any, discover_latest_session, parse_transcript, watch_transcript, CliKind,
    Role, Session, TranscriptEvent,
};
use mdterm_pty::{HotkeyConfig, ProxyEvent, PtyProxy, TerminalGuard};
use mdterm_render::{render_markdown, MathMode, RenderOptions, TerminalCaps, Theme};
use mdterm_viewer::ViewerServer;

#[derive(Parser)]
#[command(
    name = "mdterm-llm",
    version,
    about = "Wrap AI coding CLIs and render their markdown output on separate surfaces"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// PTY-proxy the CLI (default: auto-detect `claude` or `kimi` on PATH).
    Wrap(WrapArgs),
    /// Run the viewer server standalone, watching a transcript.
    Serve(ServeArgs),
    /// One-shot ANSI render of a session (or a markdown file) to stdout.
    Render(RenderArgs),
    /// Print detected terminal capabilities.
    Caps,
}

#[derive(Args)]
struct WrapArgs {
    /// Transcript file to watch (default: discover latest for the wrapped CLI).
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// Transcript provider (default: inferred from the wrapped command).
    #[arg(long, value_enum)]
    provider: Option<ProviderArg>,
    /// Enable/disable hotkey chord interception (Ctrl-G r / Ctrl-G b).
    #[arg(long, value_enum, default_value_t = HotkeyMode::On)]
    hotkeys: HotkeyMode,
    /// Command (and args) to wrap, e.g. `mdterm-llm wrap -- claude --debug`.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    cmd: Vec<String>,
}

#[derive(Args)]
struct ServeArgs {
    /// Transcript file to watch (default: discover latest across providers).
    #[arg(long)]
    transcript: Option<PathBuf>,
    /// Transcript provider (default: inferred from discovery / sniffed).
    #[arg(long, value_enum)]
    provider: Option<ProviderArg>,
    /// Port to bind (0 = OS-assigned).
    #[arg(long, default_value_t = 0)]
    port: u16,
}

#[derive(Args)]
struct RenderArgs {
    /// Render only the last assistant message.
    #[arg(long, conflicts_with = "all")]
    last: bool,
    /// Render the whole session (default).
    #[arg(long)]
    all: bool,
    /// Transcript file to render (default: discover latest across providers).
    #[arg(long, conflicts_with = "file")]
    transcript: Option<PathBuf>,
    /// Transcript provider (default: sniffed from the file's contents).
    #[arg(long, value_enum)]
    provider: Option<ProviderArg>,
    /// Render a plain markdown file instead of a transcript.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Math rendering mode.
    #[arg(long, value_enum, default_value_t = MathArg::Auto)]
    math: MathArg,
    /// Wrap width in columns (default: terminal width, else 100).
    #[arg(long)]
    width: Option<usize>,
}

/// `--provider` values, mapping 1:1 onto [`CliKind`].
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProviderArg {
    Claude,
    Codex,
    Kimi,
    Selfdefined,
}

impl From<ProviderArg> for CliKind {
    fn from(p: ProviderArg) -> Self {
        match p {
            ProviderArg::Claude => CliKind::Claude,
            ProviderArg::Codex => CliKind::Codex,
            ProviderArg::Kimi => CliKind::Kimi,
            ProviderArg::Selfdefined => CliKind::Selfdefined,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MathArg {
    Auto,
    Unicode,
    Image,
    Off,
}

impl From<MathArg> for MathMode {
    fn from(m: MathArg) -> Self {
        match m {
            MathArg::Auto => MathMode::Auto,
            MathArg::Unicode => MathMode::Unicode,
            MathArg::Image => MathMode::Image,
            MathArg::Off => MathMode::Off,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum HotkeyMode {
    On,
    Off,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mdterm_llm=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Commands::Wrap(args) => wrap(args).await,
        Commands::Serve(args) => serve(args).await,
        Commands::Render(args) => render(args).await,
        Commands::Caps => caps(),
    }
}

// ---------------------------------------------------------------------------
// wrap
// ---------------------------------------------------------------------------

async fn wrap(args: WrapArgs) -> anyhow::Result<()> {
    let (cmd, cmd_args) = resolve_wrap_command(&args.cmd)?;
    let kind = args
        .provider
        .map(CliKind::from)
        .unwrap_or_else(|| cli_kind_for_command(&cmd));

    let mut hotkeys = HotkeyConfig::default();
    hotkeys.enabled = args.hotkeys == HotkeyMode::On;

    let (mut child, mut events, terminal) =
        PtyProxy::spawn_with_terminal_guard(&cmd, &cmd_args, hotkeys)
            .with_context(|| format!("failed to spawn `{cmd}` under the PTY proxy"))?;

    // Viewer server state: started lazily on the first RenderBrowser chord.
    let mut viewer_port: Option<u16> = None;
    let mut transcript = args.transcript;

    // Shared session state for the terminal-render surface: a background
    // task feeds watch_transcript events into this slot (started lazily,
    // since transcript discovery may only succeed once the CLI is running).
    let session: Arc<RwLock<Session>> = Arc::new(RwLock::new(Session::empty(kind)));
    let mut session_task: Option<tokio::task::JoinHandle<()>> = None;

    let exit_code = loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(ProxyEvent::RenderBrowser) => {
                    if viewer_port.is_none() {
                        if transcript.is_none() {
                            transcript = discover_latest_session(kind);
                        }
                        match &transcript {
                            Some(path) => match start_viewer(path.clone(), Some(kind), 0).await {
                                Ok(port) => {
                                    viewer_port = Some(port);
                                    eprintln!("[mdterm] viewer: http://127.0.0.1:{port}/ (watching {})", path.display());
                                }
                                Err(e) => {
                                    eprintln!("[mdterm] failed to start viewer server: {e:#}");
                                    continue;
                                }
                            },
                            None => {
                                eprintln!("[mdterm] no transcript found for {kind:?}; pass --transcript <path>");
                                continue;
                            }
                        }
                    }
                    if let Some(port) = viewer_port {
                        open_browser(&format!("http://127.0.0.1:{port}/"));
                    }
                }
                Some(ProxyEvent::RenderTerminal) => {
                    if session_task.is_none() {
                        if transcript.is_none() {
                            transcript = discover_latest_session(kind);
                        }
                        if let Some(path) = &transcript {
                            session_task = Some(spawn_session_task(
                                path.clone(),
                                Some(kind),
                                session.clone(),
                            ));
                        }
                    }
                    let md = session
                        .read()
                        .ok()
                        .and_then(|s| {
                            s.messages
                                .iter()
                                .rev()
                                .find(|m| m.role == Role::Assistant)
                                .map(|m| m.content.clone())
                        });
                    match md {
                        Some(md) if !md.trim().is_empty() => {
                            if let Err(e) = pager_render(&md, &terminal).await {
                                eprintln!("[mdterm] terminal render failed: {e:#}");
                            }
                        }
                        _ => eprintln!(
                            "[mdterm] no assistant message in the transcript yet \
                             (watching {})",
                            transcript
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "nothing — pass --transcript".into())
                        ),
                    }
                }
                None => break wait_child(&mut child).await, // proxy closed; child exiting
            },
            code = &mut child => break code.unwrap_or(1),
        }
    };

    std::process::exit(exit_code);
}

/// Forward watch_transcript events into the shared session slot.
fn spawn_session_task(
    path: PathBuf,
    kind: Option<CliKind>,
    session: Arc<RwLock<Session>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = watch_transcript(path, kind);
        while let Some(ev) = rx.recv().await {
            match ev {
                TranscriptEvent::Updated(s) => {
                    if let Ok(mut slot) = session.write() {
                        *slot = s;
                    }
                }
                TranscriptEvent::Error(e) => {
                    eprintln!("[mdterm] transcript watch error: {e}");
                }
            }
        }
    })
}

async fn wait_child(child: &mut tokio::task::JoinHandle<i32>) -> i32 {
    child.await.unwrap_or(1)
}

/// The command to wrap: explicit args after `--`, else auto-detect
/// `claude`, `kimi` or `codex` on PATH (first match wins).
fn resolve_wrap_command(cmd: &[String]) -> anyhow::Result<(String, Vec<String>)> {
    if let Some((first, rest)) = cmd.split_first() {
        return Ok((first.clone(), rest.to_vec()));
    }
    for name in ["claude", "kimi", "codex"] {
        if which(name).is_some() {
            return Ok((name.to_string(), Vec::new()));
        }
    }
    Err(anyhow!(
        "no command given and none of `claude`, `kimi`, `codex` found on PATH; \
         usage: mdterm-llm wrap -- <cmd> [args...]"
    ))
}

fn cli_kind_for_command(cmd: &str) -> CliKind {
    let base = cmd.rsplit('/').next().unwrap_or(cmd);
    if base.contains("kimi") {
        CliKind::Kimi
    } else if base.contains("codex") {
        CliKind::Codex
    } else {
        CliKind::Claude
    }
}

/// Minimal `which(1)`: find an executable named `name` on PATH.
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            Some(candidate)
        } else {
            None
        }
    })
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

// ---------------------------------------------------------------------------
// serve / render
// ---------------------------------------------------------------------------

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let (path, kind) = resolve_transcript(args.transcript, args.provider)?;
    let port = start_viewer(path.clone(), kind, args.port).await?;
    println!("mdterm viewer: http://127.0.0.1:{port}/");
    println!("watching {} (Ctrl-C to quit)", path.display());

    tokio::signal::ctrl_c().await?;
    Ok(())
}

async fn render(args: RenderArgs) -> anyhow::Result<()> {
    let markdown = if let Some(file) = &args.file {
        std::fs::read_to_string(file)
            .with_context(|| format!("reading markdown file {}", file.display()))?
    } else {
        let (path, kind) = resolve_transcript(args.transcript, args.provider)?;
        let session = parse_transcript(&path, kind)
            .with_context(|| format!("parsing transcript {}", path.display()))?;
        if args.last {
            session
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .map(|m| m.content.clone())
                .ok_or_else(|| anyhow!("no assistant message in {}", path.display()))?
        } else {
            session
                .messages
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n\n---\n\n")
        }
    };

    let opts = RenderOptions {
        width: args.width.unwrap_or_else(terminal_width),
        theme: Theme::Dark,
        math: args.math.into(),
        caps: TerminalCaps::detect(),
    };
    let ansi = render_markdown(&markdown, &opts)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(ansi.as_bytes())?;
    if !ansi.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    stdout.flush()?;
    Ok(())
}

fn caps() -> anyhow::Result<()> {
    let caps = TerminalCaps::detect();
    let graphics = match caps.graphics {
        mdterm_render::GraphicsProto::Kitty => "kitty",
        mdterm_render::GraphicsProto::Sixel => "sixel",
        mdterm_render::GraphicsProto::ITerm2 => "iterm2",
        mdterm_render::GraphicsProto::None => "none",
    };
    println!("mdterm caps: detected terminal capabilities");
    println!("  terminal program: {}", TerminalCaps::terminal_program());
    println!("  graphics protocol: {graphics}");
    println!("  truecolor: {}", if caps.truecolor { "yes" } else { "no" });
    println!("  TERM={}", std::env::var("TERM").unwrap_or_default());
    println!("  TERM_PROGRAM={}", std::env::var("TERM_PROGRAM").unwrap_or_default());
    println!("  COLORTERM={}", std::env::var("COLORTERM").unwrap_or_default());
    println!("  TMUX={}", std::env::var("TMUX").unwrap_or_default());
    Ok(())
}

// ---------------------------------------------------------------------------
// terminal-render surface (wrap hotkey)
// ---------------------------------------------------------------------------

/// Render `md` and show it on a terminal surface: tmux popup when inside
/// tmux, else `less -R`, else a direct dump. Never writes into the child
/// CLI's live screen: the pager gets the real terminal with cooked mode
/// restored (via the proxy's [`TerminalGuard`]) while it runs.
async fn pager_render(md: &str, terminal: &TerminalGuard) -> anyhow::Result<()> {
    let opts = RenderOptions {
        width: terminal_width(),
        theme: Theme::Dark,
        math: MathMode::Auto,
        caps: TerminalCaps::detect(),
    };
    let ansi = render_markdown(md, &opts)?;

    // The rendered output goes to a temp file so the pager can be spawned
    // as a plain child sharing the real terminal.
    let mut tmp = tempfile::Builder::new()
        .prefix("mdterm-render-")
        .suffix(".ansi")
        .tempfile()?;
    tmp.write_all(ansi.as_bytes())?;
    tmp.flush()?;
    let path = tmp.path().to_path_buf();

    let in_tmux = std::env::var_os("TMUX").is_some() && which("tmux").is_some();
    let less = which("less");

    if in_tmux {
        // tmux owns the popup's terminal; no raw-mode suspension needed.
        let pager = match &less {
            Some(_) => format!("less -R '{}'", path.display()),
            None => format!(
                "sh -c 'cat \"$1\"; printf \"\\n[mdterm] press Enter to close \"; read _' sh '{}'",
                path.display()
            ),
        };
        let status = tokio::process::Command::new("tmux")
            .args(["display-popup", "-E", "-w", "90%", "-h", "85%"])
            .arg(&pager)
            .status()
            .await;
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("[mdterm] tmux display-popup exited with {s}"),
            Err(e) => eprintln!("[mdterm] could not run tmux display-popup: {e}"),
        }
        return Ok(());
    }

    if let Some(less) = less {
        terminal.suspend();
        let status = tokio::process::Command::new(less)
            .arg("-R")
            .arg(&path)
            .status()
            .await;
        terminal.resume();
        if let Err(e) = status {
            eprintln!("[mdterm] could not run less: {e}");
        }
        return Ok(());
    }

    // Last resort: dump straight to stdout. Cooked mode first so newlines
    // do not stair-step; the child TUI will redraw over it on its next
    // frame.
    terminal.suspend();
    println!("\n----- mdterm: last assistant message -----");
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(ansi.as_bytes());
    let _ = stdout.flush();
    println!("----- end (the CLI will redraw) -----\n");
    drop(stdout);
    terminal.resume();
    Ok(())
}

/// Current terminal width in columns: TIOCGWINSZ on stdout, then $COLUMNS,
/// else 100.
fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
            && ws.ws_col > 0
        {
            return ws.ws_col as usize;
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .filter(|&c: &usize| c > 0)
        .unwrap_or(100)
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Resolve the transcript to watch/render and the provider to parse it
/// with. Explicit `--transcript` wins; `--provider` pins the parser (and
/// the discovery root when no transcript is given). With neither, the
/// newest transcript across all providers is used; with `--transcript`
/// alone the format is sniffed from the file's contents.
fn resolve_transcript(
    explicit: Option<PathBuf>,
    provider: Option<ProviderArg>,
) -> anyhow::Result<(PathBuf, Option<CliKind>)> {
    let kind = provider.map(CliKind::from);
    if let Some(path) = explicit {
        return Ok((path, kind));
    }
    match kind {
        Some(CliKind::Selfdefined) => Err(anyhow!(
            "--provider selfdefined has no discovery roots; pass --transcript <path>"
        )),
        Some(k) => discover_latest_session(k).map(|p| (p, Some(k))).ok_or_else(|| {
            anyhow!("no {k:?} transcript found; pass --transcript <path>")
        }),
        None => discover_latest_any()
            .map(|(p, k)| (p, Some(k)))
            .ok_or_else(|| {
                anyhow!(
                    "no transcript found under ~/.claude/projects, ~/.kimi-code/sessions, \
                     ~/.kimi/sessions or ~/.codex/sessions; pass --transcript <path>"
                )
            }),
    }
}

/// Start the file watcher + viewer server; returns the actual bound port.
async fn start_viewer(path: PathBuf, kind: Option<CliKind>, port: u16) -> anyhow::Result<u16> {
    if !path.is_file() {
        return Err(anyhow!("transcript {} does not exist", path.display()));
    }
    let rx = watch_transcript(path, kind);
    ViewerServer::start(port, rx).await
}

/// Open a URL in the system browser (`open` on macOS, `xdg-open` on Linux).
fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(opener).arg(url).spawn() {
        Ok(_) => eprintln!("[mdterm] opened {url} in browser"),
        Err(e) => eprintln!("[mdterm] could not launch `{opener}` ({e}); open {url} manually"),
    }
}
