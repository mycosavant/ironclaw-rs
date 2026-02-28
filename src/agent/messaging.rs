//! Inter-agent communication bus.
//!
//! Provides a per-job message routing system so running agents can exchange
//! structured messages. Each worker registers an inbox (`mpsc::Sender`) in the
//! shared [`AgentMessageBus`] at spawn time; tools like `agent_send` look up
//! the target and deliver directly.
//!
//! # Design choice: per-job mpsc vs broadcast
//!
//! The `broadcast` channel requires `T: Clone`, but [`AgentMessage`] carries an
//! optional `oneshot::Sender` for request-reply patterns — and `oneshot::Sender`
//! is *not* `Clone`. Using per-job `mpsc` channels avoids this limitation,
//! routes messages directly (no filtering), and makes "target not found"
//! immediately detectable.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{RwLock, mpsc, oneshot};
use uuid::Uuid;

/// A message sent between agents via the inter-agent communication bus.
#[derive(Debug)]
pub struct AgentMessage {
    /// Job that sent this message.
    pub from_job_id: Uuid,
    /// The message payload (arbitrary JSON).
    pub payload: serde_json::Value,
    /// Optional one-shot reply channel for request-reply patterns.
    ///
    /// The receiver can call `reply_tx.send(value)` to respond.  If the sender
    /// does not need a reply, this is `None`.
    pub reply_tx: Option<oneshot::Sender<serde_json::Value>>,
}

/// Shared message bus mapping job UUIDs to their inbox senders.
///
/// Workers register their inbox on spawn and deregister on completion.
/// Tools look up the target job's sender to deliver messages.
pub type AgentMessageBus = Arc<RwLock<HashMap<Uuid, mpsc::Sender<AgentMessage>>>>;

/// Create a new, empty message bus.
pub fn new_message_bus() -> AgentMessageBus {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Register a job's inbox sender in the bus.
pub async fn register_inbox(bus: &AgentMessageBus, job_id: Uuid, tx: mpsc::Sender<AgentMessage>) {
    bus.write().await.insert(job_id, tx);
}

/// Remove a job from the bus (called when the worker finishes).
pub async fn unregister_inbox(bus: &AgentMessageBus, job_id: Uuid) {
    bus.write().await.remove(&job_id);
}

/// Send a message to a specific job.
///
/// Returns `Ok(())` on successful delivery to the job's inbox channel.
/// Returns `Err` if the target job is not registered or its inbox is closed.
pub async fn send_message(
    bus: &AgentMessageBus,
    to_job_id: Uuid,
    message: AgentMessage,
) -> Result<(), String> {
    // Clone the sender out of the map before releasing the read lock,
    // so we don't hold the RwLock across the async send().
    let tx = {
        let map = bus.read().await;
        map.get(&to_job_id).cloned()
    };
    match tx {
        Some(tx) => tx
            .send(message)
            .await
            .map_err(|_| format!("Job {} inbox closed", to_job_id)),
        None => Err(format!("Job {} not found in message bus", to_job_id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_register_and_send() {
        let bus = new_message_bus();
        let (tx, mut rx) = mpsc::channel(16);
        let job_id = Uuid::new_v4();

        register_inbox(&bus, job_id, tx).await;

        let msg = AgentMessage {
            from_job_id: Uuid::new_v4(),
            payload: serde_json::json!({"hello": "world"}),
            reply_tx: None,
        };
        send_message(&bus, job_id, msg)
            .await
            .expect("send should succeed");

        let received = rx.try_recv().expect("should have message");
        assert_eq!(received.payload["hello"], "world");
    }

    #[tokio::test]
    async fn test_send_to_unknown_job_fails() {
        let bus = new_message_bus();
        let msg = AgentMessage {
            from_job_id: Uuid::new_v4(),
            payload: serde_json::json!(null),
            reply_tx: None,
        };
        let result = send_message(&bus, Uuid::new_v4(), msg).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn test_unregister_then_send_fails() {
        let bus = new_message_bus();
        let (tx, _rx) = mpsc::channel(16);
        let job_id = Uuid::new_v4();

        register_inbox(&bus, job_id, tx).await;
        unregister_inbox(&bus, job_id).await;

        let msg = AgentMessage {
            from_job_id: Uuid::new_v4(),
            payload: serde_json::json!(null),
            reply_tx: None,
        };
        let result = send_message(&bus, job_id, msg).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_reply_channel() {
        let bus = new_message_bus();
        let (tx, mut rx) = mpsc::channel(16);
        let job_id = Uuid::new_v4();

        register_inbox(&bus, job_id, tx).await;

        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = AgentMessage {
            from_job_id: Uuid::new_v4(),
            payload: serde_json::json!({"question": "ping"}),
            reply_tx: Some(reply_tx),
        };
        send_message(&bus, job_id, msg).await.expect("send ok");

        // Simulate the receiving worker replying
        let received = rx.try_recv().expect("should have message");
        if let Some(reply) = received.reply_tx {
            reply
                .send(serde_json::json!({"answer": "pong"}))
                .expect("reply ok");
        }

        let reply_val = reply_rx.await.expect("should get reply");
        assert_eq!(reply_val["answer"], "pong");
    }
}
