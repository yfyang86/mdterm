# mdterm — usage hints & findings

Findings from 2026-09-30, verified against local source (`crates/`) and
`mdterm 0.1.0` installed at `~/.local/bin/mdterm`.

**Update (same day): the kimi/codex/selfdefined provider support described
in "Gap" below is now implemented in mdterm-core and installed.** Native
parsers for all kimi formats, codex rollouts and a canonical self-defined
format; `--provider <claude|codex|kimi|selfdefined>` on wrap/serve/render;
`--transcript` alone now sniffs the format; bare discovery picks the newest
transcript across all providers (no more "Claude always wins"). All 20 core
tests pass; verified live against real kimi/codex sessions.

## Checking render output in real time (`mdterm wrap -- kimi`)

`wrap` renders to surfaces *outside* the wrapped CLI's TUI:

- **Live browser viewer** — at startup, wrap prints
  `[mdterm] viewer: http://127.0.0.1:<port>`. Open it; the page live-updates
  via SSE on every transcript change (header shows `live` / `reconnecting`).
- **Hotkey chords** (disable with `--hotkeys off`; press Ctrl-G, release,
  then the key):
  - `Ctrl-G r` — render last assistant message in terminal: tmux popup if
    `$TMUX` set → `less -R` → direct stdout dump (fallback chain).
  - `Ctrl-G b` — open/focus the browser viewer.
- **From a second terminal**: `mdterm serve` (live viewer, prints its own
  port), `mdterm render --last` / `--all` (one-shot ANSI to stdout).
- Debug logging: `RUST_LOG=mdterm=debug`.

## Pitfall (historical): `render` / `serve` showed Claude, not kimi

Pre-fix, default discovery was "latest Claude, then Kimi", so any
`~/.claude/projects/**/*.jsonl` won. **Fixed:** discovery now picks the
newest transcript across all providers; `--provider` or `--transcript`
pins explicitly.

## Gap (now fixed): kimi transcript support was a stub in v0.1.0

The original findings, for the record:

- **Parser was Claude-only** — `parse.rs` implemented only
  `parse_claude_jsonl`; `watch.rs` used it best-effort for both kinds.
- Kimi lines have no top-level `"type": "assistant"` field, so every line
  was skipped → `Error: no assistant message in <path>`.
- **Discovery roots didn't match real kimi paths** — probed
  `~/.kimi/projects/**` and `~/.config/kimi/**/sessions`, while Kimi Code
  CLI 2.1.1 writes to `~/.kimi/sessions/<hash>/<uuid>/{context,wire}.jsonl`
  and `~/.kimi-code/sessions/<wd_hash>/<session_id>/agents/main/wire.jsonl`.

**Fix implemented** (mdterm-core): native parsers for kimi-code wire
(protocol ≥ 1.5, `agent.message.appended`), legacy wire
(`TurnBegin`/`ContentPart`) and `context.jsonl`; codex rollouts
(`response_item`, `event_msg` fallback); and the canonical self-defined
format. `parse_transcript(path, Option<CliKind>)` sniffs the format when
no provider is given. Discovery roots extended; `discover_latest_any()`
picks the newest across providers. Session id for kimi resolves to the
session directory name.

## Getting the current kimi session id / transcript path

```bash
kimi session list --limit 1 --json          # newest session for cwd
kimi session list --limit 1 --json | jq -r '.[0].id'
kimi session list --limit 1 --json | jq -r '.[0].sessionDir + "/agents/main/wire.jsonl"'
```

- "Current" is resolved by working directory; use `--cwd <path>` from
  elsewhere, `--all` to search every workspace, `--archived` to include
  archived.
- Filesystem fallback (id = directory basename):
  `ls -td ~/.kimi-code/sessions/*/*/ | head -1`
  (old layout: `ls -td ~/.kimi/sessions/*/*/ | head -1`)
- Example (the session where these findings were made):
  `session_e3636907-5b05-46d9-8a73-882f8ff61909` under
  `~/.kimi-code/sessions/wd_yifanyang_22aec0c79573/`.

## Resuming a wrapped session (`mdterm wrap -- kimi -S …`)

Always resume a wrap-created session **through the wrapper** and **from
the same working directory** it was started in:

```bash
mdterm wrap -- kimi -S <session-id>     # or: mdterm wrap -- kimi -c
```

- Kimi stores sessions per working directory
  (`~/.kimi-code/sessions/wd_<hash>/`). Resuming from a different folder
  fails with a folder-mismatch error (`-c` likewise only sees the current
  directory's sessions).
- Flag note: kimi 2.1.1 has no `-r`; the resume flags are
  `-S, --session [id]` (explicit id or interactive picker) and
  `-c, --continue` (previous session for this cwd).
- mdterm side: after the resume starts, the session's `wire.jsonl` becomes
  the newest kimi transcript, so the wrap viewer's discovery follows it
  automatically once kimi writes the first event. To be certain from the
  start, pin it: `--transcript <sessionDir>/agents/main/wire.jsonl`.

## Providers (post-fix)

`wrap` / `serve` / `render` all take `--provider <claude|codex|kimi|
selfdefined>`:

- `mdterm serve --transcript <file>` — provider sniffed from contents;
  all kimi shapes, codex rollouts and selfdefined files work directly.
- `mdterm serve` / `mdterm render` — newest transcript across all
  provider roots (`~/.claude/projects`, `~/.kimi-code/sessions`,
  `~/.kimi/sessions`, `~/.codex/sessions`).
- `mdterm serve --provider kimi` — restrict discovery to kimi roots;
  `--provider selfdefined` requires `--transcript` (no roots).

Self-defined format (canonical mdterm JSONL):

```json
{"type":"metadata","sessionId":"my-session"}
{"type":"message","role":"user","content":"markdown","timestamp":"..."}
{"type":"message","role":"assistant","content":[{"type":"text","text":"..."}],"model":"..."}
```

Roles: user/assistant/system/tool; content: string or text-block array;
unknown lines skipped. Full spec in README "Self-defined transcript
format".

## Reinstall note (macOS arm64)

Overwriting a running-signed binary in place (`cp` over
`~/.local/bin/mdterm`) gets it `Killed: 9` by the kernel's signature
cache. `rm` the destination first, then `cp`.

## Switching sessions in `mdterm serve`

Not possible at runtime: the server exposes only GET routes (`/`, `/events`,
`/api/session`, `/assets/*` — see `mdterm-viewer/src/lib.rs:83-88`) and the
watched transcript is fixed at startup. Options:

- Restart with the other transcript:
  `mdterm serve --transcript <path> [--port N]`
- Pin `--port N` so the URL stays stable across restarts.
- Run several `serve` instances on different ports to watch sessions in
  parallel.
- Under `mdterm wrap -- <cli>`, the viewer follows the wrapped CLI's
  discovered session automatically — wrap is the per-session workflow;
  `serve` is the standalone fixed-transcript one.

## Workaround (obsolete): live-view a kimi session on the pre-fix v0.1.0

**Not needed anymore** — `mdterm serve --transcript <kimi wire.jsonl>`
parses natively now. Kept for reference: the trick was converting kimi
wire events into Claude-shaped lines with jq and serving the converted
file. The sniff/dispatch layer does this properly in-process now.

<details><summary>original recipe</summary>

```bash
# run from the session's project dir (or: kimi session list --cwd <dir>)
WIRE="$(kimi session list --limit 1 --json | jq -r '.[0].sessionDir')/agents/main/wire.jsonl"
OUT=/tmp/kimi-live.jsonl
: > "$OUT"   # start empty; tail -n +1 replays history — pre-filling duplicates it

FILTER='select(.type=="agent.message.appended") | .message.message as $m
  | select(($m.content|type)=="array")
  | ([$m.content[]? | select(.type=="text") | .text] | join("\n\n")) as $t
  | select($t != "") | select($m.role=="user" or $m.role=="assistant")
  | {type:$m.role, timestamp:((.time/1000)|todate),
     message:{role:$m.role, content:[{type:"text", text:$t}]}}'

tail -n +1 -F "$WIRE" | jq --unbuffered -c "$FILTER" >> "$OUT" &   # converter
mdterm serve --port 7777 --transcript "$OUT"    # live: http://127.0.0.1:7777/
```

- Shows user prompts + assistant text; think/tool events are filtered out.
- Same file works for one-shots: `mdterm render --last --transcript "$OUT"`.
- Stop with Ctrl-C on serve and `kill %1` for the converter.
</details>
