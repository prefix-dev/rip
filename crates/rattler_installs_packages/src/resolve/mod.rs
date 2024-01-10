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
mod solve;

pub use solve::{resolve, PinnedPackage, PreReleaseResolution, ResolveOptions, SDistResolution};
