//! Shell command analysis for the policy engine.
//!
//! The permission system must reason about what a command *actually* runs.
//! This module splits compound commands, strips common wrappers, classifies
//! read-only commands, and refuses destructive patterns. It is deliberately
//! conservative: if a construct cannot be understood, it is treated as
//! mutating.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSegment {
    /// The command after wrappers were stripped, used for rule matching.
    pub command: String,
    pub base: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellAnalysis {
    pub segments: Vec<ShellSegment>,
    /// True only when every segment is understood and read-only.
    pub read_only: bool,
    /// A built-in safety refusal, if any.
    pub refusal: Option<String>,
}

impl ShellAnalysis {
    pub fn analyze(command: &str) -> Self {
        let raw_segments = split_commands(command);
        let mut segments = Vec::new();
        let mut refusal = None;
        for raw in raw_segments {
            let command = strip_wrappers(&raw);
            if command.trim().is_empty() {
                continue;
            }
            if let Some(reason) = refusal_for(&command) {
                refusal = Some(reason);
            }
            let base = first_word(&command).to_string();
            let read_only = is_read_only(&command);
            segments.push(ShellSegment {
                command,
                base,
                read_only,
            });
        }
        let read_only = !segments.is_empty()
            && refusal.is_none()
            && segments.iter().all(|segment| segment.read_only);
        Self {
            segments,
            read_only,
            refusal,
        }
    }

    pub fn commands(&self) -> Vec<String> {
        self.segments
            .iter()
            .map(|segment| segment.command.clone())
            .collect()
    }
}

/// Split on `&&`, `||`, `;`, `|`, `&`, and newlines while respecting quotes.
pub fn split_commands(input: &str) -> Vec<String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        Double,
    }

    let chars: Vec<char> = input.chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            current.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' && quote != Quote::Single {
            current.push(ch);
            escaped = true;
            index += 1;
            continue;
        }
        match quote {
            Quote::Single => {
                current.push(ch);
                if ch == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => {
                current.push(ch);
                if ch == '"' {
                    quote = Quote::None;
                }
            }
            Quote::None => match ch {
                '\'' => {
                    quote = Quote::Single;
                    current.push(ch);
                }
                '"' => {
                    quote = Quote::Double;
                    current.push(ch);
                }
                ';' | '\n' | '\r' | '|' | '&' => {
                    push_segment(&mut segments, &mut current);
                    // Consume the second character of && / ||.
                    if (ch == '&' || ch == '|') && chars.get(index + 1) == Some(&ch) {
                        index += 1;
                    }
                }
                _ => current.push(ch),
            },
        }
        index += 1;
    }
    push_segment(&mut segments, &mut current);
    segments
}

fn push_segment(segments: &mut Vec<String>, current: &mut String) {
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
    current.clear();
}

/// Remove common command wrappers (`sudo`, `timeout`, `env`, `nice`, ...).
pub fn strip_wrappers(raw: &str) -> String {
    let mut command = raw.trim().to_string();
    loop {
        let mut words = command.split_whitespace();
        let Some(first) = words.next() else {
            return String::new();
        };
        let rest: Vec<&str> = words.collect();
        let replacement = match first {
            "sudo" => {
                let mut index = 0;
                while index < rest.len() && rest[index].starts_with('-') {
                    // Skip flags and the argument of -u/-g.
                    let needs_arg = matches!(rest[index], "-u" | "-g" | "-C" | "-h" | "-p");
                    index += 1;
                    if needs_arg && index < rest.len() {
                        index += 1;
                    }
                }
                Some(rest[index..].join(" "))
            }
            "timeout" => {
                let mut index = 0;
                while index < rest.len() && rest[index].starts_with('-') {
                    let needs_arg = matches!(rest[index], "-s" | "-k" | "--signal" | "--kill-after");
                    index += 1;
                    if needs_arg && index < rest.len() {
                        index += 1;
                    }
                }
                // Skip the duration argument.
                if index < rest.len() {
                    index += 1;
                }
                Some(rest[index..].join(" "))
            }
            "env" => {
                let mut index = 0;
                while index < rest.len() {
                    if rest[index].starts_with('-') || rest[index].contains('=') {
                        index += 1;
                    } else {
                        break;
                    }
                }
                Some(rest[index..].join(" "))
            }
            "nice" => {
                let mut index = 0;
                while index < rest.len() {
                    if rest[index] == "-n" || rest[index] == "--adjustment" {
                        index += 2;
                    } else if rest[index].starts_with('-') || rest[index].parse::<i32>().is_ok() {
                        index += 1;
                    } else {
                        break;
                    }
                }
                Some(rest[index..].join(" "))
            }
            "command" | "exec" | "nohup" | "builtin" | "time" => Some(rest.join(" ")),
            _ => None,
        };
        match replacement {
            Some(value) if !value.trim().is_empty() && value != command => command = value,
            _ => return command,
        }
    }
}

fn first_word(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('"')
        .trim_matches('\'')
}

fn refusal_for(command: &str) -> Option<String> {
    let lower = command.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let base = words.first().copied().unwrap_or("");

    if base == "find" {
        for flag in ["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprintf"] {
            if words.contains(&flag) {
                return Some(format!(
                    "built-in safety rule: refusing 'find {flag}' (it can execute or delete files)"
                ));
            }
        }
    }
    if matches!(base, "sh" | "bash" | "zsh" | "dash" | "fish")
        && words.iter().any(|word| *word == "-c")
    {
        // Nested shells are not read-only, but only refuse obviously
        // destructive payloads here; approval still applies.
        if lower.contains("rm -rf /") || lower.contains("mkfs.") {
            return Some("built-in safety rule: refusing a destructive nested shell".to_string());
        }
    }
    if lower.starts_with("mkfs.") || base == "mkfs" {
        return Some("built-in safety rule: filesystem formatting is refused".to_string());
    }
    if base == "dd" && lower.contains("of=/dev/") {
        return Some("built-in safety rule: raw device writes are refused".to_string());
    }
    if matches!(base, "shutdown" | "reboot" | "poweroff" | "halt") {
        return Some("built-in safety rule: system power commands are refused".to_string());
    }
    if lower.contains(":(){ :|:& };:") {
        return Some("built-in safety rule: fork bombs are refused".to_string());
    }
    if base == "chmod" && lower.contains("777 /") && lower.contains("-r") {
        return Some("built-in safety rule: recursive chmod on / is refused".to_string());
    }
    if base == "rm" {
        let recursive = words.iter().any(|word| {
            *word == "-r" || *word == "-rf" || *word == "-fr" || *word == "--recursive"
        });
        let root = words.iter().any(|word| {
            matches!(*word, "/" | "/*" | "~" | "~/*" | "--no-preserve-root")
        });
        if recursive && root {
            return Some(format!(
                "built-in safety rule: refusing to recursively delete '{command}'"
            ));
        }
    }
    if lower.starts_with("rm -rf /")
        || lower.starts_with("rm -fr /")
        || lower.starts_with("rm -rf ~")
        || lower.starts_with("rm -fr ~")
    {
        return Some(format!(
            "built-in safety rule: refusing to run '{command}'"
        ));
    }
    None
}

fn is_read_only(command: &str) -> bool {
    if command.contains("$(") || command.contains('`') {
        return false;
    }
    if has_output_redirect(command) {
        return false;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    let base = first_word(command);
    match base {
        "git" => git_read_only(&words[1..]),
        "rg" | "grep" | "egrep" | "fgrep" | "ag" | "ack" | "ls" | "cat" | "head" | "tail"
        | "wc" | "pwd" | "echo" | "printf" | "stat" | "file" | "which" | "whereis"
        | "whoami" | "id" | "date" | "uname" | "df" | "du" | "tree" | "fd" | "sort" | "uniq"
        | "cut" | "tr" | "basename" | "dirname" | "realpath" | "readlink" | "test" | "true"
        | "false" | "diff" | "cmp" | "md5sum" | "sha1sum" | "sha256sum" | "xxd" | "hexdump"
        | "jq" | "column" | "comm" | "join" | "paste" | "nl" | "od" | "strings" | "type" => true,
        "find" => !words.iter().any(|word| {
            matches!(
                *word,
                "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete" | "-fprint" | "-fprintf"
            )
        }),
        "sed" => {
            words.iter().any(|word| *word == "-n" || *word == "--quiet")
                && !words.iter().any(|word| {
                    *word == "-i" || word.starts_with("-i") || *word == "--in-place"
                })
        }
        "awk" | "gawk" => false,
        "xargs" => false,
        "tee" => false,
        _ => false,
    }
}

fn has_output_redirect(command: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '>' => return true,
                _ => {}
            },
        }
    }
    false
}

fn git_read_only(args: &[&str]) -> bool {
    let Some(subcommand) = args.iter().find(|arg| !arg.starts_with('-')) else {
        return false;
    };
    if args.iter().any(|arg| {
        matches!(
            *arg,
            "--output" | "--ext-diff" | "--textconv" | "--exec-path" | "-c"
        )
    }) {
        return false;
    }
    match *subcommand {
        "status" | "diff" | "log" | "show" | "rev-parse" | "ls-files" | "blame" | "grep"
        | "describe" | "cat-file" | "merge-base" | "name-rev" | "shortlog" | "whatchanged" => true,
        "branch" => !args
            .iter()
            .any(|arg| matches!(*arg, "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "--delete")),
        "tag" => args.iter().all(|arg| {
            matches!(*arg, "-l" | "--list" | "-n" | "--contains" | "--no-contains")
                || arg.starts_with("--")
                || !arg.starts_with('-')
        }),
        "remote" => args.iter().all(|arg| !matches!(*arg, "add" | "remove" | "rm" | "set-url")),
        "config" => args.iter().any(|arg| matches!(*arg, "--get" | "--get-all" | "--list" | "-l")),
        "stash" => args.iter().any(|arg| matches!(*arg, "list" | "show")),
        "worktree" => args.iter().any(|arg| matches!(*arg, "list")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_compounds_and_respects_quotes() {
        assert_eq!(
            split_commands("npm test && curl evil; echo 'a;b' || true"),
            vec!["npm test", "curl evil", "echo 'a;b'", "true"]
        );
        assert_eq!(split_commands("rg foo | head"), vec!["rg foo", "head"]);
        assert_eq!(split_commands("echo \"a && b\""), vec!["echo \"a && b\""]);
    }

    #[test]
    fn strips_common_wrappers() {
        assert_eq!(
            strip_wrappers("timeout 30 env FOO=1 nice -n 5 rg foo"),
            "rg foo"
        );
        assert_eq!(strip_wrappers("sudo -u root ls"), "ls");
        assert_eq!(strip_wrappers("command git status"), "git status");
    }

    #[test]
    fn classifies_read_only_commands() {
        for command in [
            "git status",
            "git diff --stat",
            "rg TODO src",
            "ls -la",
            "cat README.md",
            "find . -name '*.rs'",
            "sed -n '1,20p' src/lib.rs",
        ] {
            assert!(is_read_only(command), "expected read-only: {command}");
        }
        for command in [
            "npm test",
            "git commit -m x",
            "rm file",
            "cat x > out",
            "find . -delete",
            "sed -i 's/a/b/' x",
            "echo $(rm -rf /)",
        ] {
            assert!(!is_read_only(command), "expected mutating: {command}");
        }
    }

    #[test]
    fn refuses_destructive_find_and_rm() {
        assert!(ShellAnalysis::analyze("find . -delete")
            .refusal
            .is_some());
        assert!(ShellAnalysis::analyze("find . -exec rm {} \\;")
            .refusal
            .is_some());
        assert!(ShellAnalysis::analyze("rm -rf /").refusal.is_some());
        assert!(ShellAnalysis::analyze("rm -rf ./target").refusal.is_none());
    }

    #[test]
    fn compounds_are_only_read_only_when_all_segments_are() {
        assert!(ShellAnalysis::analyze("git status && rg foo").read_only);
        assert!(!ShellAnalysis::analyze("git status && npm test").read_only);
    }
}
