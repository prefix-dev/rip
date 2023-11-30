//! Module containing artifacts that can be resolved and installed.
mod sdist;

mod byte_code_compiler;
/// Module for working with PyPA wheels. Contains the [`Wheel`] type, and related functionality.
pub mod wheel;

pub use sdist::SDist;
pub use wheel::Wheel;
