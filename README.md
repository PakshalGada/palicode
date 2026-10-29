# palicode

Minimal, shortcut-driven terminal coding agent in Rust. Provider-agnostic
(OpenCode Console + local Ollama), tool-driven, single binary.

**Status:** Phase 0 (bootstrap), Phase 1 (providers + tools), Phase 2 (planner,
approvals, permissions, subagents), Phase 3 (TUI), and Phase 4 (polish +
sessions) are complete.

## Quick start

```bash
cargo build --release

# Launch the TUI (default when no subcommand is given)
./target/release/palicode
./target/release/palicode tui

# Pin provider/model and approval mode
palicode tui --provider ollama --model qwen3.5:9b --mode suggest

# Headless / scriptable
palicode run "Find where the config is loaded and explain it"
echo "Run the tests and fix failures" | palicode run
palicode plan "Add a permissions smoke test"
palicode run --plan --yes "Add a permissions smoke test"

# Discovery
palicode models
palicode tools
palicode config            # effective config
palicode config --init     # write a commented example config
```

**Build location:** this machine's `~/.bashrc` exports
`CARGO_TARGET_DIR=$HOME/.local/tauri-libs/cargo-target`, so `cargo` writes there
instead of `./target`. A local `target` symlink makes the documented
`./target/release/palicode` path work. To force a genuine repo-local build,
remove the symlink first:

```bash
rm target
CARGO_TARGET_DIR=target cargo build --release
```

## TUI

```
┌ Chat ───────────────────────────────┬ Plan ──────────────┐
│ streaming transcript, tool calls    │ editable checklist │
│                                     ├ Diff ──────────────┤
│                                     │ unified / split    │
│                                     ├ Agents ────────────┤
│ Message [Enter send · Esc normal]   │ live subagents     │
└─────────────────────────────────────┴────────────────────┘
 status: focus · model · mode · tokens · plan · cwd
```

The TUI starts in **insert mode** so you can type immediately. Press `Esc` for
**normal mode**, where single-key shortcuts work. `Tab`/`Shift+Tab` cycles panes;
`Ctrl+K` opens the command palette from anywhere; `F1` opens help; `F2` opens
settings.

### Keybindings

| Key | Action |
|---|---|
| `Ctrl+K` | Command palette |
| `Tab` / `Shift+Tab` | Cycle focus |
| `Esc` | Insert → normal; normal → chat / clear input |
| `i` / `Enter` | Type in chat (normal mode) |
| `Enter` | Send message / execute plan |
| `j` / `k` (or arrows) | Navigate / scroll |
| `p` `d` `t` `c` | Focus plan / diff / tool log / chat |
| `s` | Session list |
| `n` | New session (normal mode, except in Plan where it selects none) |
| `m` | Model switcher |
| `?` / `F1` | Help |
| `F2` | Settings (OpenCode API key) |
| `Ctrl+C` | Quit |

**Plan pane:** `space` approve/toggle a step, `a`/`n` select all/none, `e` edit,
`r` reorder mode (`j`/`k` move, Enter/Esc done), `d` delete, `Enter` execute.
Use `/plan <task>` in chat or palette → “Plan a task…” to create one.

**Diff pane:** `j`/`k` hunks, `h`/`l` files, `a`/`A` accept hunk/file,
`r`/`R` reject hunk/file (reverts the file), `u` unified/split, `←`/`→`
horizontal scroll. Diffs are syntax-highlighted with syntect.

**Tool log:** `t` opens the collapsible tool-call log (`j`/`k`, Enter expand).

**Agents:** the sidebar shows live read-only subagent status. `Tab` to the pane
and use `j`/`k` to select.

**Mouse:** click a pane to focus it; the wheel scrolls the chat, plan, diff, or
agents pane under the pointer. Clicking the chat input enters insert mode. Hold
Shift while dragging to select terminal text (mouse capture is enabled).

### Sessions

`s` opens the session list; `n` creates a new in-memory session. Enter switches,
`d` deletes an inactive session, and Esc closes. Each session keeps its own chat,
tool log, plan, diff, and subagent state, and the agent restores that session's
conversation history when you switch back. Durable cross-restart resume arrives
in Phase 5 with SQLite checkpoints.

### Themes

The default is **black and white** (`mono-dark`): black background, white/gray
text, grayscale syntax highlighting and diff shading. `Ctrl+K → Theme: toggle
dark/light` switches to `mono-light` (white background, black/gray text). Both
are strictly monochrome—no hue anywhere.

### Polish

- Streaming shows a live cursor, elapsed turn time, token counts, and respects
  manual scroll (it does not yank you back to the bottom).
- Errors are deduplicated in the transcript, mirrored to the status bar/toast,
  and tool failures include a reflection hint for the model.
- Help shows the effective config/credentials/sandbox paths, and the TUI warns
  when config is missing.
- UI redraws are dirty-gated and batched; diff syntax highlighting is cached,
  so large diffs and fast streams stay responsive.

## OpenCode API key

OpenCode needs a Console API key for paid models. In the TUI, press `F2` (or run
`/settings`, or `Ctrl+K → Settings: OpenCode API key…`), paste the key, and press
Enter. After saving, switch with `Ctrl+K → Switch provider: opencode`.

Stored keys live outside `config.toml` in
`~/.config/palicode/credentials.toml`, written with user-only permissions (mode
`0600` on Unix). `palicode config` never prints them. `OPENCODE_API_KEY` takes
precedence when set; `Ctrl+D` in the settings screen removes a stored key.
`PALICODE_CREDENTIALS` can point at a different credentials file.

The file is plaintext protected by filesystem permissions, not encrypted; this is
the same practical model as most CLI tools. Use the environment variable or a
secret manager if you need stronger guarantees.

## Plan -> review -> approve

In the REPL, `/plan <task>` generates a structured checklist you can edit,
reorder, select, or delete before execution. `palicode plan` prints a plan;
`palicode run --plan` prints one and requires `--yes` (or `--mode full-auto`) to
execute. In the TUI, plan review happens in the Plan pane.

Mutating tools are blocked until a plan step is approved. Outside plans, the
approval mode decides.

## Approval modes

| Mode | Behaviour |
|---|---|
| `suggest` (default) | Ask before every mutating tool |
| `auto-edit` | File writes/edits allowed; shell and other mutations ask |
| `full-auto` | All tools allowed, except deny rules and built-in safety checks |

Set with `--mode`, `--yes` (full-auto), `/mode <mode>` in the REPL, or
`general.approval_mode` in config. The TUI shows an approval modal with
`y` (once), `a` (remember this tool), `n`/`Esc` (deny).

Deny rules can never be bypassed by full-auto. Built-in safety checks refuse
root filesystem deletion, `mkfs`, raw device writes, power commands, and edits
to `.git` metadata.

## Permissions

Rules use globs for the tool name and an optional subject (shell command, path,
or task prompt). A matching deny always wins; otherwise the first matching
allow/ask rule applies.

```toml
[permissions]
[[permissions.rules]]
tool = "run_shell"
pattern = "git push*"
action = "ask"        # allow | ask | deny

[[permissions.rules]]
tool = "write_file"
pattern = "**/.env*"
action = "deny"
```

## Providers

Any OpenAI-compatible `/chat/completions` endpoint works.

| Provider | Base URL | Credential |
|---|---|---|
| OpenCode Console | `https://opencode.ai/inference/openai/v1` | `OPENCODE_API_KEY` |
| Ollama | `http://localhost:11434/v1` | none |

Environment variables:

- `OPENCODE_API_KEY` - enables the OpenCode provider.
- `OLLAMA_HOST` - e.g. `127.0.0.1:11434`; `/v1` is appended automatically.
- `PALICODE_PROVIDER`, `PALICODE_MODEL` - default selection.
- `PALICODE_LOG` - `error|warn|info|debug|trace` (or use `-v` / `-vv`).
- `PALICODE_CONFIG` - alternate config path.
- `PALICODE_CREDENTIALS` - alternate credentials file (default: `~/.config/palicode/credentials.toml`).
- `PALICODE_SANDBOX` - `true|false`.
- `PALICODE_SHOW_REASONING=1` - show model thinking deltas in the REPL.

Config lives at `~/.config/palicode/config.toml`; run `palicode config --init`
to create it. Missing config files fall back to built-in defaults.

## Tools

| Tool | Purpose |
|---|---|
| `read_file` | Line-numbered reads with offset/limit |
| `write_file` | Create/overwrite, creates parent dirs |
| `edit_file` | Exact unique string replacement, returns a diff |
| `apply_patch` | Unified diff: multi-file, multi-hunk, create/delete |
| `list_dir` | Sandboxed recursive listing |
| `run_shell` | Non-interactive shell, cwd, timeout, process-group kill |
| `grep` | ripgrep JSON when available, built-in fallback |
| `glob` | .gitignore-aware glob matching |
| `git_status` | Porcelain status |
| `git_diff` | Unified diff / diffstat |
| `task` | Read-only research subagent with its own context |

All filesystem paths are resolved relative to the working directory and
confined to the sandbox root (`tools.path_sandbox = true` by default).
Symlink escapes are rejected. Tool output is truncated to keep context small.

Subagents are intentionally read-only in Phase 2/3: they get read/search/git
tools but cannot edit files, run shell, or spawn another subagent. Parallel
dispatch arrives in Phase 5.

## Architecture

- `agent/` - turn loop (`run_turn`), plan model + JSON planner, shared plan
  executor, subagent runner
- `approval/` - permission engine, glob rules, built-in safety denies, approvers
- `event/` - `tokio::sync::broadcast` event bus (`Agent`, `Plan`, `PlanStep`,
  `Subagent`, `Status`, `Error`)
- `provider/` - OpenAI-compatible streaming, SSE parser, tool-call assembly
- `tools/` - sandboxed tool implementations + registry
- `credentials` - user-only (0600) provider key storage outside config.toml
- `ui/` - ratatui app, chat/plan/diff/agent panes, syntect diff highlighting,
  command palette, model switcher, themes, and the agent worker
- `cli/` - REPL, headless run, plan review, config/discovery commands

The TUI's agent worker owns the session; the UI communicates with it over
channels and receives all streaming/tool/plan/subagent events. Approval
requests are oneshot round-trips, so the worker pauses until the modal answers.

## REPL commands

`/plan <task>`, `/mode [mode]`, `/help`, `/models`, `/model <id>`,
`/provider [name]`, `/tools`, `/system [prompt]`, `/clear`, `/cwd`,
`/history`, `/quit`.

## Verification

```bash
cargo test          # 118 tests: providers, SSE, plans, permissions, diff revert, TUI, fs, search, shell, git, agent
cargo clippy --all-targets -- -D warnings
```

See `palicode_master_plan.md` for the full roadmap. Next: Phase 5 (SQLite
checkpoints, durable resume, task list/switch, context compaction, handoff,
verified completion, task DAG + parallel subagents).
