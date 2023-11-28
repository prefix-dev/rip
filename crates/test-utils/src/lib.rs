use rattler_digest::Sha256;
use reqwest::blocking::Client;
use std::time::Instant;
use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
pub enum Error {
    #[error("invalid url: {0}")]
    InvalidUrl(String),

    #[error("could not determine the systems cache directory")]
    FailedToDetermineCacheDir,

    #[error("failed to create temporary file")]
    FailedToCreateTemporaryFile(#[source] std::io::Error),

    #[error("failed to acquire cache lock")]
    FailedToAcquireCacheLock(#[source] std::io::Error),

    #[error("failed to create cache dir {0}")]
    FailedToCreateCacheDir(String, #[source] std::io::Error),

    #[error(transparent)]
    HttpError(#[from] reqwest::Error),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("hash mismatch. Expected: {0}, Actual: {1}")]
    HashMismatch(String, String),
}

/// Returns a [`Client`] that can be shared between all requests.
fn reqwest_client() -> Client {
    static CLIENT: OnceLock<Mutex<Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| Mutex::new(Client::new()))
        .lock()
        .unwrap()
        .clone()
}

/// Returns the cache directory to use for storing cached files
fn cache_dir() -> Result<PathBuf, Error> {
    Ok(dirs::cache_dir()
        .ok_or(Error::FailedToDetermineCacheDir)?
        .join("rip/tests/cache/"))
}

/// Downloads a file to a semi-temporary location that can be used for testing.
pub async fn download_and_cache_file_async(
    url: Url,
    expected_sha256: &str,
) -> Result<PathBuf, Error> {
    let expected_sha256 = expected_sha256.to_owned();
    tokio::task::spawn_blocking(move || download_and_cache_file(url, &expected_sha256))
        .await
        .unwrap()
}

/// Downloads a file to a semi-temporary location that can be used for testing.
pub fn download_and_cache_file(url: Url, expected_sha256: &str) -> Result<PathBuf, Error> {
    // Acquire a lock to the cache directory
    let cache_dir = cache_dir()?;

    // Determine the extension of the file
    let filename = url
        .path_segments()
        .into_iter()
        .flatten()
        .last()
        .ok_or_else(|| Error::InvalidUrl(String::from("missing filename")))?;

    // Determine the final location of the file
    let final_parent_dir = cache_dir.join(expected_sha256);
    let final_path = final_parent_dir.join(filename);

    // Ensure the cache directory exists
    std::fs::create_dir_all(&final_parent_dir)
        .map_err(|e| Error::FailedToCreateCacheDir(final_parent_dir.display().to_string(), e))?;

    // Acquire the lock on the cache directory
    let mut lock = fslock::LockFile::open(&cache_dir.join(".lock"))
        .map_err(Error::FailedToAcquireCacheLock)?;
    lock.lock_with_pid()
        .map_err(Error::FailedToAcquireCacheLock)?;

    // Check if the file is already there
    if final_path.is_file() {
        return Ok(final_path);
    }

    eprintln!("Downloading {} to {}", url, final_path.display());
    let start_download = Instant::now();

    // Construct a temporary file to which we will write the file
    let tempfile = tempfile::NamedTempFile::new_in(&final_parent_dir)
        .map_err(Error::FailedToCreateTemporaryFile)?;

    // Execute the download request
    let mut response = reqwest_client().get(url).send()?.error_for_status()?;

    // Compute the hash while downloading
    let mut writer = rattler_digest::HashingWriter::<_, Sha256>::new(tempfile);
    std::io::copy(&mut response, &mut writer)?;
    let (tempfile, hash) = writer.finalize();

    // Check if the hash matches
    let actual_hash = format!("{hash:x}");
    if actual_hash != expected_sha256 {
        return Err(Error::HashMismatch(expected_sha256.to_owned(), actual_hash));
    }

    // Write the file to its final destination
    tempfile.persist(&final_path).map_err(|e| e.error)?;

    let end_download = Instant::now();
    eprintln!(
        "Finished download in {}s",
        (end_download - start_download).as_secs_f32()
    );

    Ok(final_path)
}
