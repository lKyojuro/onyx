//! Native Windows 11 Fluent Design Slint UI presentation layer for Onyx.
//!
//! This crate intentionally has zero dependencies on Onyx core or platform crates.
//! All data and actions flow through the generated [`OnyxWindow`] component,
//! [`OnyxTheme`] global, and exported `Ui*` data structs.

#![allow(
    clippy::all,
    clippy::unwrap_used,
    clippy::expect_used,
    missing_debug_implementations
)]

slint::include_modules!();
