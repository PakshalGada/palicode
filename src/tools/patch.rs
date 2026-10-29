//! A small, dependency-free unified-diff parser and applier.
//!
//! Supports multi-file patches, additions (`--- /dev/null`), deletions
//! (`+++ /dev/null`), and context-based hunk location with a fuzzy search.

use anyhow::{Context, Result, bail};
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct FilePatch {
    /// `None` means the file is being created (`/dev/null` on the old side).
    pub old_path: Option<String>,
    /// `None` means the file is being deleted.
    pub new_path: Option<String>,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: usize,
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
pub struct HunkLine {
    /// ' ' (context), '+' (added), or '-' (removed).
    pub kind: char,
    pub text: String,
}

/// Parse a unified diff. Prose around the patch is ignored.
pub fn parse_unified_patch(patch: &str) -> Result<Vec<FilePatch>> {
    let lines: Vec<&str> = patch.lines().collect();
    let mut files = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if !lines[index].starts_with("--- ") {
            index += 1;
            continue;
        }
        let Some(new_header) = lines.get(index + 1).filter(|line| line.starts_with("+++ ")) else {
            index += 1;
            continue;
        };

        let old_path = parse_header_path(&lines[index][4..]);
        let new_path = parse_header_path(&new_header[4..]);
        index += 2;

        let mut hunks = Vec::new();
        while index < lines.len() {
            let line = lines[index];
            if line.starts_with("@@") {
                let header = parse_hunk_header(line)
                    .with_context(|| format!("invalid hunk header: {line}"))?;
                index += 1;

                let mut body = Vec::new();
                while index < lines.len() {
                    let body_line = lines[index];
                    if body_line.starts_with("@@")
                        || body_line.starts_with("diff --git")
                        || (body_line.starts_with("--- ")
                            && lines
                                .get(index + 1)
                                .map(|next| next.starts_with("+++ "))
                                .unwrap_or(false))
                    {
                        break;
                    }

                    let Some(kind) = body_line.chars().next() else {
                        body.push(HunkLine {
                            kind: ' ',
                            text: String::new(),
                        });
                        index += 1;
                        continue;
                    };

                    match kind {
                        ' ' | '+' | '-' => {
                            body.push(HunkLine {
                                kind,
                                text: body_line[1..].to_string(),
                            });
                            index += 1;
                        }
                        '\\' => {
                            // "\ No newline at end of file" marker.
                            index += 1;
                        }
                        _ => break,
                    }
                }

                if body.is_empty() {
                    bail!("empty hunk at patch line {}", index + 1);
                }
                hunks.push(Hunk {
                    old_start: header.old_start,
                    old_len: header.old_len,
                    new_start: header.new_start,
                    new_len: header.new_len,
                    lines: body,
                });
                continue;
            }

            // Metadata between file sections, or the next file header.
            if line.trim().is_empty()
                || line.starts_with("diff --git")
                || line.starts_with("index ")
                || line.starts_with("new file mode")
                || line.starts_with("deleted file mode")
                || line.starts_with("old mode")
                || line.starts_with("new mode")
                || line.starts_with("similarity index")
                || line.starts_with("rename from")
                || line.starts_with("rename to")
            {
                index += 1;
                continue;
            }
            break;
        }

        if hunks.is_empty() {
            bail!(
                "patch for {} has no hunks",
                new_path
                    .as_deref()
                    .or(old_path.as_deref())
                    .unwrap_or("<unknown>")
            );
        }
        files.push(FilePatch {
            old_path,
            new_path,
            hunks,
        });
    }

    if files.is_empty() {
        bail!("no unified diff found (expected '--- ' followed by '+++ ')");
    }
    Ok(files)
}

struct HunkHeader {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
}

fn parse_hunk_header(line: &str) -> Result<HunkHeader> {
    static HUNK: OnceLock<Regex> = OnceLock::new();
    let regex = HUNK.get_or_init(|| {
        Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@").expect("valid regex")
    });
    let captures = regex
        .captures(line)
        .with_context(|| format!("expected '@@ -old,count +new,count @@', got '{line}'"))?;
    let number = |index: usize, default: usize| -> Result<usize> {
        match captures.get(index) {
            Some(value) => value
                .as_str()
                .parse()
                .with_context(|| format!("invalid number in hunk header '{line}'")),
            None => Ok(default),
        }
    };
    Ok(HunkHeader {
        old_start: number(1, 1)?,
        old_len: number(2, 1)?,
        new_start: number(3, 1)?,
        new_len: number(4, 1)?,
    })
}

fn parse_header_path(raw: &str) -> Option<String> {
    let raw = raw.split('\t').next().unwrap_or(raw).trim();
    if raw.is_empty() || raw == "/dev/null" {
        return None;
    }
    let raw = raw.trim_matches('"');
    let raw = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    Some(raw.to_string())
}

/// Apply hunks to a file's text, returning the new text.
pub fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String> {
    let had_trailing_newline = original.is_empty() || original.ends_with('\n');
    let mut lines: Vec<String> = if original.is_empty() {
        Vec::new()
    } else {
        original.split('\n').map(str::to_string).collect()
    };
    if original.ends_with('\n') {
        lines.pop();
    }

    let mut offset: isize = 0;
    for hunk in hunks {
        let old_seq: Vec<&str> = hunk
            .lines
            .iter()
            .filter(|line| line.kind != '+')
            .map(|line| line.text.as_str())
            .collect();
        let new_seq: Vec<String> = hunk
            .lines
            .iter()
            .filter(|line| line.kind != '-')
            .map(|line| line.text.clone())
            .collect();

        let expected = hunk.old_start.saturating_sub(1) as isize + offset;
        let found = locate(&lines, &old_seq, expected).with_context(|| {
            format!(
                "cannot apply hunk @@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
            )
        })?;

        lines.splice(found..found + old_seq.len(), new_seq.iter().cloned());
        let length_delta = new_seq.len() as isize - old_seq.len() as isize;
        offset = found as isize - (hunk.old_start as isize - 1) + length_delta;
    }

    let mut output = lines.join("\n");
    if !lines.is_empty() && had_trailing_newline {
        output.push('\n');
    }
    Ok(output)
}

fn locate(lines: &[String], old: &[&str], expected: isize) -> Result<usize> {
    if old.is_empty() {
        return Ok(expected.clamp(0, lines.len() as isize) as usize);
    }
    let max_start = lines.len().saturating_sub(old.len()) as isize;
    if max_start < 0 {
        bail!(
            "hunk expects {} lines but the file has only {}",
            old.len(),
            lines.len()
        );
    }

    let start = expected.clamp(0, max_start);
    if matches_at(lines, old, start as usize) {
        return Ok(start as usize);
    }
    for distance in 1..=2_000isize {
        let below = start - distance;
        if below >= 0 && matches_at(lines, old, below as usize) {
            return Ok(below as usize);
        }
        let above = start + distance;
        if above <= max_start && matches_at(lines, old, above as usize) {
            return Ok(above as usize);
        }
    }
    bail!(
        "context not found near line {} (file may have changed; re-read it)",
        expected.max(0) + 1
    )
}

fn matches_at(lines: &[String], old: &[&str], at: usize) -> bool {
    old.iter().enumerate().all(|(offset, expected)| {
        lines
            .get(at + offset)
            .map(|line| line.as_str())
            .map(|line| line == *expected)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(
        old_start: usize,
        old_len: usize,
        new_start: usize,
        new_len: usize,
        lines: &[(char, &str)],
    ) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
            lines: lines
                .iter()
                .map(|(kind, text)| HunkLine {
                    kind: *kind,
                    text: text.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn applies_simple_update() {
        let original = "one\ntwo\nthree\n";
        let h = hunk(
            1,
            3,
            1,
            3,
            &[(' ', "one"), ('-', "two"), ('+', "TWO"), (' ', "three")],
        );
        let output = apply_hunks(original, &[h]).unwrap();
        assert_eq!(output, "one\nTWO\nthree\n");
    }

    #[test]
    fn applies_multiple_hunks_with_offset() {
        let original = "a\nb\nc\nd\ne\nf\n";
        let first = hunk(1, 2, 1, 3, &[(' ', "a"), ('+', "x"), (' ', "b")]);
        let second = hunk(5, 2, 6, 2, &[(' ', "e"), ('-', "f"), ('+', "F")]);
        let output = apply_hunks(original, &[first, second]).unwrap();
        assert_eq!(output, "a\nx\nb\nc\nd\ne\nF\n");
    }

    #[test]
    fn inserts_at_start() {
        let original = "b\nc\n";
        let h = hunk(0, 0, 1, 1, &[('+', "a")]);
        assert_eq!(apply_hunks(original, &[h]).unwrap(), "a\nb\nc\n");
    }

    #[test]
    fn preserves_missing_trailing_newline() {
        let original = "one\ntwo";
        let h = hunk(1, 2, 1, 2, &[(' ', "one"), ('-', "two"), ('+', "TWO")]);
        assert_eq!(apply_hunks(original, &[h]).unwrap(), "one\nTWO");
    }

    #[test]
    fn reports_context_mismatch() {
        let original = "one\nthree\n";
        let h = hunk(1, 2, 1, 2, &[(' ', "one"), ('-', "two"), ('+', "TWO")]);
        assert!(apply_hunks(original, &[h]).is_err());
    }

    #[test]
    fn parses_multi_file_patch() {
        let patch = r#"Here is the patch:
--- a/src/a.txt
+++ b/src/a.txt
@@ -1,2 +1,2 @@
 one
-two
+TWO
diff --git a/src/b.txt b/src/b.txt
index 111..222 100644
--- a/src/b.txt
+++ b/src/b.txt
@@ -1 +1 @@
-old
+new
"#;
        let files = parse_unified_patch(patch).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path.as_deref(), Some("src/a.txt"));
        assert_eq!(files[1].hunks[0].old_start, 1);
    }

    #[test]
    fn parses_creation_and_deletion() {
        let patch = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+hi\n+there\n";
        let files = parse_unified_patch(patch).unwrap();
        assert!(files[0].old_path.is_none());
        assert_eq!(files[0].new_path.as_deref(), Some("new.txt"));

        let deletion = "--- a/gone.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-bye\n";
        let files = parse_unified_patch(deletion).unwrap();
        assert!(files[0].new_path.is_none());
    }

    #[test]
    fn rejects_patch_without_headers() {
        assert!(parse_unified_patch("just some text").is_err());
    }
}
