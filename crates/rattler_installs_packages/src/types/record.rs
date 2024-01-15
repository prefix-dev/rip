//! Defines the [`Record`] struct which holds the information stored in a `RECORD` file which is
//! found in a wheel archive or installation.

use itertools::Itertools;
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

/// Represents the RECORD file found in a wheels .dist-info folder.
///
/// See <https://www.python.org/dev/peps/pep-0376/#record> for more information about the format.
#[derive(Debug, Clone)]
pub struct Record {
    entries: Vec<RecordEntry>,
}

/// A single entry in a `RECORD` file
#[derive(Debug, Deserialize, Serialize, PartialOrd, PartialEq, Ord, Eq, Clone)]
pub struct RecordEntry {
    /// The path relative to the root of the environment or archive
    pub path: String,

    /// The hash of the file. Usually this is a Sha256 hash.
    pub hash: Option<String>,

    /// The size of the file in bytes.
    pub size: Option<u64>,
}

impl Record {
    /// Reads the contents of a `RECORD` file from disk.
    pub fn from_path(path: &Path) -> csv::Result<Self> {
        Self::from_reader(fs_err::File::open(path)?)
    }

    /// Reads the contents of a `RECORD` file from a reader.
    pub fn from_reader(reader: impl Read) -> csv::Result<Self> {
        Ok(Self {
            entries: csv::ReaderBuilder::new()
                .has_headers(false)
                .escape(Some(b'"'))
                .from_reader(reader)
                .deserialize()
                .collect::<Result<Vec<RecordEntry>, csv::Error>>()?,
        })
    }

    /// Write to a `RECORD` file on disk
    pub fn write_to_path(&self, path: &Path) -> csv::Result<()> {
        let mut record_writer = csv::WriterBuilder::new()
            .has_headers(false)
            .escape(b'"')
            .from_path(path)?;
        for entry in self.entries.iter().sorted() {
            record_writer.serialize(entry)?;
        }
        Ok(())
    }

    /// Returns an iterator over the entries in this instance.
    pub fn iter(&self) -> std::slice::Iter<RecordEntry> {
        self.entries.iter()
    }
}

impl IntoIterator for Record {
    type Item = RecordEntry;
    type IntoIter = std::vec::IntoIter<RecordEntry>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl FromIterator<RecordEntry> for Record {
    fn from_iter<T: IntoIterator<Item = RecordEntry>>(iter: T) -> Self {
        Self {
            entries: FromIterator::from_iter(iter),
        }
    }
}
