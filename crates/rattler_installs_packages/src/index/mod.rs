//! This module contains functions for working with PyPA packaging repositories.

mod file_store;

mod direct_url;
mod git_interop;
pub mod html;
mod http;
mod lazy_metadata;
mod package_database;
mod package_sources;

pub use package_database::{ArtifactRequest, CheckAvailablePackages, PackageDb};
pub use package_sources::{PackageSources, PackageSourcesBuilder};

pub use self::http::CacheMode;
pub use html::parse_hash;
