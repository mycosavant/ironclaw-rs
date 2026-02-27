//! Cryptographic utilities for manifest and artifact verification.

pub mod signing;

pub use signing::{SignatureError, SigningConfig};
