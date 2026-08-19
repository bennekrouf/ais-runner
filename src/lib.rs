//! ais-runner as a library.
//!
//! Exists so the GUI (`src/main.rs`) and the headless scenario runner
//! (`src/bin/ais-test.rs`) can share one copy of the service layer instead of
//! each compiling its own. Nothing here is public API for outside consumers —
//! the module tree is exposed wholesale because both binaries live in this repo.

pub mod components;
pub mod handlers;
pub mod screens;
pub mod services;
pub mod update_check;
pub mod utils;
