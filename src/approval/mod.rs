//! Permission engine and approval gate.
//!
//! Decisions combine (in order): built-in safety denies, user glob rules,
//! session-wide "always allow", plan-step approval, the approval mode default.
//! A deny rule or safety rule can never be bypassed by full-auto.

use crate::config::{ApprovalMode, PermissionAction, PermissionsConfig};
use crate::util::text::one_line;
use anyhow::Result;
use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;
use std::collections::HashSet;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}

struct CompiledRule {
    tool: GlobMatcher,
    pattern: Option<GlobMatcher>,
    action: PermissionAction,
    description: String,
}

pub struct PermissionEngine {
    mode: ApprovalMode,
    rules: Vec<CompiledRule>,
    always_allow: HashSet<String>,
}

impl PermissionEngine {
    pub fn new(mode: ApprovalMode, config: &PermissionsConfig) -> Self {
        let mut rules = Vec::new();
        for rule in &config.rules {
            let tool = match compile_glob(&rule.tool) {
                Ok(matcher) => matcher,
                Err(err) => {
                    warn!(tool = %rule.tool, error = %err, "ignoring invalid permission rule");
                    continue;
                }
            };
            let pattern = match rule.pattern.as_deref() {
                Some(pattern) => match compile_glob(pattern) {
                    Ok(matcher) => Some(matcher),
                    Err(err) => {
                        warn!(pattern = %pattern, error = %err, "ignoring invalid permission rule");
                        continue;
                    }
                },
                None => None,
            };
            let description = match &rule.pattern {
                Some(pattern) => format!("tool='{}' pattern='{}'", rule.tool, pattern),
                None => format!("tool='{}'", rule.tool),
            };
            rules.push(CompiledRule {
                tool,
                pattern,
                action: rule.action,
                description,
            });
        }
        Self {
            mode,
            rules,
            always_allow: HashSet::new(),
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: ApprovalMode) {
        self.mode = mode;
    }

    /// Remember `tool` as allowed for the rest of the session ("always").
    pub fn allow_tool_for_session(&mut self, tool: &str) {
        self.always_allow.insert(tool.to_string());
    }

    pub fn check(
        &self,
        tool: &str,
        args: &Value,
        read_only: bool,
        plan_approved: bool,
    ) -> Decision {
        if let Some(reason) = builtin_safety_denial(tool, args) {
            return Decision::Deny(reason);
        }

        let subjects = subjects_for(tool, args);
        let matches = |rule: &CompiledRule| {
            rule.tool.is_match(tool)
                && rule
                    .pattern
                    .as_ref()
                    .map(|pattern| subjects.iter().any(|subject| pattern.is_match(subject)))
                    .unwrap_or(true)
        };

        // A matching deny always wins, regardless of mode or plan approval.
        if let Some(rule) = self
            .rules
            .iter()
            .find(|rule| rule.action == PermissionAction::Deny && matches(rule))
        {
            return Decision::Deny(format!("denied by permission rule ({})", rule.description));
        }

        // Otherwise the first matching allow/ask rule applies.
        if let Some(rule) = self
            .rules
            .iter()
            .find(|rule| rule.action != PermissionAction::Deny && matches(rule))
        {
            return match rule.action {
                PermissionAction::Allow => Decision::Allow,
                PermissionAction::Ask => Decision::Ask(format!(
                    "approval required by permission rule ({})",
                    rule.description
                )),
                PermissionAction::Deny => unreachable!("deny handled above"),
            };
        }

        if self.always_allow.contains(tool) {
            return Decision::Allow;
        }
        if read_only {
            return Decision::Allow;
        }
        // Approving a plan step is the gate for mutations within that step.
        if plan_approved {
            return Decision::Allow;
        }

        match self.mode {
            ApprovalMode::Suggest => {
                Decision::Ask(format!("approval mode is '{}' (mutating tool)", self.mode))
            }
            ApprovalMode::AutoEdit => {
                if is_file_mutation(tool) {
                    Decision::Allow
                } else {
                    Decision::Ask(format!(
                        "approval mode is '{}' (non-edit mutation)",
                        self.mode
                    ))
                }
            }
            ApprovalMode::FullAuto => Decision::Allow,
        }
    }
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
}

fn is_file_mutation(tool: &str) -> bool {
    matches!(tool, "write_file" | "edit_file" | "apply_patch")
}

/// A human-meaningful subject for approval prompts and rule matching.
pub fn subjects_for(tool: &str, args: &Value) -> Vec<String> {
    let string = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    let mut subjects = Vec::new();
    match tool {
        "run_shell" => {
            if let Some(command) = string("command") {
                subjects.push(command.clone());
                let collapsed = command.split_whitespace().collect::<Vec<_>>().join(" ");
                if collapsed != command {
                    subjects.push(collapsed);
                }
            }
            if let Some(cwd) = string("cwd") {
                subjects.push(cwd);
            }
        }
        "write_file" | "edit_file" | "read_file" | "list_dir" | "grep" | "glob" | "git_diff"
        | "git_status" => {
            subjects.push(string("path").unwrap_or_else(|| ".".to_string()));
        }
        "apply_patch" => {
            if let Some(patch) = string("patch") {
                for line in patch.lines() {
                    let header = line
                        .strip_prefix("+++ ")
                        .or_else(|| line.strip_prefix("--- "));
                    let Some(header) = header else { continue };
                    let path = header
                        .split('\t')
                        .next()
                        .unwrap_or(header)
                        .trim()
                        .trim_matches('"');
                    if path == "/dev/null" || path.is_empty() {
                        continue;
                    }
                    let path = path
                        .strip_prefix("a/")
                        .or_else(|| path.strip_prefix("b/"))
                        .unwrap_or(path);
                    subjects.push(path.to_string());
                }
                if subjects.is_empty() {
                    subjects.push(one_line(&patch, 200));
                }
            }
        }
        "task" => {
            if let Some(description) = string("description") {
                subjects.push(description);
            }
            if let Some(prompt) = string("prompt") {
                subjects.push(one_line(&prompt, 200));
            }
        }
        _ => {}
    }

    if subjects.is_empty() {
        subjects.push(one_line(&args.to_string(), 200));
    }
    subjects
}

/// Non-configurable safety net for catastrophic shell commands and git metadata.
fn builtin_safety_denial(tool: &str, args: &Value) -> Option<String> {
    if matches!(tool, "write_file" | "edit_file" | "apply_patch") {
        for subject in subjects_for(tool, args) {
            if subject
                .split(['/', '\\'])
                .any(|component| component == ".git")
            {
                return Some("built-in safety rule: refusing to modify .git metadata".to_string());
            }
        }
    }

    if tool != "run_shell" {
        return None;
    }
    let command = args.get("command").and_then(Value::as_str)?;
    let collapsed = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let without_sudo = collapsed.strip_prefix("sudo ").unwrap_or(&collapsed).trim();
    let lower = without_sudo.to_ascii_lowercase();

    let exact = [
        "rm -rf /",
        "rm -fr /",
        "rm -rf /*",
        "rm -fr /*",
        "rm -rf ~",
        "rm -rf ~/*",
        "rm -rf --no-preserve-root /",
        "chmod -r 777 /",
        ":(){ :|:& };:",
    ];
    if exact.contains(&lower.as_str()) {
        return Some(format!(
            "built-in safety rule: refusing to run '{without_sudo}'"
        ));
    }
    if lower.starts_with("mkfs.") || lower.starts_with("mkfs ") {
        return Some("built-in safety rule: filesystem formatting is refused".to_string());
    }
    if lower.starts_with("dd ") && lower.contains("of=/dev/") {
        return Some("built-in safety rule: raw device writes are refused".to_string());
    }
    if lower == "shutdown"
        || lower.starts_with("shutdown ")
        || lower == "reboot"
        || lower.starts_with("reboot ")
        || lower == "poweroff"
        || lower.starts_with("poweroff ")
        || lower == "halt"
    {
        return Some("built-in safety rule: system power commands are refused".to_string());
    }
    None
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub subject: String,
    pub arguments: Value,
    pub reason: String,
    pub plan_step: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResponse {
    Allow,
    AllowAlways,
    Deny { reason: String },
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalResponse>;
}

/// Denies anything that still needs approval, for headless runs.
pub struct NonInteractiveApprover {
    reason: String,
}

impl NonInteractiveApprover {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Default for NonInteractiveApprover {
    fn default() -> Self {
        Self::new(
            "approval required but this run is non-interactive; re-run with --mode full-auto or --yes",
        )
    }
}

#[async_trait]
impl Approver for NonInteractiveApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalResponse> {
        Ok(ApprovalResponse::Deny {
            reason: format!(
                "{} (tool '{}', subject '{}')",
                self.reason, request.tool, request.subject
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PermissionRule, PermissionsConfig};
    use serde_json::json;

    fn engine(mode: ApprovalMode, rules: Vec<PermissionRule>) -> PermissionEngine {
        PermissionEngine::new(mode, &PermissionsConfig { rules })
    }

    fn rule(tool: &str, pattern: Option<&str>, action: PermissionAction) -> PermissionRule {
        PermissionRule {
            tool: tool.to_string(),
            pattern: pattern.map(str::to_string),
            action,
        }
    }

    #[test]
    fn read_only_tools_are_allowed_in_every_mode() {
        for mode in [
            ApprovalMode::Suggest,
            ApprovalMode::AutoEdit,
            ApprovalMode::FullAuto,
        ] {
            let engine = engine(mode, vec![]);
            assert_eq!(
                engine.check("read_file", &json!({"path": "a"}), true, false),
                Decision::Allow
            );
        }
    }

    #[test]
    fn suggest_asks_before_mutations() {
        let engine = engine(ApprovalMode::Suggest, vec![]);
        assert!(matches!(
            engine.check("edit_file", &json!({"path": "a"}), false, false),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn auto_edit_allows_file_edits_but_asks_for_shell() {
        let engine = engine(ApprovalMode::AutoEdit, vec![]);
        assert_eq!(
            engine.check("edit_file", &json!({"path": "a"}), false, false),
            Decision::Allow
        );
        assert!(matches!(
            engine.check("run_shell", &json!({"command": "make"}), false, false),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn full_auto_allows_mutations() {
        let engine = engine(ApprovalMode::FullAuto, vec![]);
        assert_eq!(
            engine.check("run_shell", &json!({"command": "make"}), false, false),
            Decision::Allow
        );
    }

    #[test]
    fn plan_approval_bypasses_mode_but_not_deny_rules() {
        let engine = engine(
            ApprovalMode::Suggest,
            vec![rule("run_shell", Some("git push*"), PermissionAction::Deny)],
        );
        assert_eq!(
            engine.check("edit_file", &json!({"path": "a"}), false, true),
            Decision::Allow
        );
        assert!(matches!(
            engine.check(
                "run_shell",
                &json!({"command": "git push origin main"}),
                false,
                true
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn user_allow_and_ask_rules_work_in_suggest_mode() {
        let engine = engine(
            ApprovalMode::Suggest,
            vec![
                rule("run_shell", Some("git status*"), PermissionAction::Allow),
                rule("run_shell", Some("git push*"), PermissionAction::Ask),
            ],
        );
        assert_eq!(
            engine.check("run_shell", &json!({"command": "git status"}), false, false),
            Decision::Allow
        );
        assert!(matches!(
            engine.check("run_shell", &json!({"command": "git push"}), false, false),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn builtin_safety_denies_catastrophic_commands() {
        let engine = engine(ApprovalMode::FullAuto, vec![]);
        for command in ["rm -rf /", "sudo rm -rf /", "mkfs.ext4 /dev/sda1", "reboot"] {
            assert!(
                matches!(
                    engine.check("run_shell", &json!({"command": command}), false, true),
                    Decision::Deny(_)
                ),
                "expected denial for {command}"
            );
        }
        assert_eq!(
            engine.check(
                "run_shell",
                &json!({"command": "rm -rf /tmp/palicode-build"}),
                false,
                false
            ),
            Decision::Allow
        );
    }

    #[test]
    fn builtin_safety_denies_git_metadata_edits() {
        let engine = engine(ApprovalMode::FullAuto, vec![]);
        assert!(matches!(
            engine.check("write_file", &json!({"path": ".git/config"}), false, true),
            Decision::Deny(_)
        ));
        assert_eq!(
            engine.check(
                "write_file",
                &json!({"path": "src/.gitignore"}),
                false,
                true
            ),
            Decision::Allow
        );
    }

    #[test]
    fn session_always_allow_is_remembered() {
        let mut engine = engine(ApprovalMode::Suggest, vec![]);
        assert!(matches!(
            engine.check("edit_file", &json!({"path": "a"}), false, false),
            Decision::Ask(_)
        ));
        engine.allow_tool_for_session("edit_file");
        assert_eq!(
            engine.check("edit_file", &json!({"path": "b"}), false, false),
            Decision::Allow
        );
    }

    #[test]
    fn patch_subjects_include_file_paths() {
        let subjects = subjects_for(
            "apply_patch",
            &json!({"patch": "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n"}),
        );
        assert!(subjects.contains(&"src/a.rs".to_string()));
    }
}
