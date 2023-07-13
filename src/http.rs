use crate::seek_slice::SeekSlice;
use crate::{utils::ReadMaybeSeek, FileStore};
use futures::TryStreamExt;
use http::header::{ACCEPT, CACHE_CONTROL};
use http_cache_semantics::CachePolicy;
use miette::{Diagnostic, IntoDiagnostic};
use reqwest::{header::HeaderMap, Client, Method};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use thiserror::Error;
use url::Url;

// Attached to HTTP responses, to make testing easier
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CacheStatus {
    Fresh,
    StaleButValidated,
    StaleAndChanged,
    Miss,
    Uncacheable,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CacheMode {
    /// Apply regular HTTP caching semantics
    Default,
    /// If we have a valid cache entry, return it; otherwise return Err(NotCached)
    OnlyIfCached,
    /// Don't look in cache, and don't write to cache
    NoStore,
}

#[derive(Debug, Clone)]
pub struct Http {
    client: Client,
    http_cache: Arc<FileStore>,
    hash_cache: Arc<FileStore>,
}

#[derive(Debug, Error, Diagnostic)]
pub enum HttpError {
    #[error(transparent)]
    HttpError(#[from] reqwest::Error),
}

impl Http {
    /// Constructs a new instance.
    pub fn new(client: Client, http_cache: FileStore, hash_cache: FileStore) -> Self {
        Http {
            client,
            http_cache: Arc::new(http_cache),
            hash_cache: Arc::new(hash_cache),
        }
    }

    /// Performs a single request caching the result internally if requested.
    pub async fn request(
        &self,
        url: Url,
        method: Method,
        headers: HeaderMap,
        cache_mode: CacheMode,
    ) -> Result<http::Response<ReadMaybeSeek>, HttpError> {
        if cache_mode == CacheMode::NoStore {
            // Construct a request using the reqwest client.
            let request = self.client.request(method, url).headers(headers).build()?;
            let mut response = self.client.execute(request).await?.error_for_status()?;
            let mut builder = http::Response::builder()
                .version(response.version())
                .status(response.status());

            // Take the headers from the response
            let headers = builder.headers_mut().unwrap();
            *headers = std::mem::take(response.headers_mut());
            std::mem::swap(response.headers_mut(), headers);

            // Take the extensions from the response
            let extensions = builder.extensions_mut().unwrap();
            *extensions = std::mem::take(response.extensions_mut());
            extensions.insert(CacheStatus::Uncacheable);

            Ok(builder
                .body(ReadMaybeSeek::ReadOnly {
                    inner: Box::new(
                        response
                            .bytes_stream()
                            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                            .into_async_read(),
                    ),
                })
                .expect("building should never fail"))
        } else {
            let key = key_for_request(&url, method, &headers);
            let lock = self.http_cache.lock(&key.as_slice())?;

            if let Some(reader) = lock.reader() {}
        }
    }
}

/// Construct a key from an http request that we can use to store and retrieve stuff from a
/// [`FileStore`].
fn key_for_request(url: &Url, method: Method, headers: &HeaderMap) -> Vec<u8> {
    let mut key: Vec<u8> = Default::default();
    let method = method.to_string().into_bytes();
    key.extend(method.len().to_le_bytes());
    key.extend(method);

    // Add the url to the key but ignore the fragments.
    let mut url = url.clone();
    url.set_fragment(None);
    let uri = url.to_string();
    key.extend(uri.len().to_le_bytes());
    key.extend(uri);

    // Add specific headers if they are added to the request
    for header_name in [ACCEPT, CACHE_CONTROL] {
        if let Some(value) = headers.get(&header_name) {
            let header_name = header_name.to_string().into_bytes();
            key.extend(header_name.len().to_le_bytes());
            key.extend(header_name);

            let header_value = value.as_bytes().to_vec();
            key.extend(header_value.len().to_le_bytes());
            key.extend(header_value);
        }
    }

    key
}

fn read_cache<R>(mut f: R) -> std::io::Result<(CachePolicy, impl Read + Seek)>
where
    R: Read + Seek,
{
    let policy: CachePolicy = ciborium::de::from_reader(&mut f)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let start = f.stream_position()?;
    let end = f.seek(SeekFrom::End(0))?;
    let mut body = SeekSlice::new(f, start, end)?;
    body.rewind()?;
    Ok((policy, body))
}
