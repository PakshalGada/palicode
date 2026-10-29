//! Shared stdin and the interactive approval prompt.
//!
//! The REPL and the approver lock the same buffered stdin handle, so piped
//! input works in order (commands, approvals, and plan edits never race).

use crate::approval::{ApprovalRequest, ApprovalResponse, Approver};
use crate::util::text::one_line;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::io::{IsTerminal, Write};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct SharedInput {
    inner: Arc<Mutex<BufReader<tokio::io::Stdin>>>,
}

impl SharedInput {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(BufReader::new(tokio::io::stdin()))),
        }
    }

    pub async fn read_line(&self, line: &mut String) -> Result<usize> {
        line.clear();
        let mut reader = self.inner.lock().await;
        reader.read_line(line).await.context("cannot read stdin")
    }
}

impl Default for SharedInput {
    fn default() -> Self {
        Self::new()
    }
}

pub struct PromptApprover {
    input: SharedInput,
}

impl PromptApprover {
    pub fn new(input: SharedInput) -> Self {
        Self { input }
    }
}

#[async_trait]
impl Approver for PromptApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalResponse> {
        let mut line = String::new();
        for _ in 0..3 {
            println!();
            println!(
                "approval required: {} {}",
                request.tool,
                if request.subject.is_empty() {
                    String::new()
                } else {
                    format!("({})", one_line(&request.subject, 180))
                }
            );
            if let Some(step) = request.plan_step {
                println!("  plan step: {step}");
            }
            if !request.arguments.is_null() {
                let args = one_line(&request.arguments.to_string(), 220);
                if args != "{}" {
                    println!("  args: {args}");
                }
            }
            println!("  reason: {}", request.reason);
            print!("allow? [y]es / [n]o / [a]lways: ");
            let _ = std::io::stdout().flush();

            if self.input.read_line(&mut line).await? == 0 {
                return Ok(ApprovalResponse::Deny {
                    reason: "stdin closed".to_string(),
                });
            }
            if !std::io::stdin().is_terminal() {
                println!();
            }
            match parse_approval_response(&line) {
                Some(response) => return Ok(response),
                None => println!("please answer y, n, or a"),
            }
        }
        Ok(ApprovalResponse::Deny {
            reason: "no valid approval response".to_string(),
        })
    }
}

pub fn parse_approval_response(line: &str) -> Option<ApprovalResponse> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(ApprovalResponse::Allow),
        "a" | "always" => Some(ApprovalResponse::AllowAlways),
        "n" | "no" | "" => Some(ApprovalResponse::Deny {
            reason: "denied by user".to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_approval_answers() {
        assert_eq!(parse_approval_response("y"), Some(ApprovalResponse::Allow));
        assert_eq!(
            parse_approval_response(" ALWAYS "),
            Some(ApprovalResponse::AllowAlways)
        );
        assert!(matches!(
            parse_approval_response("n"),
            Some(ApprovalResponse::Deny { .. })
        ));
        assert!(parse_approval_response("maybe").is_none());
    }
}
