//! Scoped policy engine and approval gate.
//!
//! Rule precedence is intentionally simple and global:
//! `deny` → `ask` → `allow`, then session approvals, then mode defaults.
//! A deny rule always wins, including over a project-level allow and over
//! "always allow". Built-in safety denials and protected paths cannot be
//! overridden by configuration.
//!
//! Compound shell commands are evaluated segment by segment. An allow rule for
//! `Bash(npm test *)` therefore cannot approve `npm test && curl evil`.

pub mod path;
pub mod shell;

use crate::config::{
    ApprovalMode, PermissionAction, PermissionRule, PermissionsConfig,
};
use crate::util::text::one_line;
use anyhow::Result;
use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::warn;

use self::path::PathPattern;
use self::shell::ShellAnalysis;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask(String),
    Deny(String),
}

#[derive(Debug, Clone)]
enum Matcher {
    /// Any use of the tool is allowed.
    Tool,
    /// Match a shell segment after wrappers are stripped.
    Shell(GlobMatcher),
    /// Match one of the paths touched by the call.
    Path(PathPattern),
    /// Match the human-readable subject (legacy `pattern` rules).
    Subject(GlobMatcher),
}

#[derive(Debug, Clone)]
struct CompiledRule {
    tool: String,
    matcher: Matcher,
    action: PermissionAction,
    description: String,
}

impl CompiledRule {
    fn tool_matches(&self, tool: &str) -> bool {
        tool_matches(&self.tool, tool)
    }

    fn matches_segment(&self, tool: &str, segment: &str) -> bool {
        if !self.tool_matches(tool) {
            return false;
        }
        match &self.matcher {
            Matcher::Shell(glob) => glob.is_match(segment),
            Matcher::Tool => true,
            Matcher::Subject(glob) => glob.is_match(segment),
            Matcher::Path(_) => false,
        }
    }

    fn matches(
        &self,
        tool: &str,
        args: &Value,
        cwd: &Path,
        analysis: Option<&ShellAnalysis>,
    ) -> bool {
        if !self.tool_matches(tool) {
            return false;
        }
        match &self.matcher {
            Matcher::Tool => true,
            Matcher::Shell(glob) => analysis
                .map(|analysis| {
                    analysis
                        .segments
                        .iter()
                        .any(|segment| glob.is_match(&segment.command))
                })
                .unwrap_or(false),
            Matcher::Path(pattern) => crate::memory::paths_for_tool(tool, args, cwd)
                .iter()
                .any(|path| pattern.matches(path, cwd)),
            Matcher::Subject(glob) => subjects_for(tool, args)
                .iter()
                .any(|subject| glob.is_match(subject)),
        }
    }
}

pub struct PermissionEngine {
    mode: ApprovalMode,
    cwd: PathBuf,
    rules: Vec<CompiledRule>,
    always_allow: HashSet<String>,
}

impl PermissionEngine {
    pub fn new(mode: ApprovalMode, config: &PermissionsConfig, cwd: impl Into<PathBuf>) -> Self {
        let mut engine = Self {
            mode,
            cwd: cwd.into(),
            rules: Vec::new(),
            always_allow: HashSet::new(),
        };
        for rule in &config.rules {
            engine.add_rule(rule.clone());
        }
        for (action, entries) in [
            (PermissionAction::Allow, &config.allow),
            (PermissionAction::Ask, &config.ask),
            (PermissionAction::Deny, &config.deny),
        ] {
            for entry in entries {
                let (tool, pattern) = parse_tool_specifier(entry);
                engine.add_rule(PermissionRule {
                    tool,
                    pattern,
                    action,
                });
            }
        }
        engine
    }

    pub fn mode(&self) -> ApprovalMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: ApprovalMode) {
        self.mode = mode;
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn add_rule(&mut self, rule: PermissionRule) {
        let description = match &rule.pattern {
            Some(pattern) => format!("tool='{}' pattern='{}'", rule.tool, pattern),
            None => format!("tool='{}'", rule.tool),
        };
        match compile_rule(&rule, &self.cwd) {
            Ok(compiled) => self.rules.push(CompiledRule {
                tool: compiled.0,
                matcher: compiled.1,
                action: rule.action,
                description,
            }),
            Err(err) => warn!(error = %err, rule = %description, "ignoring invalid permission rule"),
        }
    }

    /// Remember `tool` as allowed for the rest of the session ("always").
    pub fn allow_tool_for_session(&mut self, tool: &str) {
        self.always_allow.insert(canonical_tool(tool));
    }

    pub fn rules_summary(&self) -> Vec<String> {
        let mut lines = vec![format!("approval mode: {}", self.mode)];
        for rule in &self.rules {
            lines.push(format!(
                "  {:<5} {}",
                match rule.action {
                    PermissionAction::Allow => "allow",
                    PermissionAction::Ask => "ask",
                    PermissionAction::Deny => "deny",
                },
                rule.description
            ));
        }
        if self.rules.is_empty() {
            lines.push("  (no custom rules; built-in safety rules still apply)".to_string());
        }
        lines
    }

    pub fn check(
        &self,
        tool: &str,
        args: &Value,
        read_only: bool,
        plan_approved: bool,
    ) -> Decision {
        let analysis = if tool == "run_shell" {
            args.get("command")
                .and_then(Value::as_str)
                .map(ShellAnalysis::analyze)
        } else {
            None
        };

        if let Some(reason) = builtin_safety_denial(tool, args, &self.cwd, analysis.as_ref()) {
            return Decision::Deny(reason);
        }

        let effective_read_only = read_only || analysis.as_ref().is_some_and(|a| a.read_only);

        if self.mode == ApprovalMode::Plan && !effective_read_only {
            return Decision::Deny(
                "plan mode is read-only; approve the plan before mutating the workspace".to_string(),
            );
        }

        // Deny always wins.
        if let Some(rule) = self
            .rules
            .iter()
            .find(|rule| rule.action == PermissionAction::Deny && rule.matches(tool, args, &self.cwd, analysis.as_ref()))
        {
            return Decision::Deny(format!("denied by permission rule ({})", rule.description));
        }

        // Explicit ask rules are next.
        if let Some(rule) = self
            .rules
            .iter()
            .find(|rule| rule.action == PermissionAction::Ask && rule.matches(tool, args, &self.cwd, analysis.as_ref()))
        {
            return Decision::Ask(format!(
                "approval required by permission rule ({})",
                rule.description
            ));
        }

        // Compound shell commands need an allow rule for every non-read-only
        // segment. This prevents a narrow allow rule from blessing extra work.
        if let Some(analysis) = &analysis
            && analysis.segments.len() > 1
        {
            let mut all_allowed = true;
            for segment in &analysis.segments {
                if segment.read_only {
                    continue;
                }
                let allowed = self.rules.iter().any(|rule| {
                    rule.action == PermissionAction::Allow
                        && rule.matches_segment(tool, &segment.command)
                });
                if !allowed {
                    all_allowed = false;
                    break;
                }
            }
            if all_allowed {
                return Decision::Allow;
            }
        } else if self
            .rules
            .iter()
            .any(|rule| rule.action == PermissionAction::Allow && rule.matches(tool, args, &self.cwd, analysis.as_ref()))
        {
            return Decision::Allow;
        }

        if self.always_allow.contains(&canonical_tool(tool)) {
            return Decision::Allow;
        }
        if effective_read_only {
            return Decision::Allow;
        }
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
            ApprovalMode::FullAuto | ApprovalMode::BypassPermissions => Decision::Allow,
            ApprovalMode::DontAsk => Decision::Deny(
                "approval mode is 'dont-ask' and this tool would require a prompt".to_string(),
            ),
            ApprovalMode::Plan => {
                // Unreachable because mutations were denied above.
                Decision::Deny("plan mode is read-only".to_string())
            }
        }
    }
}

fn compile_rule(rule: &PermissionRule, cwd: &Path) -> Result<(String, Matcher), anyhow::Error> {
    let (tool, specifier) = parse_tool_specifier(&rule.tool);
    let subject = rule.pattern.clone().or(specifier);
    let canonical = canonical_tool(&tool);
    let matcher = match subject {
        None => Matcher::Tool,
        Some(pattern) => {
            if canonical == "run_shell" {
                Matcher::Shell(compile_glob(&shell_glob(&pattern))?)
            } else if is_path_tool(&canonical) {
                Matcher::Path(PathPattern::compile(&pattern)?)
            } else {
                Matcher::Subject(compile_glob(&pattern)?)
            }
        }
    };
    let _ = cwd;
    Ok((tool, matcher))
}

/// Split `Tool(specifier)` into its parts. Bare names have no specifier.
pub fn parse_tool_specifier(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim();
    if let Some(open) = raw.find('(')
        && raw.ends_with(')')
    {
        let tool = raw[..open].trim().to_string();
        let specifier = raw[open + 1..raw.len() - 1].trim().to_string();
        return (
            tool,
            if specifier.is_empty() {
                None
            } else {
                Some(specifier)
            },
        );
    }
    (raw.to_string(), None)
}

fn shell_glob(pattern: &str) -> String {
    let pattern = pattern.trim();
    if let Some(prefix) = pattern.strip_suffix(":*") {
        format!("{prefix}*")
    } else if pattern == "*" || pattern.contains('*') || pattern.contains('?') {
        pattern.to_string()
    } else {
        // A bare command such as `Bash(git status)` matches exactly that
        // command, not every command with that prefix.
        pattern.to_string()
    }
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(false)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
}

fn canonical_tool(tool: &str) -> String {
    match tool.trim().to_ascii_lowercase().as_str() {
        "bash" | "shell" | "run_shell" | "run-bash" => "run_shell".to_string(),
        "read" | "read_file" | "readfile" => "read_file".to_string(),
        "write" | "write_file" | "writefile" => "write_file".to_string(),
        "edit" | "edit_file" | "editfile" | "multiedit" => "edit_file".to_string(),
        "patch" | "apply_patch" => "apply_patch".to_string(),
        "glob" => "glob".to_string(),
        "grep" | "search" => "grep".to_string(),
        "ls" | "list" | "list_dir" => "list_dir".to_string(),
        "git_status" => "git_status".to_string(),
        "git_diff" => "git_diff".to_string(),
        other => other.to_string(),
    }
}

fn tool_matches(expected: &str, actual: &str) -> bool {
    let expected = canonical_tool(expected);
    let actual = canonical_tool(actual);
    if expected.contains(['*', '?', '[']) {
        compile_glob(&expected)
            .map(|glob| glob.is_match(&actual))
            .unwrap_or(false)
    } else {
        expected == actual
    }
}

fn is_path_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read_file"
            | "write_file"
            | "edit_file"
            | "apply_patch"
            | "list_dir"
            | "grep"
            | "glob"
            | "git_diff"
            | "git_status"
    )
}

fn is_file_mutation(tool: &str) -> bool {
    matches!(tool, "write_file" | "edit_file" | "apply_patch")
}

/// A human-meaningful subject for approval prompts and legacy rules.
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

/// Non-configurable safety net for catastrophic shell commands and protected
/// workspace metadata.
fn builtin_safety_denial(
    tool: &str,
    args: &Value,
    cwd: &Path,
    analysis: Option<&ShellAnalysis>,
) -> Option<String> {
    if matches!(tool, "write_file" | "edit_file" | "apply_patch") {
        for path in crate::memory::paths_for_tool(tool, args, cwd) {
            if path::is_protected(&path) {
                return Some(
                    "built-in safety rule: refusing to modify .git or .palicode metadata"
                        .to_string(),
                );
            }
        }
    }
    if tool == "run_shell"
        && let Some(analysis) = analysis
        && let Some(reason) = &analysis.refusal
    {
        return Some(reason.clone());
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
    /// Allow for the rest of this session.
    AllowAlways,
    /// Persist an allow rule to the project config.
    AllowProject,
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
            "approval required but this run is non-interactive; re-run with --mode full-auto, --mode dont-ask, or --yes",
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

/// Suggest a durable project rule for an approved request. Shell commands get
/// an exact-command rule; path tools get the exact path.
pub fn suggested_rule(tool: &str, args: &Value) -> PermissionRule {
    let canonical = canonical_tool(tool);
    let pattern = if canonical == "run_shell" {
        args.get("command")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else if is_path_tool(&canonical) {
        args.get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    PermissionRule {
        tool: canonical,
        pattern,
        action: PermissionAction::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ApprovalMode, PermissionAction, PermissionRule, PermissionsConfig};
    use serde_json::json;

    fn engine(mode: ApprovalMode, rules: Vec<PermissionRule>) -> PermissionEngine {
        PermissionEngine::new(mode, &PermissionsConfig { rules, ..Default::default() }, "/workspace")
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
            ApprovalMode::DontAsk,
            ApprovalMode::BypassPermissions,
        ] {
            let engine = engine(mode, vec![]);
            assert_eq!(
                engine.check("read_file", &json!({"path": "a"}), true, false),
                Decision::Allow,
                "mode {mode}"
            );
        }
    }

    #[test]
    fn read_only_shell_is_allowed_in_plan_mode() {
        let engine = engine(ApprovalMode::Plan, vec![]);
        assert_eq!(
            engine.check("run_shell", &json!({"command": "git status"}), false, false),
            Decision::Allow
        );
        assert!(matches!(
            engine.check("run_shell", &json!({"command": "npm test"}), false, false),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn plan_mode_blocks_edits_even_with_allow_rule() {
        let engine = engine(
            ApprovalMode::Plan,
            vec![rule("edit_file", None, PermissionAction::Allow)],
        );
        assert!(matches!(
            engine.check("edit_file", &json!({"path": "a"}), false, true),
            Decision::Deny(_)
        ));
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
    fn dont_ask_denies_instead_of_prompting() {
        let engine = engine(ApprovalMode::DontAsk, vec![]);
        assert!(matches!(
            engine.check("run_shell", &json!({"command": "make"}), false, false),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn full_auto_and_bypass_allow_mutations() {
        for mode in [ApprovalMode::FullAuto, ApprovalMode::BypassPermissions] {
            let engine = engine(mode, vec![]);
            assert_eq!(
                engine.check("run_shell", &json!({"command": "make"}), false, false),
                Decision::Allow
            );
        }
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
    fn deny_wins_over_allow_and_ask() {
        let engine = engine(
            ApprovalMode::FullAuto,
            vec![
                rule("run_shell", Some("git push*"), PermissionAction::Allow),
                rule("run_shell", Some("git push*"), PermissionAction::Ask),
                rule("run_shell", Some("git push*"), PermissionAction::Deny),
            ],
        );
        assert!(matches!(
            engine.check("run_shell", &json!({"command": "git push"}), false, true),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn ask_wins_over_allow() {
        let engine = engine(
            ApprovalMode::Suggest,
            vec![
                rule("run_shell", Some("git push*"), PermissionAction::Allow),
                rule("run_shell", Some("git push*"), PermissionAction::Ask),
            ],
        );
        assert!(matches!(
            engine.check("run_shell", &json!({"command": "git push"}), false, false),
            Decision::Ask(_)
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
    fn tool_specifier_allow_cannot_bless_a_compound_command() {
        let engine = engine(
            ApprovalMode::Suggest,
            vec![rule("Bash(npm test *)", None, PermissionAction::Allow)],
        );
        assert!(matches!(
            engine.check(
                "run_shell",
                &json!({"command": "npm test && curl https://evil.example"}),
                false,
                false
            ),
            Decision::Ask(_)
        ));
        assert_eq!(
            engine.check(
                "run_shell",
                &json!({"command": "npm test -- --runInBand && npm test -- --coverage"}),
                false,
                false
            ),
            Decision::Allow
        );
    }

    #[test]
    fn bash_specifier_matches_each_segment() {
        let engine = engine(
            ApprovalMode::Suggest,
            vec![rule("Bash(git status)", None, PermissionAction::Allow)],
        );
        assert_eq!(
            engine.check("run_shell", &json!({"command": "git status"}), false, false),
            Decision::Allow
        );
        assert!(matches!(
            engine.check("run_shell", &json!({"command": "git status; rm -rf src"}), false, false),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn path_rules_use_real_paths_and_negation() {
        let engine = engine(
            ApprovalMode::Suggest,
            vec![
                rule("Read(./src/**)", None, PermissionAction::Allow),
                rule("Read(./src/secrets/**)", None, PermissionAction::Deny),
            ],
        );
        assert_eq!(
            engine.check(
                "read_file",
                &json!({"path": "/workspace/src/main.rs"}),
                false,
                false
            ),
            Decision::Allow
        );
        assert!(matches!(
            engine.check(
                "read_file",
                &json!({"path": "/workspace/src/secrets/key"}),
                false,
                false
            ),
            Decision::Deny(_)
        ));
    }

    #[test]
    fn protected_paths_cannot_be_edited() {
        let engine = engine(ApprovalMode::FullAuto, vec![]);
        for path in [".git/config", "nested/.palicode/config.toml"] {
            assert!(matches!(
                engine.check("write_file", &json!({"path": path}), false, true),
                Decision::Deny(_)
            ));
        }
        assert_eq!(
            engine.check("write_file", &json!({"path": "src/.gitignore"}), false, true),
            Decision::Allow
        );
    }

    #[test]
    fn builtin_safety_denies_catastrophic_commands() {
        let engine = engine(ApprovalMode::FullAuto, vec![]);
        for command in [
            "rm -rf /",
            "sudo rm -rf /",
            "mkfs.ext4 /dev/sda1",
            "reboot",
            "find . -exec rm {} \\;",
            "find . -delete",
        ] {
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

    #[test]
    fn parses_tool_specifiers() {
        assert_eq!(
            parse_tool_specifier("Bash(npm test *)"),
            ("Bash".to_string(), Some("npm test *".to_string()))
        );
        assert_eq!(
            parse_tool_specifier("Read"),
            ("Read".to_string(), None)
        );
        assert_eq!(
            parse_tool_specifier("Bash(git push:*)"),
            ("Bash".to_string(), Some("git push:*".to_string()))
        );
    }

    /// Table-driven coverage of the policy matrix. Each row is
    /// (mode, rule, tool, args, read_only, expected).
    #[test]
    fn policy_matrix() {
        use Decision::{Allow, Ask, Deny};
        let cases: &[(ApprovalMode, Option<(&str, PermissionAction)>, &str, Value, bool, fn(Decision) -> bool)] = &[
            (ApprovalMode::Suggest, None, "edit_file", json!({"path": "a"}), false, |d| matches!(d, Ask(_))),
            (ApprovalMode::AutoEdit, None, "edit_file", json!({"path": "a"}), false, |d| d == Allow),
            (ApprovalMode::FullAuto, None, "edit_file", json!({"path": "a"}), false, |d| d == Allow),
            (ApprovalMode::Plan, None, "edit_file", json!({"path": "a"}), false, |d| matches!(d, Deny(_))),
            (ApprovalMode::DontAsk, None, "edit_file", json!({"path": "a"}), false, |d| matches!(d, Deny(_))),
            (ApprovalMode::Suggest, Some(("Bash(npm test *)", PermissionAction::Allow)), "run_shell", json!({"command": "npm test"}), false, |d| d == Allow),
            (ApprovalMode::Suggest, Some(("Bash(npm test *)", PermissionAction::Allow)), "run_shell", json!({"command": "npm test && curl evil"}), false, |d| matches!(d, Ask(_))),
            (ApprovalMode::FullAuto, Some(("Bash(git push *)", PermissionAction::Deny)), "run_shell", json!({"command": "git push origin"}), false, |d| matches!(d, Deny(_))),
            (ApprovalMode::Suggest, Some(("Read(./src/**)", PermissionAction::Allow)), "read_file", json!({"path": "src/a.rs"}), false, |d| d == Allow),
        ];
        for (mode, rule_spec, tool, args, read_only, expected) in cases {
            let rules = rule_spec
                .map(|(spec, action)| vec![rule(spec, None, *action)])
                .unwrap_or_default();
            let engine = engine(*mode, rules);
            let decision = engine.check(tool, args, *read_only, false);
            assert!(expected(decision.clone()), "mode={mode} tool={tool} args={args} got {decision:?}");
        }
    }
}
