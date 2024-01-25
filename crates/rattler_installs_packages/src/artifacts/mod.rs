//! Module containing artifacts that can be resolved and installed.
mod sdist;

/// Module for working with PyPA wheels. Contains the [`Wheel`] type, and related functionality.
pub mod wheel;

pub use sdist::{SDist, SDistError, STree, SourceArtifact};
pub use wheel::Wheel;
