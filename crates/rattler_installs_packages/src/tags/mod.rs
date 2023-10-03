//! Wheels encode the Python interpreter, ABI, and platform that they support in their filenames
//! using platform compatibility tags. This module provides support for discovering what tags the
//! running Python interpreter supports and determining if a wheel is compatible with a set of tags.

mod from_env;

use indexmap::IndexSet;
use std::fmt::{Debug, Display, Formatter};

/// A representation of a tag triple for a wheel.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct WheelTag {
    /// The interpreter name, e.g. "py"
    pub interpreter: String,

    /// The ABI that a wheel supports, e.g. "cp37m"
    pub abi: String,

    /// The OS/platform the wheel supports, e.g. "win_am64".
    pub platform: String,
}

impl Display for WheelTag {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}-{}", &self.interpreter, &self.abi, &self.platform)
    }
}

/// Contains an ordered set of platform tags with which compatibility of wheels can be determined.
#[derive(Debug, Clone)]
pub struct WheelTags {
    tags: IndexSet<WheelTag>,
}

impl WheelTags {
    /// Returns an iterator over the supported tags.
    pub fn tags(&self) -> impl Iterator<Item = &'_ WheelTag> + '_ {
        self.tags.iter()
    }

    /// Determines the compatibility of the specified tag with the tags in this instance. Returns
    /// `None` if the specified tag is not compatible with any of the tags in this instance. Returns
    /// `Some(i)` where `i` indicates the compatibility level. The higher the number the more
    /// specific the tag is to the platform. The wheel artifact with the highest number should be
    /// preferred over others.
    pub fn compatibility(&self, tag: &WheelTag) -> Option<i32> {
        self.tags.get_index_of(tag).map(|score| -(score as i32))
    }

    /// Returns if the specified tag is compatible with this set.
    pub fn is_compatible(&self, tag: &WheelTag) -> bool {
        self.tags.contains(tag)
    }
}

impl FromIterator<WheelTag> for WheelTags {
    fn from_iter<T: IntoIterator<Item = WheelTag>>(iter: T) -> Self {
        Self {
            tags: FromIterator::from_iter(iter),
        }
    }
}
