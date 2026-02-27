//! Tool progress reporting channel.
//!
//! `ProgressSender` is a lightweight handle that tools use to stream
//! incremental output (e.g., stdout lines from a shell command) back to the
//! user in real time.  It wraps an optional `mpsc::UnboundedSender` so tools
//! that don't need streaming can ignore it with zero overhead.

use std::sync::Arc;

use tokio::sync::mpsc;

/// A progress event emitted by a tool during execution.
#[derive(Debug, Clone)]
pub struct ProgressEvent {
    /// Name of the tool producing the event.
    pub tool_name: String,
    /// Incremental output chunk (e.g., a single line of stdout).
    pub chunk: String,
}

/// Cloneable, cheaply constructable handle for streaming tool progress.
///
/// Attached to `JobContext` as a `#[serde(skip)]` field.  The default
/// (`ProgressSender::noop()`) discards all events so existing tools
/// compile and work without modification.
#[derive(Clone, Default)]
pub struct ProgressSender {
    inner: Option<Arc<mpsc::UnboundedSender<ProgressEvent>>>,
}

impl std::fmt::Debug for ProgressSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgressSender")
            .field("active", &self.inner.is_some())
            .finish()
    }
}

impl ProgressSender {
    /// Create a no-op sender that silently discards all events.
    pub fn noop() -> Self {
        Self { inner: None }
    }

    /// Create a live sender backed by an unbounded channel.
    pub fn new(tx: mpsc::UnboundedSender<ProgressEvent>) -> Self {
        Self {
            inner: Some(Arc::new(tx)),
        }
    }

    /// Send a progress chunk.  No-ops silently if the receiver is gone
    /// or this is a no-op sender.
    pub fn send(&self, tool_name: &str, chunk: impl Into<String>) {
        if let Some(ref tx) = self.inner {
            let _ = tx.send(ProgressEvent {
                tool_name: tool_name.to_string(),
                chunk: chunk.into(),
            });
        }
    }

    /// Whether this sender is wired to a live receiver.
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }
}
