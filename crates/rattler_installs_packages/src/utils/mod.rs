mod read_and_seek;
mod streaming_or_local;

mod seek_slice;
#[cfg(test)]
mod test;
#[cfg(test)]
pub use test::{get_package_db, setup};

use std::path::{Component, Path, PathBuf};

use include_dir::{include_dir, Dir};
use url::Url;

pub use read_and_seek::ReadAndSeek;
pub use streaming_or_local::StreamingOrLocal;

pub use seek_slice::SeekSlice;

/// Keep retrying a certain IO function until it either succeeds or until it doesn't return
/// [`std::io::ErrorKind::Interrupted`].
pub fn retry_interrupted<F, T>(mut f: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    loop {
        match f() {
            Ok(result) => return Ok(result),
            Err(err) if err.kind() != std::io::ErrorKind::Interrupted => {
                return Err(err);
            }
            _ => {
                // Otherwise keep looping!
            }
        }
    }
}

/// Normalize url according to pip standards
pub fn normalize_index_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    url
}

pub(crate) static VENDORED_PACKAGING_DIR: Dir<'_> =
    include_dir!("$CARGO_MANIFEST_DIR/vendor/packaging/");

/// Normalize path (remove .. and . components)
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();

    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {} // Do nothing for current directory (.)
            _ => normalized.push(component.as_os_str()),
        }
    }

    normalized
}
