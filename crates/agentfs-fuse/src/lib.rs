//! Sealed Linux FUSE mount surface for AgentFS.
//!
//! The adopted FUSE transport and AgentFS adapter are private implementation
//! details. Callers receive only the mount entry point, mount options, and
//! session handle needed for lifecycle management.

mod adapter;
pub(crate) mod telemetry;
pub(crate) mod transport;

pub use adapter::{mount, FuseMountOptions, SessionHandle};
