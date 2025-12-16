//! This module contains the auto-generated Rust structs for Knative resources.
//! These are generated using `kopium` from the Knative Serving CRDs.
//!
//! Do not edit these files directly. Instead, re-run `kopium` and update the generated files
//! in `src/resources/knative/` directory.

pub mod configuration;
pub mod route;
pub mod revision;

// Re-export the types from the sub-modules for easier access.
pub use configuration::*;
pub use route::*;
pub use revision::*;
