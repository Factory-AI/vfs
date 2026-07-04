//! Shared semantics facade under the transport adapters.
//!
//! M6 fills this module with the permission, durability, and handle-table
//! contracts that sit under the FUSE and NFS adapters. Access control lives
//! here so transport handlers cannot drift on POSIX mode-bit behavior.

pub mod access;
