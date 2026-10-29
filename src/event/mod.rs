//! Event bus between the agent loop and the UI (Phase 2).
//!
//! The REPL currently consumes [`crate::agent::AgentEvent`] directly; Phase 2
//! adds a `tokio::sync::mpsc`-based bus and durable event records.
