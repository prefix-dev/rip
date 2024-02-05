//! Module containing artifacts that can be resolved and installed.
mod sdist;

mod stree;
/// Module for working with PyPA wheels. Contains the [`Wheel`] type, and related functionality.
pub mod wheel;

pub use sdist::{SDist, SourceArtifact};
pub use stree::STree;
pub use wheel::Wheel;
