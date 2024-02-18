//! RIP is a library that allows the resolving and installing of Python PyPI packages from Rust into a virtual environment.
//! It's based on our experience with building Rattler and aims to provide the same experience but for PyPI instead of Conda.
//! It should be fast and easy to use.
//! Like Rattler, this library is not a package manager itself but provides the low-level plumbing to be used in one.

#![deny(missing_docs)]

/// Contains the types that are used throughout the library.
pub mod types;

pub mod python_env;

pub mod index;

pub mod install;
mod utils;

pub mod resolve;

pub mod wheel_builder;

mod win;

pub mod artifacts;
pub use utils::normalize_index_url;
