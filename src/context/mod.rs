//! Per-job context isolation and state management.
//!
//! Each job runs with its own isolated context that includes:
//! - Conversation history
//! - Action history
//! - State machine
//! - Resource tracking

mod manager;
mod memory;
pub mod progress;
mod state;

pub use manager::ContextManager;
pub use memory::{ActionRecord, ConversationMemory, Memory};
pub use progress::ProgressSender;
pub use state::{JobContext, JobState, StateTransition};
