//! Terminal rendering for agent events (shared by `repl` and `run`).

use crate::agent::AgentEvent;
use crate::util::text::{one_line, truncate_tail};
use std::io::{IsTerminal, Write};

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
// Monochrome palette: tools/info are gray, success is gray, errors are bold.
const CYAN: &str = "\x1b[38;5;252m";
const GREEN: &str = "\x1b[38;5;245m";
const RED: &str = "\x1b[1;38;5;255m";
const YELLOW: &str = "\x1b[38;5;250m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Repl,
    Run,
}

pub struct Printer {
    mode: Mode,
    color_out: bool,
    color_meta: bool,
    text_open: bool,
    show_reasoning: bool,
    reasoning_open: bool,
}

impl Printer {
    pub fn new(mode: Mode) -> Self {
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            mode,
            color_out: !no_color && std::io::stdout().is_terminal(),
            color_meta: !no_color && std::io::stderr().is_terminal(),
            text_open: false,
            show_reasoning: matches!(mode, Mode::Repl)
                && std::env::var("PALICODE_SHOW_REASONING")
                    .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                    .unwrap_or(false),
            reasoning_open: false,
        }
    }

    pub fn event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(delta) => {
                self.close_reasoning();
                self.text_open = true;
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ReasoningDelta(delta) => {
                if self.show_reasoning {
                    let text = self.paint(DIM, &delta);
                    if !self.reasoning_open {
                        print!("{}", self.paint(DIM, "thinking: "));
                        self.reasoning_open = true;
                    }
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
            }
            AgentEvent::AssistantMessage => {
                self.close_text();
                self.close_reasoning();
            }
            AgentEvent::ToolCall {
                name, arguments, ..
            } => {
                self.close_text();
                let preview = one_line(&arguments, 160);
                let line = format!("▸ {name}({preview})");
                self.meta(&self.paint(CYAN, &line));
            }
            AgentEvent::ToolResult {
                name,
                output,
                is_error,
                duration_ms,
                ..
            } => {
                let (icon, color) = if is_error {
                    ("✗", RED)
                } else {
                    ("✓", GREEN)
                };
                let summary = summarize(&output, 4, 400);
                let indented = summary
                    .lines()
                    .map(|line| format!("    {line}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let header = format!("  {icon} {name} ({duration_ms} ms)");
                self.meta(&format!(
                    "{}\n{}",
                    self.paint(color, &header),
                    self.paint(DIM, &indented)
                ));
            }
            AgentEvent::PermissionDenied { tool, reason } => {
                let line = format!("⛔ {tool}: {reason}");
                self.meta(&self.paint(RED, &line));
            }
            AgentEvent::Status(text) => {
                self.close_text();
                self.meta(&self.paint(DIM, &text));
            }
            AgentEvent::Diff { .. } => {
                // The diff is already summarized in the tool result; Phase 3
                // feeds this event into the diff pane.
            }
            AgentEvent::Usage(usage) => {
                if usage.total_tokens > 0 {
                    let line = format!(
                        "  tokens: {} in / {} out ({} total)",
                        usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                    );
                    self.meta(&self.paint(DIM, &line));
                }
            }
            AgentEvent::StepLimit { steps } => {
                let line = format!("step limit reached ({steps} steps)");
                self.meta(&self.paint(YELLOW, &line));
            }
        }
    }

    pub fn info(&mut self, message: &str) {
        self.meta(&self.paint(DIM, message));
    }

    pub fn error(&mut self, message: &str) {
        self.close_text();
        self.meta(&self.paint(RED, &format!("error: {message}")));
    }

    pub fn finish(&mut self) {
        self.close_text();
        self.close_reasoning();
    }

    fn close_text(&mut self) {
        if self.text_open {
            println!();
            self.text_open = false;
        }
    }

    fn close_reasoning(&mut self) {
        if self.reasoning_open {
            println!();
            self.reasoning_open = false;
        }
    }

    fn meta(&self, line: &str) {
        match self.mode {
            Mode::Repl => {
                println!("{line}");
                let _ = std::io::stdout().flush();
            }
            Mode::Run => {
                eprintln!("{line}");
            }
        }
    }

    fn paint(&self, color: &str, text: &str) -> String {
        let enabled = match self.mode {
            Mode::Repl => self.color_out,
            Mode::Run => self.color_meta,
        };
        if enabled {
            format!("{color}{text}{RESET}")
        } else {
            text.to_string()
        }
    }
}

fn summarize(output: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.is_empty() {
        return "(no output)".to_string();
    }
    let mut selected: Vec<String> = lines
        .iter()
        .take(max_lines)
        .map(|line| truncate_tail(line, max_chars))
        .collect();
    if lines.len() > max_lines {
        selected.push(format!("... ({} more lines)", lines.len() - max_lines));
    }
    selected.join("\n")
}
