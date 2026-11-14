//! Identifier helpers.
//!
//! IDs are opaque strings so they serialize cleanly in session files and can
//! be sent over the UI event channel without extra wrapper types.

use uuid::Uuid;

pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

pub fn short_id() -> String {
    Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(8)
        .collect()
}
