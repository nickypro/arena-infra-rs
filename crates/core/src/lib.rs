//! arena-core: streamlined ARENA infrastructure control plane.
//!
//! Design goals (see README): provider-agnostic machine lifecycle behind a
//! single [`Provider`] trait, a tolerant `config.env` parser compatible with the
//! existing bash/python tooling, and a safety posture where nothing here mutates
//! remote state unless a caller explicitly asks it to.

pub mod config;
pub mod error;
pub mod naming;
pub mod pod;
pub mod provider;
pub mod proxy;

pub use config::Config;
pub use error::{Error, Result};
pub use pod::{Pod, PodSpec};
pub use provider::Provider;
