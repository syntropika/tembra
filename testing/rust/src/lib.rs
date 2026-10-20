//! Fixtures and tools for exercising Tembra through its public runtime APIs.
//!
//! This crate is development support. Production adapters do not depend on it.
//! Its counter is shared by Rust socket tests and browser acceptance tools;
//! protocol, references, resources, and lifecycle remain owned by `tembra-core`.

pub mod counter;
