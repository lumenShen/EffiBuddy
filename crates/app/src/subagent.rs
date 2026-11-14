//! Compatibility shim.
//!
//! The runtime was split into focused modules under [`crate::agents`]. This
//! file keeps the old import path working for the app shell and the side pane
//! while new code should import `crate::agents::SubagentService` directly.

pub use crate::agents::SubagentService;
