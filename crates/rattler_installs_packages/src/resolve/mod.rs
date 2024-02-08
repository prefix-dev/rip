//! This module contains the [`resolve`] function which is used
//! to make the PyPI ecosystem compatible with the [`resolvo`] crate.
//!
//! To use this enable the `resolve` feature.
//! Note that this module can also serve an example to integrate an alternate packaging system
//! with [`resolvo`].
//!
//! See the `rip_bin` crate for an example of how to use the [`resolve`] function in the: [RIP Repo](https://github.com/prefix-dev/rip)
//!

mod dependency_provider;
mod pypi_version_types;
mod solve;
pub mod solve_options;
mod solve_types;

pub use pypi_version_types::PypiVersion;
pub use pypi_version_types::PypiVersionSet;
pub use solve::{resolve, PinnedPackage};
