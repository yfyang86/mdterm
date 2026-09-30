//! Transcript file watching with `notify`.
//!
//! Watches the transcript's parent directory (so atomic-replace writes by
//! the upstream CLI are observed too) and, on each change touching the
//! transcript, re-parses and emits [`TranscriptEvent::Updated`]. An initial
//! `Updated` event with the current contents is emitted right away so
//! consumers do not need a separate bootstrap read.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::{parse_claude_jsonl, CliKind, TranscriptEvent};

/// Coalescing window for the watcher debounce (F9): after the first
/// relevant notify event, further events within this window are drained
/// and produce a single re-parse + broadcast.
const DEBOUNCE_MS: u64 = 200;

/// Watch a transcript file (or its parent dir) with `notify`; on each
/// change, re-parse and emit `TranscriptEvent::Updated`.
pub fn watch_transcript(path: PathBuf, kind: CliKind) -> mpsc::Receiver<TranscriptEvent> {
    let (tx, rx) = mpsc::channel(64);

    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    std::thread::spawn(move || {
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = notify_tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                let _ = tx.blocking_send(TranscriptEvent::Error(format!(
                    "failed to create file watcher: {e}"
                )));
                return;
            }
        };

        use notify::Watcher;
        if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
            let _ = tx.blocking_send(TranscriptEvent::Error(format!(
                "failed to watch {}: {e}",
                dir.display()
            )));
            return;
        }

        // Emit the current state immediately (best-effort bootstrap).
        emit(&path, kind, &tx);

        // Debounce (F9): a burst of writes (e.g. the CLI appending many
        // lines at once) produces a burst of notify events; re-parsing and
        // broadcasting for each of them floods the (bounded) downstream
        // channel and wastes full parses. Instead, the first relevant
        // event opens a fixed coalescing window: events keep being drained
        // (so the upstream queue stays bounded) and ONE re-parse + emit
        // happens when the window closes, observing the final state.
        while let Ok(res) = notify_rx.recv() {
            match res {
                Ok(event) => {
                    // Only content-changing events are relevant. In
                    // particular, Access(Open/Read) events must be ignored:
                    // our own re-parse opens the file and would otherwise
                    // retrigger itself in a hot loop.
                    if !is_content_change(&event.kind) {
                        continue;
                    }
                    let touches = event.paths.iter().any(|p| same_file(p, &path));
                    if !touches {
                        continue;
                    }
                    // Drain events until the window elapses.
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(DEBOUNCE_MS);
                    loop {
                        let now = std::time::Instant::now();
                        if now >= deadline {
                            break;
                        }
                        match notify_rx.recv_timeout(deadline - now) {
                            Ok(Ok(_)) | Ok(Err(_)) => {} // coalesced; errors are non-fatal
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                emit(&path, kind, &tx);
                                return;
                            }
                        }
                        if tx.is_closed() {
                            return;
                        }
                    }
                    emit(&path, kind, &tx);
                }
                Err(e) => {
                    if tx
                        .blocking_send(TranscriptEvent::Error(format!("watch error: {e}")))
                        .is_err()
                    {
                        return; // receiver dropped
                    }
                }
            }
            if tx.is_closed() {
                return;
            }
        }
    });

    rx
}

/// Does this event kind indicate the file's content (or existence) changed?
fn is_content_change(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode, EventKind, ModifyKind};
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) => true,
        EventKind::Modify(ModifyKind::Metadata(_)) => false, // chmod etc.
        EventKind::Modify(_) => true,                        // data writes, renames
        // Some writers produce only Close(Write) rather than Modify events.
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false, // open/read — would self-retrigger
        _ => false,
    }
}

/// Paths may differ syntactically (e.g. `./x` vs `x`); compare after a
/// best-effort lexical match, falling back to file-name equality.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    a == b || a.ends_with(b) || b.ends_with(a) || a.file_name() == b.file_name()
}

fn emit(path: &std::path::Path, kind: CliKind, tx: &mpsc::Sender<TranscriptEvent>) {
    // Kimi transcripts are not yet format-verified; Claude's JSONL parser is
    // used best-effort for both (unknown lines are skipped, never fatal).
    let _ = kind;
    match parse_claude_jsonl(path) {
        Ok(session) => {
            let _ = tx.blocking_send(TranscriptEvent::Updated(session));
        }
        Err(e) => {
            let _ = tx.blocking_send(TranscriptEvent::Error(format!(
                "parse error for {}: {e:#}",
                path.display()
            )));
        }
    }
}
