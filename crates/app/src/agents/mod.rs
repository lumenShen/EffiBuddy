//! Agent runtime and address book.
//!
//! The UI-facing `AgentTask` projection still lives in `app.rs` because it is
//! shared with the side pane; everything that executes a child or routes a
//! message lives here.
//!
//! Layout:
//!
//! - [`types`] — prepared child, registry record, visibility rule;
//! - [`registry`] — names, records, address resolution, groups;
//! - [`prompt`] — child system prompt;
//! - [`runtime`] — `SubagentService`, execution loop, wake-on-message.

pub mod prompt;
pub mod registry;
pub mod runtime;
pub mod types;

pub use runtime::SubagentService;

#[cfg(test)]
mod tests;
