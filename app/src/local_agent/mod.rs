//! WarpSOLO local agent adapters.
//!
//! Provides builder patterns and extension traits that isolate local agent
//! modifications from upstream Warp code, reducing merge conflicts.

#[cfg(all(feature = "local_fs", not(target_family = "wasm")))]
pub mod handoff;
