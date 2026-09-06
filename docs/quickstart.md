# Lowdown Quickstart

Lowdown turns Codex progress messages into a scannable live feed. Original
messages and final answers stay available in separate readers.

## Install And Run

From this source checkout, with Rust and Cargo installed:

```sh
cargo install --locked --path . --bin lowdown
```

Put Cargo's bin directory on PATH (normally `~/.cargo/bin` on Unix or
`%USERPROFILE%\.cargo\bin` on Windows). Then enter the project you want to
follow and run `lowdown`. No Python runtime or system-package override is needed.

Prebuilt release archives are being prepared. See [installation](install.md)
for binary installation and [releasing](ops/releasing.md) for the verification
and publication boundary.

## Controls

| Key | Action |
| --- | --- |
| j / k or arrows | Move through updates |
| Page Up / Page Down | Page updates, or scroll the expanded reader |
| h / l | Previous / next matching session |
| i | Expand or collapse the original update |
| o | Expand or collapse the latest answer |
| p | Toggle cropped rows / two-line wrapping |
| q or Ctrl+C | Quit |

Updates take most of the screen. The answer starts as a first-sentence preview.
Scrolling to the oldest loaded update requests 30 more in the background.
Returning to the newest update resumes following new messages. `SCROLL` means
you are browsing older updates, not that the agent needs review.

Switching sessions restores recently viewed updates and summaries immediately.
For other sessions, cached summaries load on a separate worker, independently
of ongoing model calls. Raw text stays usable while disk reads finish; only
missing summaries need model work. Non-regular cache files are ignored.

## Other Commands

```sh
lowdown watch --session /absolute/path/to/rollout.jsonl
lowdown watch --repo-root /absolute/path/to/project --lookback-days 30
lowdown digest --pretty
lowdown digest --session /absolute/path/to/rollout.jsonl --pretty
lowdown digest --follow --pretty
lowdown recent-updates --session /absolute/path/to/rollout.jsonl --format json
lowdown --version
```

`--session-dir` is an alias for `--session`; either accepts a file or a directory.
Watch needs an interactive terminal. Use `digest --pretty` for redirected output.

## Summaries And Privacy

The feed paints raw text first. When Codex CLI is installed and signed in,
Lowdown sends batches of at most eight progress-message texts to `codex exec`.
It does not send the full transcript, tool results, or final answers. Ordinary
messages are sent in full; exceptionally large messages are capped at 16,000
terminal cells. Model summaries can be wrong: expand the original with `i`.

The default is `gpt-5.6-luna` with `none` reasoning. Each batch has a 45-second
timeout. Lowdown ignores Codex user configuration and instruction files for
this isolated read-only job, but uses the existing Codex authentication.
It requires a Codex CLI supporting `exec --ignore-user-config --ignore-rules`,
`--ephemeral`, and `--output-schema`. Older CLIs leave the feed in fallback mode.

Set `LOWDOWN_SUMMARY_PROVIDER=fallback` to disable model calls. Watch then keeps
the original text, not a fabricated model summary. Existing cached summaries
may still be displayed. `RAW`, `HYDRATING`, `CODEX`, `MIXED`, and `FALLBACK`
describe summary availability, not whether the agent's work succeeded.

| Environment variable | Purpose |
| --- | --- |
| CODEX_HOME | Codex session and authentication directory |
| LOWDOWN_CACHE_DIR | Override Lowdown's local summary cache directory |
| LOWDOWN_TIMEZONE | `local` (default), `utc`, or `session` |
| LOWDOWN_SUMMARY_PROVIDER | `fallback` disables model calls; otherwise Codex CLI |
| LOWDOWN_SUMMARY_CODEX_MODEL | Override `gpt-5.6-luna` |
| LOWDOWN_SUMMARY_CODEX_REASONING_EFFORT | Override `none` |
| LOWDOWN_SUMMARY_CODEX_TIMEOUT | Per-batch seconds, clamped to 1-300 |

Cache defaults to `~/.cache/lowdown/rust` on macOS/Linux, respecting
`XDG_CACHE_HOME`. Windows uses `LOCALAPPDATA` or `APPDATA` when available.
Cache files contain summaries and the source path. Delete the Lowdown cache
directory to clear them. Source sessions and supervised projects are never edited.

Only local Codex rollout JSONL is supported. See the
[input contract](input-contract.md) for discovery limits and unsupported inputs.
