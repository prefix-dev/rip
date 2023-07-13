use futures::{AsyncRead, AsyncSeek, AsyncSeekExt};
use miette::IntoDiagnostic;
use pin_project_lite::pin_project;
use std::io::SeekFrom;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio_util::compat::TokioAsyncReadCompatExt;

/// Keep retrying a certain IO function until it either succeeds or until it doesnt return
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

pin_project! {
    #[project = ReadMaybeSeekProj]
    pub enum ReadMaybeSeek {
       ReadOnly { #[pin] inner: Box<dyn AsyncRead + Unpin + Send> },
       Seekable { #[pin] inner: Box<dyn AsyncReadAndSeek + Unpin + Send> }
    }
}

pub trait AsyncReadAndSeek: AsyncRead + AsyncSeek {}
impl<T> AsyncReadAndSeek for T where T: AsyncRead + AsyncSeek {}

impl AsyncRead for ReadMaybeSeek {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        let project = self.project();
        match project {
            ReadMaybeSeekProj::ReadOnly { inner } => inner.poll_read(cx, buf),
            ReadMaybeSeekProj::Seekable { inner } => inner.poll_read(cx, buf),
        }
    }
}

impl ReadMaybeSeek {
    /// Returns an instance that is both readable and seekable by first streaming the contents to
    /// disk if required.
    pub async fn force_seek(self) -> miette::Result<Box<dyn AsyncReadAndSeek + Unpin + Send>> {
        Ok(match self {
            ReadMaybeSeek::Seekable { inner } => inner,
            ReadMaybeSeek::ReadOnly { mut inner } => {
                let mut tmp =
                    tokio::fs::File::from(tempfile::tempfile().into_diagnostic()?).compat();
                futures::io::copy(&mut inner, &mut tmp)
                    .await
                    .into_diagnostic()?;
                tmp.seek(SeekFrom::Start(0)).await.into_diagnostic()?;
                Box::new(tmp)
            }
        })
    }
}
