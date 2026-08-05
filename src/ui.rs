//! The Slint bindings generated from `ui/app.slint`.
//!
//! Kept in a module of its own so the code generator — which uses `unwrap()` internally — can
//! opt out of the crate's `unwrap_used = "deny"` lint without loosening it anywhere a human
//! writes code.

#![allow(clippy::unwrap_used)]

slint::include_modules!();
