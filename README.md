# palicode

Minimal, shortcut-driven terminal coding agent in Rust. Provider-agnostic
(OpenCode Console + local Ollama), tool-driven, single binary.

**Status:** Phase 0 (bootstrap) and Phase 1 (providers + tools) are complete.
The TUI lands in Phase 3; for now `palicode repl` and `palicode run` are fully
functional.

## Quick start

```bash
cargo build --release

# Interactive session (auto-picks OpenCode if OPENCODE_API_KEY is set, else Ollama)
./target/release/palicode repl

# Pin provider/model
./target/release/palicode repl --provider ollama --model qwen3.5:9b

# One-shot / headless
./target/release/palicode run "Find where the config is loaded and explain it"
echo "Run the tests and fix failures" | ./target/release/palicode run

# Discovery
palicode models
palicode tools
palicode config            # effective config
palicode config --init     # write a commented example config
```

`palicode` with no subcommand currently prints a placeholder for the Phase 3 TUI.

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

All filesystem paths are resolved relative to the working directory and
confined to the sandbox root (`tools.path_sandbox = true` by default).
Symlink escapes are rejected. Tool output is truncated to keep context small.

## REPL commands

`/help`, `/models`, `/model <id>`, `/provider [name]`, `/tools`,
`/system [prompt]`, `/clear`, `/cwd`, `/history`, `/quit`.

## Verification

```bash
cargo test          # 51 tests: providers, SSE, patch, fs, search, shell, git
cargo clippy --all-targets -- -D warnings
```

See `palicode_master_plan.md` for the full roadmap. Next: Phase 2 (event bus,
planner, approval gate, permissions, subagents).
