//! This module contains functions for working with PyPA packaging repositories.

mod file_store;

mod git;
pub mod html;
mod http;
mod package_database;

pub use package_database::PackageDb;

pub use self::http::CacheMode;
pub use html::parse_hash;
