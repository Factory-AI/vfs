//! Sealed Linux FUSE mount surface for AgentFS.
//!
//! The adopted FUSE transport and AgentFS adapter are private implementation
//! details. Callers receive only the mount entry point, mount options, and
//! session handle needed for lifecycle management (`mount`,
//! `FuseMountOptions`, `SessionHandle`).
//!
//! Owned invariants:
//!
//! - no-open/no-flush semantics: OPEN/RELEASE and close-time FLUSH answer
//!   `ENOSYS` by default, per-inode resources live in a bounded LRU, and
//!   cleanup is driven by FORGET traffic and eviction, never by RELEASE.
//! - POSIX lookup-reference accounting: every positive lookup reply retains
//!   the backing inode reference it reports, and FORGET releases exactly
//!   that count.
//! - Cache coherence: namespace mutations invalidate affected kernel and
//!   adapter cache entries before the mutating reply is sent (epoch/reply
//!   lock protocol in the adapter caches; audited in debug builds).
//! - Transport neutrality: the io_uring and `/dev/fuse` legs share request
//!   semantics and bounded teardown; unmount joins all transport threads.
//! - No lock guard is held across an `.await` or `block_on` boundary.

// The transport and adapter are Linux-only (/dev/fuse, io_uring, mount(2));
// compile to an empty crate elsewhere so workspace-wide checks pass on macOS.
#![cfg(target_os = "linux")]

mod adapter;
pub(crate) mod telemetry;
pub(crate) mod transport;

pub use adapter::{mount, FuseMountOptions, SessionHandle};
