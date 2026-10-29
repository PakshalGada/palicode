# palicode

Minimal, shortcut-driven terminal coding agent in Rust. Provider-agnostic
(OpenCode Console + local Ollama), tool-driven, single binary.

**Status:** Phases 0–5 are complete, plus project-scoped workspaces and a global
CLI install: providers/tools, planner + approvals, TUI, durable SQLite state,
crash-safe resume, compaction, handoff, verification, and parallel task DAGs.

## Quick start

```bash
# Install the global command (builds release and copies to ~/.local/bin)
./install.sh   # re-run after updating palicode

# From any project directory:
cd ~/projects/my-app
palicode

# Or run the repo binary directly
./target/release/palicode tui

# Pin provider/model and approval mode
palicode tui --provider ollama --model qwen3.5:9b --mode suggest

# Headless / scriptable
palicode run "Find where the config is loaded and explain it"
echo "Run the tests and fix failures" | palicode run
palicode plan "Add a permissions smoke test"
palicode run --plan --yes "Add a permissions smoke test"

# Durable sessions
palicode sessions                 # list persisted sessions
palicode resume                   # resume latest interrupted/idle session
palicode resume <session-id>

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
┌ Project ──────┬ Chat / Diff ─────────────────────────────┬ Plan (if any) ────┐
│ ~/…/my-app    │                                        │ editable checklist │
│ Sessions · 2 +│               palicode                 ├ Agents ────────────┤
│ * fix auth…   │                                        │ live subagents     │
│   add tests   │          ┌ Message ────────────────┐    │                    │
│               │          │ Ask palicode…           │    │                    │
│               │          └─────────────────────────┘    │                    │
│               │          INSERT · suggest · ollama/…   │                    │
│               │          0 tok · Ctrl+K commands       │                    │
└───────────────┴────────────────────────────────────────┴────────────────────┘
 status: focus · session · plan · cwd
```

The left panel is the **project folder + previous sessions for that project**.
It is intentionally slim; hover a session (or focus the sidebar) to marquee-scroll
long titles. The `+` next to “Sessions” creates a new session. The center starts
with a large **palicode** wordmark and a centered textbox—no app description.
Below the textbox, left-aligned, are the current chat mode, approval mode,
provider/model, token total, and `Ctrl+K commands`. Once a conversation starts,
the center becomes the streaming chat. Below every assistant answer there is a
dim metrics line: `3.2s · 41.5 tok/s · provider/model` (a leading `~` marks
estimated token counts when the provider does not report usage). When the model
changes files, the **diff takes over the main panel** (`c` returns to chat). The
right panel shows the **plan if one exists**, with the **agents** sidebar below.

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
| `p` | Focus plan (when one exists) |
| `d` | Show diff in the main panel (Chat/Diff/Agents; deletes in Plan/Sessions) |
| `c` | Return to chat |
| `t` | Tool log overlay |
| `s` | Focus the project/session sidebar |
| `n` | New session (normal mode, except in Plan where it selects none) |
| `T` | Task list (persisted plan tasks) |
| `m` | Model switcher |
| `?` / `F1` | Help |
| `F2` | Settings (OpenCode API key) |
| `Ctrl+C` | Quit |

**Plan pane:** `space` approve/toggle a step, `a`/`n` select all/none, `e` edit,
`r` reorder mode (`j`/`k` move, Enter/Esc done), `d` delete, `Enter` execute.
Use `/plan <task>` in chat or palette → “Plan a task…” to create one.

**Sessions sidebar:** `s` focuses it, `j`/`k` selects, `Enter` switches, `n`
creates, `d` deletes an inactive session, and the `+` button (mouse) creates one
too. Only sessions for the current project are shown. Hovering a row marquee-
scrolls the full title; switching to keyboard focus also animates the selected
row.

**Diff main panel:** `j`/`k` hunks, `h`/`l` files, `a`/`A` accept hunk/file,
`r`/`R` reject hunk/file (reverts the file), `u` unified/split, `←`/`→`
horizontal scroll, `c` back to chat. Diffs are syntax-highlighted with syntect.

**Tool log:** `t` opens the collapsible tool-call log (`j`/`k`, Enter expand).

**Agents:** the sidebar shows live read-only subagent status. `Tab` to the pane
and use `j`/`k` to select.

**Mouse:** click a pane to focus it; the wheel scrolls the chat, plan, diff, or
agents pane under the pointer. Clicking the chat input enters insert mode. Hold
Shift while dragging to select terminal text (mouse capture is enabled).

### Project workspaces & sessions

`palicode` is project-scoped: when launched inside a repository it uses the git
root as the workspace, even if you start in a subdirectory (`--cwd` overrides).
Sessions, checkpoints, tasks, the sandbox, and `HANDOFF.md` all belong to that
project. Sessions from one project never appear in another project’s sidebar or
`palicode sessions` output, and `palicode resume <id>` refuses a session that
belongs to a different project.

The left sidebar lists the durable sessions for the current project. `s` focuses
it, `n` creates, Enter switches, `d` deletes. Each session keeps its own chat,
tool log, plan, diff, and subagent state, restored from SQLite checkpoints.

### Themes

The default is **black and white** (`mono-dark`): black background, white/gray
text, grayscale syntax highlighting and diff shading. `Ctrl+K → Theme: toggle
dark/light` switches to `mono-light` (white background, black/gray text). Both
are strictly monochrome—no hue anywhere.

## Durable state & long-horizon

Sessions and conversations are checkpointed to SQLite after every turn and tool
result, using WAL with `synchronous=FULL` so a `kill -9` keeps the last committed
snapshot.

- Database: `~/.local/share/palicode/palicode.db` (override with `PALICODE_DB`);
  rows are scoped by project path
- `palicode sessions` lists persisted sessions for the current project;
  `palicode resume [id]` opens one
- On startup the TUI detects sessions left `active` by a crash, marks them
  interrupted, resumes the latest one, and shows a recovery notice
- The session sidebar (`s`) switches between durable sessions; each keeps its own
  conversation, plan, task list, and token counts
- Messages are reconstructed into the transcript/tool log on resume
- Incomplete tool calls left by a crash are closed with an explicit error

Additional long-horizon features:

- **Context compaction:** when history exceeds `compact_after_chars`, older
  messages are summarized by the provider and replaced with a compact system
  summary while keeping the most recent `compact_keep_messages`.
- **Handoff:** on a clean TUI exit, `HANDOFF.md` is written in the workspace with
  the goal, plan status, recent conversation, tools, and a `palicode resume <id>`
  command. Disable with `handoff_on_exit = false`.
- **Verified completion:** set `verify_command = "cargo test"` (or any command).
  After plan execution it runs the command; failures trigger up to
  `verify_retries` self-heal turns. Verification status is surfaced in the TUI
  and makes headless plan runs exit non-zero on failure.
- **Task list/switch:** persisted plan steps are stored as tasks with dependencies;
  press `T` to inspect and jump to a task.
- **Parallel task DAG:** the `task_dag` tool runs read-only research subagents
  concurrently with dependency ordering, skipping dependents when a prerequisite
  fails.

## All provider models

`palicode models` lists the full catalog of every configured provider—OpenCode
Console's public catalog is listed even before an API key is saved (marked
"browsing only"). In the TUI, `m` opens a combined switcher showing
`provider/model` for **all** providers. Selecting an OpenCode model automatically
switches provider; models from providers without a key are marked `!`.

Typical output: OpenCode (≈46 models) + Ollama (local models) in one list.

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
- `PALICODE_DB` - alternate SQLite state database path.
- `PALICODE_CREDENTIALS` - alternate credentials file (default: `~/.config/palicode/credentials.toml`).
- `PALICODE_SANDBOX` - `true|false`.
- `PALICODE_SHOW_REASONING=1` - show model thinking deltas in the REPL.

Config lives at `~/.config/palicode/config.toml`; run `palicode config --init`
to create it. Missing config files fall back to built-in defaults.

HTTP 429 from OpenCode is a gateway rate limit/quota response (it is not a
palicode bug). palicode retries up to two times with backoff and honors
`Retry-After`; if it still fails, wait, switch model/provider, or check Console
credits. Free models are throttled more aggressively than paid models.

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
| `task_dag` | Parallel read-only subagents with dependency ordering |

All filesystem paths are resolved relative to the working directory and
confined to the sandbox root (`tools.path_sandbox = true` by default).
Symlink escapes are rejected. Tool output is truncated to keep context small.

Research subagents are intentionally read-only: they get read/search/git tools
but cannot edit files, run shell, or spawn another subagent. Use `task` for one
focused investigation or `task_dag` to run several in parallel with dependency
ordering.

## Architecture

- `agent/` - turn loop (`run_turn`), plan model + JSON planner, shared plan
  executor, subagent runner
- `approval/` - permission engine, glob rules, built-in safety denies, approvers
- `event/` - `tokio::sync::broadcast` event bus (`Agent`, `Plan`, `PlanStep`,
  `Subagent`, `Status`, `Error`)
- `provider/` - OpenAI-compatible streaming, SSE parser, tool-call assembly
- `tools/` - sandboxed tool implementations + registry
- `state/` - SQLite schema/migrations, WAL checkpoints, sessions, plans/tasks,
  handoffs
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
cargo test          # 144 tests: providers, SSE, plans, permissions, diff revert, TUI, fs, search, shell, git, agent
cargo clippy --all-targets -- -D warnings
```

See `palicode_master_plan.md` for the full roadmap. Phase 5 completes the
planned feature set; future work is hardening, packaging, and deeper MCP/hooks
support.
