# palicode Master Plan (Rust + ratatui)

**Owner:** personal tool  
**Goal:** Minimal, shortcut-driven, feature-rich TUI coding agent that looks/feels like Grok Build but works with OpenCode API key + local Ollama models. Prioritize (A) polished TUI first, then (B) long-horizon/crash-resume. Ship as a single binary.

**Inspo:** Claude Code (hooks/permissions/memory), OpenCode (provider agnostic UX), Grok Build (plan-review-approve + polish), Codex (approval modes), Aider (diff-first).

**Source docs:** `/home/pakshal/.opencode/plan/implementation_plan.md`, `/home/pakshal/.opencode/plan/palicode_plan.md`

---

## 1) Core Principles
- **TUI first**: Grok-like feel. Minimal chrome, maximum information density. Keyboard-first with obvious shortcuts.
- **Provider agnostic**: OpenAI-compatible first. Support OpenCode API key + Ollama (and easy to extend to others). 
- **Single binary**: `cargo build --release` -> one executable. No venv, no runtime deps.
- **Diff-first**: Rich diff visualizer (unified/split), syntax highlighting, hunk accept/reject.
- **Plan-review-approve**: Editable checklist, reorder, approve selective/all before mutations.
- **Shortcuts-driven**: Command palette (`Ctrl+K`), vim-like nav where sensible, contextual hints.
- **Long-horizon later**: After TUI feels good, add crash-safe checkpoints/resume (WAL + SQLite).

---

## 2) Tech Stack (Rust)
| Layer | Crate(s) | Notes |
|---|---|---|
| TUI | `ratatui`, `crossterm` (or `termion`), `tui-textarea`, `tui-tree-widget` | Core UI, mouse support |
| Async/runtime | `tokio` | Concurrency for subagents/parallel calls |
| HTTP | `reqwest` (with `rustls-tls`) | Provider calls, streaming |
| Serialization | `serde`, `serde_json`, `toml` | Config, events, state |
| CLI | `clap` | Args/subcommands (`tui`, `run`, `resume`, `tasks`) |
| DB/state | `rusqlite` + `r2d2` or just `rusqlite` with WAL | Checkpoints/tasks/turns/sessions |
| Diff/patch | `similar` (or `diff`) + `syntect` | Diff rendering + syntax highlighting |
| PTY/shell | `portable-pty` | Shell tool |
| Utils | `dirs`, `glob`, `walkdir`, `tracing`, `thiserror`, `uuid`, `chrono` | Paths, logging, errors, IDs |
| JSON schema/validation | `jsonschema` or just validate with serde | Tool args |

---

## 3) Architecture
```
palicode/
├── Cargo.toml
├── src/
│   ├── main.rs           # CLI entry
│   ├── cli/              # clap commands
│   ├── config/           # config.toml + env loading
│   ├── provider/         # OpenAI-compatible + OpenCode + Ollama
│   ├── tools/            # tool registry + fs/shell/search/git/task
│   ├── agent/            # loop, planner, reflector, memory
│   ├── state/            # sqlite + schema + checkpoints
│   ├── event/            # event bus (mpsc) for UI/loop decoupling
│   ├── approval/         # plan gate + approval modes
│   ├── ui/               # ratatui app, views, widgets, theme
│   └── util/             # helpers, logging, errors
└── assets/               # themes, keymaps (optional)
```

**Event bus:** Decouple agent loop from TUI via `tokio::sync::mpsc`. Events: `Thought`, `ToolCall`, `ToolResult`, `Plan`, `Diff`, `SubagentUpdate`, `Status`, `Error`.

**Provider interface:** Trait `Provider` with `chat_stream` returning async stream of tokens + tool calls. OpenAI-compatible base covers OpenCode + Ollama.

---

## 4) Feature Set (Prioritized)

### P0 - TUI First (Phase A)
| Feature | Details |
|---|---|
| Minimal layout | Chat (left/center), Plan (right-top), Diff (right-bottom), Subagents (right sidebar or tab), Status bar (bottom). Max screen real estate for content. |
| Shortcuts | Global: `Ctrl+K` palette, `q`/`Esc` context dependent, `Tab` focus switch, `?` help, `m` model switcher, `p` plan focus, `d` diff focus. Vim nav where sensible. |
| Command palette | Fuzzy search all commands/actions. |
| Model switcher | Pick provider/model mid-session (OpenCode API key models + Ollama discovered). |
| Plan-review-approve | Editable checklist, reorder (j/k or mouse), approve all/selective, block mutations until approved. |
| Diff visualizer | Unified + split view, syntax highlighting (syntect), hunk navigation, accept/reject per hunk/file, line numbers. |
| Tool log | Collapsible tool calls/results with args/outcome. |
| Subagent sidebar | Live status, elapsed, model, last message, expandable. |
| Status bar | Model, tokens/cost (track usage), task id, cwd, mode, focus. |
| Themes | Minimal dark theme first (tunable). |
| Session list | Quick switch/resume. |

### P1 - Core Agent (Phase A/B bridge)
| Feature | Details |
|---|---|
| OpenAI-compatible provider | OpenCode API key + Ollama. Config via env `OPENCODE_API_KEY`/`OLLAMA_HOST`. |
| Tool registry | `read_file`, `write_file`, `edit_file`, `apply_patch`, `run_shell`, `grep`, `glob`, `list_dir`, `git_*`, `task` (subagent). |
| Agent loop | think/tool/observe/reflect, max turns/steps, cost guard. |
| Approval modes | `suggest` (default), `auto-edit`, `full-auto` (deny-list still applies). |
| Permissions | allow/ask/deny by glob per tool. |
| Hooks (basic) | Pre/Post tool hooks (config) - can add later but structure now. |
| MCP (structure) | Interface ready, implement later. |

### P2 - Long-Horizon (Phase B)
| Feature | Details |
|---|---|
| Checkpoints | After every tool result + turn, WAL to SQLite. Survive kill -9. |
| Resume | `palicode resume <id>` or from TUI. Rebuild exact state. |
| Task list/switch | Multi-task support. |
| Crash detection | Prompt to resume interrupted tasks. |
| Context compactor | Goal-focused summarization. |
| Handoff summary | `HANDOFF.md` on pause/exit. |
| Verified completion | Run targeted checks on touched files, only mark done if pass, self-heal up to N retries. |
| Task DAG + parallel subagents | Dependencies, parallel dispatch. |

---

## 5) Phased Implementation Plan (Incremental)

### Phase 0: Bootstrap (Day 1)
- [x] `cargo init` in `/home/pakshal/palicode`
- [x] Add deps to `Cargo.toml` (ratatui, crossterm, tokio, reqwest, serde, toml, clap, similar, syntect, tracing; rusqlite/portable-pty deferred to their phases)
- [x] Create module structure (`cli/`, `config/`, `provider/`, `tools/`, `agent/`, `state/`, `event/`, `ui/`, `util/`)
- [x] Basic CLI (`palicode tui`, `palicode run`, `palicode --version`)
- [x] Config loading (`~/.config/palicode/config.toml`, env vars)

**Done:** `cargo run -- --version` works.

### Phase 1: Providers + Tools (Day 1-2)
- [x] Provider trait + OpenAI-compatible impl
- [x] OpenCode API key support + Ollama discovery
- [x] Tool trait + registry
- [x] FS tools (`read/write/edit/apply_patch/list_dir`) with path sandbox
- [x] Shell tool (`run` with timeout/cwd)
- [x] Search tools (`grep/glob` via ripgrep if present, fallback)
- [x] Git tools (status/diff)
- [x] REPL test for provider+tools

**Done:** Can chat with Ollama/OpenCode via REPL and call tools.

### Phase 2: Agent Loop + Plan Gate (Day 2)
- [x] Event bus
- [x] Planner (structured JSON plan)
- [x] Agent loop (think/tool/observe/reflect)
- [x] Approval gate + approval modes
- [x] Permissions engine
- [x] Subagent tool skeleton

**Done:** Plan -> approve -> execute works in REPL.

### Phase 3: TUI Core (Day 2-4) - **FOCUS A**
- [x] App shell with layout (panes)
- [x] Chat view (streaming)
- [x] Plan view (editable checklist, reorder)
- [x] Diff view (unified/split, syntect highlighting, hunk nav, accept/reject)
- [x] Tool log view
- [x] Subagent sidebar
- [x] Status bar
- [x] Command palette (`Ctrl+K`)
- [x] Model switcher
- [x] Keybindings + help
- [x] Theme system (minimal dark)
- [x] Focus management + shortcuts
- [x] Settings screen for secure OpenCode API key storage

**Done:** TUI feels minimal, shortcut-driven, feature-rich. Diff visualizer works well. Plan review smooth.

### Phase 4: Polish TUI + Sessions (Day 4-5)
- [x] Session list modal
- [x] Mouse support
- [x] Better streaming UX
- [x] Error display
- [x] Config UI hints
- [x] Performance tweaks

**Done:** Daily-usable TUI.

### Phase 5: Long-Horizon (Phase B) - After TUI solid
- [ ] SQLite schema + migrations
- [ ] Checkpoint system (WAL)
- [ ] Resume logic + crash detection
- [ ] Task list/switch
- [ ] Context compactor
- [ ] Handoff summary
- [ ] Verified completion + self-heal
- [ ] Task DAG + parallel subagents

**Done:** Crash-safe resume works.

---

## 6) Config
`~/.config/palicode/config.toml` example:
```toml
[general]
theme = "minimal-dark"
approval_mode = "suggest" # suggest|auto-edit|full-auto
log_level = "info"

[providers]
[[providers.list]]
name = "opencode"
type = "openai_compatible"
base_url = "https://api.opencode.ai/v1" # adjust if needed
api_key_env = "OPENCODE_API_KEY"
default_model = "claude-3.5-sonnet" # example

[[providers.list]]
name = "ollama"
type = "openai_compatible"
base_url = "http://localhost:11434/v1"
api_key_env = ""
default_model = "llama-3.1:8b"

[tools]
shell_timeout_secs = 120
path_sandbox = true

[tui]
mouse = true
vim_nav = true
```

Env: `OPENCODE_API_KEY`, `OLLAMA_HOST` (optional).

---

## 7) Keybindings (Minimal + Intuitive)
| Key | Action |
|---|---|
| `Ctrl+K` | Command palette |
| `Tab` / `Shift+Tab` | Cycle focus (Chat/Plan/Diff/Subagents) |
| `m` | Model switcher |
| `n` | New session |
| `s` | Session list |
| `p` | Focus plan |
| `d` | Focus diff |
| `t` | Focus tool log/subagents |
| `a` | Approve all plan steps |
| `e` | Edit selected plan step |
| `r` | Reorder plan steps |
| `Enter` | Contextual action |
| `j/k` or `↑/↓` | Navigate lists |
| `h/l` or `←/→` | Navigate panes/hunks |
| `?` | Help overlay |
| `q` | Quit/context back |
| `Esc` | Cancel/close modal |

---

## 8) First 10 Commits Plan
1. `chore: init rust project + deps` 
2. `feat: cli skeleton + config loading`
3. `feat: provider trait + openai-compatible (opencode/ollama)`
4. `feat: tool registry + fs/shell/search`
5. `feat: event bus + agent loop skeleton`
6. `feat: planner + approval gate`
7. `feat: tui app shell + layout + keybindings`
8. `feat: chat/plan/diff views + diff visualizer`
9. `feat: command palette + model switcher + subagent sidebar`
10. `feat: polish shortcuts + theme`

---

## 9) Immediate Next Steps
1. **Init repo**: `cd /home/pakshal/palicode && git init`
2. **Create Cargo.toml** with deps listed above
3. **Scaffold modules** as per architecture
4. **Implement Phase 0** (CLI + config)
5. **Test with Ollama first** (local) then OpenCode API key
6. **Focus on TUI feel early** - iterate on layout/shortcuts until it feels snappy

---

**Decision:** You're set on Rust, (A) first then (B), single binary, OpenCode API + Ollama, personal tool, minimal+shortcut-driven+feature-rich TUI with diff visualizer. This plan matches all of that. Start with Phase 0 now.
