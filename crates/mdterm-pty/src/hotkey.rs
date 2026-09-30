//! Hotkey chord matching: a tiny trie-based state machine over the stdin
//! byte stream. O(1) per byte (a byte is re-examined at most once after a
//! mismatch), zero allocation after construction.

use crate::{HotkeyConfig, ProxyEvent};

#[derive(Debug, Clone, Default)]
struct Node {
    /// (byte, child node index). Kept as a tiny linear list: a node has at
    /// most as many edges as there are configured chords.
    next: Vec<(u8, u32)>,
    /// Set on the terminal node of a configured chord.
    event: Option<ProxyEvent>,
}

fn insert(nodes: &mut Vec<Node>, chord: &[u8], event: ProxyEvent) {
    if chord.is_empty() {
        return;
    }
    let mut cur = 0u32;
    for &b in chord {
        let found = nodes[cur as usize]
            .next
            .iter()
            .find(|(cb, _)| *cb == b)
            .map(|(_, n)| *n);
        cur = match found {
            Some(n) => n,
            None => {
                let idx = nodes.len() as u32;
                nodes.push(Node::default());
                nodes[cur as usize].next.push((b, idx));
                idx
            }
        };
    }
    nodes[cur as usize].event = Some(event);
}

/// Bracketed-paste markers (DEC private mode 2004). Raw-mode children
/// (TUIs) enable bracketed paste and wrap pasted text in these sequences;
/// the child expects the wrapped bytes verbatim.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Stateful chord filter over the input byte stream.
///
/// Invariant: `pending` holds exactly the bytes of the path from the trie
/// root to `state` (i.e. the current partial match). On a full match the
/// pending bytes are discarded (consumed) and the event fires; on a mismatch
/// the pending bytes are flushed to the child verbatim and the current byte
/// is re-examined from the root, so overlapping chords (e.g. the shared
/// `0x07` prefix of the defaults) can never swallow or corrupt input.
///
/// If one configured chord is a strict prefix of another, the shorter chord
/// always wins (its event fires as soon as it completes).
///
/// # Bracketed paste (F6)
///
/// Between `\x1b[200~` and `\x1b[201~` every byte is forwarded verbatim and
/// chord matching is suspended: pasted text must never fire hotkeys (a
/// pasted "…\x07r…" is text, not a chord) nor lose bytes to the chord
/// prefix buffer. Both markers are forwarded to the child verbatim. Marker
/// detection holds back at most `marker.len() - 1` bytes (the same
/// bounded-withholding policy as chord prefixes); on a mismatch the
/// longest suffix that is still a marker prefix is retained, so a marker
/// split across arbitrary byte boundaries is always found.
pub(crate) struct ChordFilter {
    nodes: Vec<Node>,
    state: u32,
    pending: Vec<u8>,
    passthrough: bool,
    /// True between a complete PASTE_START and the next PASTE_END.
    in_paste: bool,
    /// Bytes withheld while they are a strict prefix of the current paste
    /// marker (PASTE_START when not pasting, PASTE_END when pasting).
    marker: Vec<u8>,
}

impl ChordFilter {
    pub(crate) fn new(config: &HotkeyConfig) -> Self {
        let mut nodes = vec![Node::default()];
        if config.enabled {
            insert(&mut nodes, &config.render_terminal, ProxyEvent::RenderTerminal);
            insert(&mut nodes, &config.render_browser, ProxyEvent::RenderBrowser);
        }
        let max_len = config
            .render_terminal
            .len()
            .max(config.render_browser.len());
        ChordFilter {
            // Only the root node means no chords were configured.
            passthrough: !config.enabled || nodes.len() == 1,
            nodes,
            state: 0,
            // Preallocated to the longest chord: never grows in `feed`.
            pending: Vec::with_capacity(max_len),
            in_paste: false,
            // Preallocated to a full marker: never grows in `feed`.
            marker: Vec::with_capacity(PASTE_START.len()),
        }
    }

    /// Feed one input byte. `out` receives byte slices to forward to the
    /// child verbatim and in order; `event` receives matched chord events.
    pub(crate) fn feed<O, E>(&mut self, byte: u8, out: &mut O, event: &mut E)
    where
        O: FnMut(&[u8]),
        E: FnMut(ProxyEvent),
    {
        if self.passthrough {
            out(std::slice::from_ref(&byte));
            return;
        }
        // Bracketed-paste pre-filter (F6). `marker` holds bytes that are a
        // strict prefix of the marker currently being watched for.
        let watching: &[u8] = if self.in_paste { PASTE_END } else { PASTE_START };
        self.marker.push(byte);
        if self.marker == watching {
            // Complete marker: forward verbatim, toggle paste state.
            out(&self.marker);
            self.marker.clear();
            self.in_paste = !self.in_paste;
            return;
        }
        if watching.starts_with(&self.marker) {
            return; // marker candidate: withhold for now
        }
        // Mismatch: the longest suffix of `marker` that is still a prefix
        // of the watched marker stays withheld; the rest is resolved now
        // (verbatim when pasting, through the chord matcher otherwise).
        let kept = longest_marker_prefix_suffix(&self.marker, watching);
        let n = self.marker.len() - kept;
        let mut tmp = [0u8; PASTE_START.len()];
        tmp[..n].copy_from_slice(&self.marker[..n]);
        self.marker.drain(..n);
        for &b in &tmp[..n] {
            if self.in_paste {
                out(std::slice::from_ref(&b));
            } else {
                self.feed_chord(b, out, event);
            }
        }
    }

    /// Chord matching for one byte (the original filter logic); only
    /// reached for bytes outside bracketed paste.
    fn feed_chord<O, E>(&mut self, byte: u8, out: &mut O, event: &mut E)
    where
        O: FnMut(&[u8]),
        E: FnMut(ProxyEvent),
    {
        let mut byte = Some(byte);
        while let Some(b) = byte.take() {
            let next = self.nodes[self.state as usize]
                .next
                .iter()
                .find(|(cb, _)| *cb == b)
                .map(|(_, n)| *n);
            match next {
                Some(idx) => {
                    if let Some(ev) = self.nodes[idx as usize].event {
                        // Full chord: consume it, emit the event, forward nothing.
                        self.pending.clear();
                        self.state = 0;
                        event(ev);
                    } else {
                        self.pending.push(b);
                        self.state = idx;
                    }
                }
                None => {
                    if self.state == 0 {
                        out(std::slice::from_ref(&b));
                    } else {
                        // Partial match then mismatch: flush the buffered
                        // prefix to the child, then retry this byte from root
                        // (it may itself start a new chord).
                        out(&self.pending);
                        self.pending.clear();
                        self.state = 0;
                        byte = Some(b);
                    }
                }
            }
        }
    }

    /// At input EOF, flush any withheld paste-marker bytes and any
    /// in-progress partial chord to the child.
    pub(crate) fn finish<O, E>(&mut self, out: &mut O, event: &mut E)
    where
        O: FnMut(&[u8]),
        E: FnMut(ProxyEvent),
    {
        let marker = std::mem::take(&mut self.marker);
        for &b in &marker {
            if self.in_paste {
                out(std::slice::from_ref(&b));
            } else {
                self.feed_chord(b, out, event);
            }
        }
        if !self.pending.is_empty() {
            out(&self.pending);
            self.pending.clear();
            self.state = 0;
        }
    }
}

/// Length of the longest suffix of `buf` (strictly shorter than `buf`) that
/// is a prefix of `marker`; 0 when there is none.
fn longest_marker_prefix_suffix(buf: &[u8], marker: &[u8]) -> usize {
    (1..buf.len())
        .rev()
        .find(|&k| marker.starts_with(&buf[buf.len() - k..]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sink {
        bytes: Vec<u8>,
        events: Vec<ProxyEvent>,
    }

    impl Sink {
        fn new() -> Self {
            Sink { bytes: Vec::new(), events: Vec::new() }
        }
        fn feed(&mut self, filter: &mut ChordFilter, input: &[u8]) {
            for &b in input {
                filter.feed(
                    b,
                    &mut |s| self.bytes.extend_from_slice(s),
                    &mut |e| self.events.push(e),
                );
            }
        }
        fn finish(&mut self, filter: &mut ChordFilter) {
            filter.finish(
                &mut |s| self.bytes.extend_from_slice(s),
                &mut |e| self.events.push(e),
            );
        }
    }

    #[test]
    fn full_chord_is_consumed_and_emits_event() {
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        s.feed(&mut f, &[0x07]);
        assert!(s.bytes.is_empty());
        s.feed(&mut f, b"r");
        assert_eq!(s.events, vec![ProxyEvent::RenderTerminal]);
        assert!(s.bytes.is_empty(), "chord bytes must not be forwarded");
        s.feed(&mut f, &[0x07, b'b']);
        assert_eq!(s.events, vec![ProxyEvent::RenderTerminal, ProxyEvent::RenderBrowser]);
        assert!(s.bytes.is_empty());
    }

    #[test]
    fn partial_match_then_mismatch_flushes_prefix() {
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        s.feed(&mut f, &[0x07, b'x']);
        assert_eq!(s.bytes, vec![0x07, b'x']);
        assert!(s.events.is_empty());
    }

    #[test]
    fn overlapping_prefix_is_not_swallowed() {
        // 0x07 0x07 r: first 0x07 must reach the child, second starts a chord.
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        s.feed(&mut f, &[0x07, 0x07, b'r']);
        assert_eq!(s.bytes, vec![0x07]);
        assert_eq!(s.events, vec![ProxyEvent::RenderTerminal]);
    }

    #[test]
    fn ordinary_bytes_pass_untouched() {
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        let payload: Vec<u8> = (0u8..=255).filter(|&b| b != 0x07).collect();
        s.feed(&mut f, &payload);
        assert_eq!(s.bytes, payload);
        assert!(s.events.is_empty());
        s.finish(&mut f);
        assert_eq!(s.bytes, payload);
    }

    #[test]
    fn eof_flushes_pending_partial_chord() {
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        s.feed(&mut f, b"ab");
        s.feed(&mut f, &[0x07]);
        s.finish(&mut f);
        assert_eq!(s.bytes, b"ab\x07");
        assert!(s.events.is_empty());
    }

    #[test]
    fn disabled_config_passes_everything() {
        let mut cfg = HotkeyConfig::default();
        cfg.enabled = false;
        let mut f = ChordFilter::new(&cfg);
        let mut s = Sink::new();
        s.feed(&mut f, &[0x07, b'r', 0x07, b'b']);
        assert_eq!(s.bytes, vec![0x07, b'r', 0x07, b'b']);
        assert!(s.events.is_empty());
    }

    #[test]
    fn bracketed_paste_passes_verbatim_without_firing_hotkeys() {
        // The review probe: pasted text containing the Ctrl-G r chord bytes
        // must reach the child untouched and must NOT fire RenderTerminal.
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        let input = b"\x1b[200~hello\x07rworld\x1b[201~";
        s.feed(&mut f, input);
        s.finish(&mut f);
        assert_eq!(s.bytes, input, "pasted bytes must pass verbatim");
        assert!(s.events.is_empty(), "no hotkeys inside bracketed paste");
    }

    #[test]
    fn chords_work_again_after_paste_ends() {
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        s.feed(&mut f, b"\x1b[200~\x07r\x1b[201~");
        assert!(s.events.is_empty());
        s.feed(&mut f, &[0x07, b'r']);
        assert_eq!(s.events, vec![ProxyEvent::RenderTerminal]);
        assert_eq!(s.bytes, b"\x1b[200~\x07r\x1b[201~");
    }

    #[test]
    fn paste_marker_split_across_feeds() {
        // One byte at a time: marker detection must not lose or duplicate.
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        let input = b"pre\x1b[200~a\x07bb\x1b[201~post";
        for &b in input {
            s.feed(&mut f, &[b]);
        }
        s.finish(&mut f);
        assert_eq!(s.bytes, input);
        assert!(s.events.is_empty());
    }

    #[test]
    fn partial_paste_start_then_mismatch_is_not_swallowed() {
        // "\x1b[20" is a marker prefix; the '9' breaks it. Everything must
        // come out verbatim and chords must still match afterwards.
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        s.feed(&mut f, b"\x1b[2099");
        assert_eq!(s.bytes, b"\x1b[2099");
        s.feed(&mut f, &[0x07, b'r']);
        assert_eq!(s.events, vec![ProxyEvent::RenderTerminal]);
    }

    #[test]
    fn paste_end_marker_lookalike_inside_paste() {
        // Inside a paste, "\x1b[200~" and near-misses of the end marker are
        // just pasted bytes.
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        let input = b"\x1b[200~a\x1b[200~b\x1b[201xc\x1b[201~";
        s.feed(&mut f, input);
        s.finish(&mut f);
        assert_eq!(s.bytes, input);
        assert!(s.events.is_empty());
        assert!(!f.in_paste, "the real end marker closes the paste");
    }

    #[test]
    fn eof_flushes_partial_marker_bytes() {
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let mut s = Sink::new();
        s.feed(&mut f, b"ab\x1b[20");
        assert_eq!(s.bytes, b"ab");
        s.finish(&mut f);
        assert_eq!(s.bytes, b"ab\x1b[20");
        assert!(s.events.is_empty());
    }

    #[test]
    fn no_allocation_in_hot_path() {
        let mut f = ChordFilter::new(&HotkeyConfig::default());
        let cap = f.pending.capacity();
        let mut s = Sink::new();
        for _ in 0..1000 {
            s.feed(&mut f, &[0x07, b'x']);
        }
        assert_eq!(f.pending.capacity(), cap, "pending buffer must not grow");
    }
}
