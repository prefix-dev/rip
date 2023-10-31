use futures::{AsyncRead, AsyncReadExt, AsyncSeekExt};
use include_dir::{include_dir, Dir};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use thiserror::Error;
use tokio_util::compat::TokioAsyncReadCompatExt;
use url::Url;
use which::which;

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

/// Represents either data coming from the network in an async fashion or a local thing on disk.
/// We only use async for the network stuff, the local filesystem doesn't really benefit from it.
pub enum StreamingOrLocal {
    Streaming(Box<dyn AsyncRead + Unpin + Send>),
    Local(Box<dyn ReadAndSeek + Send>),
}

pub trait ReadAndSeek: Read + Seek {}
impl<T> ReadAndSeek for T where T: Read + Seek {}

impl StreamingOrLocal {
    /// Returns an instance that is both readable and seekable by first streaming the contents to
    /// disk if required.
    pub async fn force_local(self) -> std::io::Result<Box<dyn ReadAndSeek + Send>> {
        Ok(match self {
            StreamingOrLocal::Local(stream) => stream,
            StreamingOrLocal::Streaming(mut stream) => {
                let mut tmp = tokio::fs::File::from(tempfile::tempfile()?).compat();
                futures::io::copy(&mut stream, &mut tmp).await?;
                tmp.seek(SeekFrom::Start(0)).await?;
                Box::new(tmp.into_inner().into_std().await)
            }
        })
    }

    pub async fn read_to_end(self, bytes: &mut Vec<u8>) -> std::io::Result<usize> {
        match self {
            StreamingOrLocal::Streaming(mut streaming) => streaming.read_to_end(bytes).await,
            StreamingOrLocal::Local(mut local) => {
                match tokio::task::spawn_blocking(move || {
                    let mut bytes = Vec::new();
                    local.read_to_end(&mut bytes).map(|_| bytes)
                })
                .await
                {
                    Ok(Ok(result)) => {
                        *bytes = result;
                        Ok(bytes.len())
                    }
                    Ok(Err(err)) => Err(err),
                    Err(err) => {
                        if let Ok(panic) = err.try_into_panic() {
                            std::panic::resume_unwind(panic)
                        }
                        Err(std::io::ErrorKind::Interrupted.into())
                    }
                }
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

/// Error that can occur while finding the python executable.
#[derive(Debug, Error)]
pub enum FindPythonError {
    #[error("could not find python executable")]
    NotFound,
}

/// Try to find the python executable in the current environment.
pub fn python_executable() -> Result<PathBuf, FindPythonError> {
    let python = which("python").map_err(|_| FindPythonError::NotFound);

    // When installed with homebrew on macOS, the python3 executable is called `python3` instead
    #[cfg(target_os = "macos")]
    let python = if python.is_err() {
        which("python3").map_err(|_| FindPythonError::NotFound)
    } else {
        python
    };
    python
}

pub(crate) static VENDORED_PACKAGING_DIR: Dir<'_> =
    include_dir!("$CARGO_MANIFEST_DIR/vendor/packaging/");
