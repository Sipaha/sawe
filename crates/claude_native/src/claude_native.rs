//! Native Rust connection to the `claude` binary's stream-json protocol.
//! Implements `acp_thread::AgentConnection`. This is the sole Claude backend
//! since the `@agentclientprotocol/claude-agent-acp` node wrapper path was
//! retired (revert via git history if it ever needs to come back).

pub mod claude_settings;
pub mod command;
mod connection;
pub mod process;
pub mod protocol;
mod translate;
mod watchdog;
pub mod worktree_hook;

pub use connection::{ClaudeNativeAgentServer, ClaudeNativeConnection};
pub use protocol::ModelInfo;
