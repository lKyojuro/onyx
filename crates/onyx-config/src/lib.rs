//! Persistent atomic configuration and session recovery for Onyx.

mod model;
mod store;

pub use model::{AppConfig, MAX_RECENT_LOCATIONS, SessionState};
pub use store::{ConfigStore, atomic_write};
