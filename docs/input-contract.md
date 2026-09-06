# Supported Input Contract

Lowdown 0.1.0 reads exactly one family: local Codex rollout JSONL. A session
starts with a `session_meta` record containing `payload.cwd`; subsequent
records have RFC 3339 timestamps and the observed Codex payload shapes.
This is not a generic chat-export reader.

## Watch

Progress rows come from `event_msg.agent_message` records other than
`phase="final_answer"`. Final answers are kept separately, from that phase or
`event_msg.task_complete.last_agent_message`. Response-item copies are not
added as duplicate progress rows. Reasoning and tool output are not feed rows.

Watch scans backward in 64 KiB blocks until it finds the requested number of
updates (30 by default), with an additional context window (64 records by
default). It need not parse hours of history before painting. Sparse logs may
still require a large scan to find enough commentary. An old final answer
outside the scanned window is not loaded until history reaches it.

Malformed scanned records are skipped; `recent-updates --format json` reports
their count and exact byte offsets for valid messages. An unfinished append is
ignored until it becomes valid JSON. Logs must be UTF-8 JSONL, not a JSON array.

Refresh and older-history reads, disk-cache hydration, and model summaries run
on separate workers. Scrolling to the oldest loaded row requests 30 more.
Unchanged files are not reparsed. When a file changes, the requested tail is
rescanned; this is not yet a persistent incremental file cursor.
The session list is discovered at startup; restart to discover a new session.

The original selected text stays intact. The latest-answer reader preserves
the answer's prose, while shortening absolute file references to readable
labels. It shows the first sentence by default; `o` expands it and Page Up /
Page Down scroll. The feed crops to one line unless `p` enables two-line wrap.
It uses the terminal's native background and handles resize.

## Discovery

With no source flags, the current directory is the project root. Lowdown does
not walk parents. It uses `$CODEX_HOME`, or `~/.codex`, and:

1. Reads recent activity from `session_index.jsonl` and rollout-file modification times.
2. Walks filenames under `sessions/` and ranks candidates by that activity.
3. Inspects metadata for at most 200 recent candidates across projects.
4. Matches a canonical session cwd equal to or below the chosen project root.

The default lookback is seven days of activity, not session creation time.
A weeks-old session with fresh activity can match. The global candidate cap can
exclude quiet projects on busy hosts; pin a file with `--session` in that case.
Zero-commentary helper sessions are omitted from the watch carousel when real
commentary-bearing sessions exist. The latest result is not proof that an
agent process is currently running.

Explicit `--session` (alias `--session-dir`) accepts a file of any name or a
directory containing `rollout-*.jsonl`. A directory selects the lexically last
filename, not the most recently modified file. Explicit watch stays pinned.

## Text Digest

`digest` parses the full file, unlike watch. It extracts user objectives,
assistant commentary/finals, task start/completion, command/patch events, and
observed function/custom tool result records (not the call records themselves).
It groups stretches into a
chaptered timeline and includes raw line references.

Headline, status, caveat and timeline extraction are heuristic, not verified
execution truth or a model synthesis of the whole task. Recent progress rows
can use the same Codex summarizer as watch. Final answers are not summarized
by that model. Read source references before acting on ambiguous status.

`digest --follow` polls and prints a new full digest when the selected file
changes; it can spend time parsing and summarizing. Use watch for the responsive
interactive path. Watch and digest have separate optimized parsers for the same
rollout family. Watch requires a TTY rather than silently changing output modes.

## Model And Platform Boundary

Model access is optional and uses only the local Codex CLI. The command,
configuration, caching and fallback behavior are documented in the
[quickstart](quickstart.md). No OpenAI API-key adapter, legacy `GODSPEED_*`
variables, or adjustable batch-size variable is implemented in the native app.

Local validation is on macOS Apple Silicon. The build workflow targets macOS
Intel, Linux x86-64 and Windows x86-64 too, but those hosted runs and terminal
smokes must pass before advertising verified binaries for those platforms.
See [releasing](ops/releasing.md).

Not supported: Claude or Gemini transcripts, other Codex transcript families,
HTML exports, multi-session stitching, transcript edits, remote dashboards,
daemons, or orchestration.
