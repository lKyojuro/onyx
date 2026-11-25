//! Persistent atomic configuration and session recovery for Onyx.

mod model;
mod store;

pub use model::{
    AppConfig, DEFAULT_TERMINAL, MAX_RECENT_LOCATIONS, SUPPORTED_TERMINALS, SessionState,
};
pub use store::{ConfigStore, atomic_write};
