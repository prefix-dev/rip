//! Create a cache for storing locally built wheels
//! These are wheel files that are created when building a wheel from an sdist.
//! That uses cacache as a backend to actually store wheels.
//!
//! The wheels are stored via a layer of indirection. The key is a hash of the sdist content and the python interpreter version.
//! The value is the wheel file itself.
//!
//! So in cacache we have:
//! ┌──────────────────┐
//! │                  │
//! │                  │
//! │                  │
//! │   WheelCacheKey  │
//! │                  │
//! │                  │
//! │                  │
//! └──────────────────┘
//!          │   Metadata
//!          │
//! ┌───────▼────┐           ┌──────────────┐
//! │             │           │              │
//! │  WheelHash  │           │              │
//! │             ├─────────►│    Wheel     │
//! │             │           │              │
//! └─────────────┘           └──────────────┘
//!
//! So cacache stores the hashed wheel key and associated with this is with the content hash of the wheel
//! This way multiple WheelCacheKeys can point to the same wheel.
use crate::artifacts::Wheel;
use crate::python_env::PythonInterpreterVersion;
use crate::types::ArtifactFromSource;
use crate::types::{ArtifactFromBytes, WheelFilename};
use cacache::{Integrity, WriteOpts};
use rattler_digest::Sha256;
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::str::FromStr;

/// Wrapper around an API built on top of cacache
/// This is used to store wheels that are built from sdists
#[derive(Debug, Clone)]
pub struct WheelCache {
    // Path to the cache directory
    path: PathBuf,
}

#[derive(Debug)]
/// A key that can be used to retrieve a wheel from the cache
pub struct WheelCacheKey(String);

#[derive(Serialize, Deserialize, Debug)]
struct WheelKeyMetadata {
    wheel_filename: WheelFilename,
    integrity: String,
}

impl ToString for WheelCacheKey {
    /// Get WheelKey string representation without suffix
    fn to_string(&self) -> String {
        let mut parts = self.0.split(':');
        parts.nth(1).unwrap_or_default().to_owned()
    }
}

impl WheelCacheKey {
    /// Create a wheel key from bytes, will become '{prefix}:{hash_hexadecimal}'
    pub fn from_bytes(prefix: impl AsRef<str>, bytes: impl AsRef<[u8]>) -> Self {
        let hash = rattler_digest::compute_bytes_digest::<Sha256>(bytes);
        Self(format!("{}:{:x}", prefix.as_ref(), hash))
    }

    /// Create a wheel key from a prefix and a string, will become '{prefix}:{string}'
    pub fn new(prefix: impl AsRef<str>, key: impl AsRef<str>) -> Self {
        Self(format!("{}:{}", prefix.as_ref(), key.as_ref()))
    }

    /// Create a WheelCacheKey from an sdist and the python interpreter version
    pub fn from_sdist(
        sdist: &impl ArtifactFromSource,
        python_interpreter_version: &PythonInterpreterVersion,
    ) -> Result<WheelCacheKey, std::io::Error> {
        let hash = sdist.try_get_bytes()?;
        let hash = rattler_digest::compute_bytes_digest::<Sha256>(&hash);

        // Hash python version
        Ok(WheelCacheKey::new(
            "sdist",
            format!(
                "{:x}:v{}.{}",
                hash, python_interpreter_version.major, python_interpreter_version.minor,
            ),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WheelCacheError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Cacache(#[from] cacache::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("error constructing wheel")]
    WheelConstruction,
}

impl WheelCache {
    /// Create a new entry into the wheel cache
    /// **path** is the path to the cache directory
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// List wheels in the cache
    pub fn wheels(&self) -> impl Iterator<Item = serde_json::Result<WheelFilename>> {
        cacache::index::ls(&self.path)
            .filter_map(|index| index.ok())
            .map(|index| {
                serde_json::from_value::<WheelKeyMetadata>(index.metadata)
                    .map(|metadata| metadata.wheel_filename)
            })
    }

    /// Save wheel into cache
    fn save_wheel(&self, wheel_contents: &mut dyn Read) -> Result<Integrity, WheelCacheError> {
        // Write the wheel to the cache
        let mut writer = WriteOpts::new().open_hash_sync(&self.path)?;
        std::io::copy(wheel_contents, &mut writer)?;
        Ok(writer.commit()?)
    }

    /// Associate wheel with cache key
    pub fn associate_wheel(
        &self,
        key: &WheelCacheKey,
        wheel_name: WheelFilename,
        wheel: &mut dyn Read,
    ) -> Result<(), WheelCacheError> {
        // Save the wheel to the cache
        let wheel_integrity = self.save_wheel(wheel)?;
        let metadata = serde_json::to_value(WheelKeyMetadata {
            wheel_filename: wheel_name,
            integrity: wheel_integrity.to_string(),
        })?;
        // Associate with the integrity
        cacache::index::insert(
            &self.path,
            &key.0,
            WriteOpts::new()
                // This is just so the index entry is loadable.
                .integrity("sha256-deadbeef".parse().unwrap())
                .metadata(metadata),
        )?;

        Ok(())
    }

    /// Get wheel for key, returns None if it does not exist for this key
    pub fn wheel_for_key(
        &self,
        wheel_key: &WheelCacheKey,
    ) -> Result<Option<Wheel>, WheelCacheError> {
        // Find metadata for the key
        let metadata = cacache::index::find(&self.path, &wheel_key.0)?;

        if let Some(metadata) = metadata {
            // Find integrity associated with metadata
            let value: WheelKeyMetadata = serde_json::from_value(metadata.metadata)?;
            let integrity =
                Integrity::from_str(&value.integrity).map_err(cacache::Error::IntegrityError)?;

            // Find wheel associated with integrity
            let bytes = Cursor::new(cacache::read_hash_sync(&self.path, &integrity)?);
            let wheel = Wheel::from_bytes(value.wheel_filename, Box::new(bytes));

            // Need to do this to get out of miette::Result
            // TODO: change artifact to not use miette::Result?
            match wheel {
                Ok(wheel) => Ok(Some(wheel)),
                Err(_) => Err(WheelCacheError::WheelConstruction),
            }
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::types::WheelFilename;
    use crate::wheel_builder::wheel_cache::WheelCache;
    use std::path::Path;

    #[test]
    pub fn test_key() {
        let bytes = b"hello world";
        let key = super::WheelCacheKey::from_bytes("bla", bytes);
        insta::assert_debug_snapshot!(key, @r###"
        WheelCacheKey(
            "bla:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9",
        )
        "###);
    }

    #[test]
    pub fn save_retrieve_wheel() {
        let cache = WheelCache::new(tempfile::tempdir().unwrap().into_path());

        // Load the wheel file
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/wheels/purelib_and_platlib-1.0.0-cp38-cp38-linux_x86_64.whl");
        let wheel = fs_err::File::open(&path).unwrap();
        let wheel_filename = WheelFilename::from_filename(
            path.file_name().unwrap().to_str().unwrap(),
            &"purelib_and_platlib".parse().unwrap(),
        )
        .unwrap();

        // Associate wheel with another key
        // use a bit of random data here but we need to use
        // the sdist content hash in reality
        let key = super::WheelCacheKey::from_bytes("bla", "foo");
        cache
            .associate_wheel(&key, wheel_filename, &mut std::io::BufReader::new(wheel))
            .unwrap();

        // Get back the wheel
        // See if we have a value
        cache.wheel_for_key(&key).unwrap().unwrap();

        assert_eq!(cache.wheels().count(), 1);
    }
}
