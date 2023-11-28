//! RIP is a library that allows the resolving and installing of Python PyPI packages from Rust into a virtual environment.
//! It's based on our experience with building Rattler and aims to provide the same experience but for PyPI instead of Conda.
//! It should be fast and easy to use.
//! Like Rattler, this library is not a package manager itself but provides the low-level plumbing to be used in one.

#![deny(missing_docs)]

/// Contains the types that are used throughout the library.
pub mod types;

pub mod index;
mod seek_slice;
mod utils;

mod env_markers;
#[cfg(feature = "resolvo")]
mod resolve;
pub mod tags;

mod distribution_finder;
pub mod uninstall;
mod wheel;

mod sdist;
mod system_python;
mod wheel_builder;

mod venv;
mod win;

#[cfg(feature = "resolvo")]
pub use resolve::{resolve, PinnedPackage, ResolveOptions, SDistResolution};

pub use distribution_finder::{find_distributions_in_venv, Distribution, FindDistributionError};
pub use env_markers::Pep508EnvMakers;
pub use pep440_rs::{Version, VersionSpecifier, VersionSpecifiers};
pub use pep508_rs::{MarkerEnvironment, Requirement};
pub use utils::normalize_index_url;
pub use wheel::{InstallPaths, UnpackWheelOptions, Wheel};
pub use wheel_builder::{WheelBuildError, WheelBuilder};
