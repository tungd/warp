//! Handoff adapters for local-to-cloud transitions.
//!
//! Provides builder patterns and extension traits to isolate local agent
//! modifications from upstream SpawnAgentRequest construction.

mod builder;
mod toast;

pub use builder::LocalSpawnRequestBuilder;
pub use toast::show_local_handoff_toast;
