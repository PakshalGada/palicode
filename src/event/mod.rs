//! Event bus decoupling the agent loop from front-ends.
//!
//! The REPL renders [`crate::agent::AgentEvent`] directly for low-latency
//! streaming, while every event is also published here. The Phase 3 TUI will
//! subscribe once and drive all panes from this single stream.

use crate::agent::AgentEvent;
use crate::agent::plan::{Plan, StepStatus};
use tokio::sync::broadcast;
use tracing::trace;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    Started,
    Finished,
    Failed,
}

/// Bus events. Some variants are only consumed by tests until the Phase 3 TUI
/// lands; the REPL renders [`AgentEvent`] directly for latency.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Event {
    Agent(AgentEvent),
    Plan(Plan),
    PlanStep {
        index: usize,
        status: StepStatus,
        description: String,
    },
    PlanComplete {
        done: usize,
        failed: usize,
        skipped: usize,
    },
    Subagent {
        description: String,
        state: SubagentState,
        message: Option<String>,
    },
    Status(String),
    Error(String),
}

/// Fan-out event bus. Publishing with no subscribers is a no-op.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { tx }
    }

    pub fn publish(&self, event: Event) {
        trace!(event = ?event, "event");
        let _ = self.tx.send(event);
    }

    #[allow(dead_code)] // used by the Phase 3 TUI and tests
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    #[allow(dead_code)]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_events_to_subscribers() {
        let bus = EventBus::default();
        let mut rx = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        bus.publish(Event::Status("hello".to_string()));
        match rx.recv().await.unwrap() {
            Event::Status(message) => assert_eq!(message, "hello"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn publish_without_subscribers_does_not_panic() {
        let bus = EventBus::default();
        bus.publish(Event::Status("nobody listening".to_string()));
    }
}
