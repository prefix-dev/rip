use std::io::{Read, Seek};

/// Defines that a type can be read and seeked. This trait has a blanket implementation for any type
/// that implements both [`Read`] and [`Seek`].
pub trait ReadAndSeek: Read + Seek {}

impl<T> ReadAndSeek for T where T: Read + Seek {}
