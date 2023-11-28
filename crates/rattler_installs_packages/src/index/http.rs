use super::file_store::FileLock;
use super::file_store::FileStore;
use super::package_database::NotCached;
use crate::seek_slice::SeekSlice;
use crate::utils::{ReadAndSeek, StreamingOrLocal};
use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use http::header::{ACCEPT, CACHE_CONTROL};
use http_cache_semantics::{AfterResponse, BeforeRequest, CachePolicy};
use miette::Diagnostic;
use reqwest::{header::HeaderMap, Client, Method};
use serde::{Deserialize, Serialize};
use std::io;
use std::io::{Read, Seek, SeekFrom, Write};
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;
use thiserror::Error;
use tokio_util::compat::FuturesAsyncReadCompatExt;
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
/// Different caching semantics that can be applied to a request.
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
    pub(crate) client: Client,
    http_cache: Arc<FileStore>,
    _hash_cache: Arc<FileStore>,
}

#[derive(Debug, Error, Diagnostic)]
pub enum HttpRequestError {
    #[error(transparent)]
    HttpError(#[from] reqwest::Error),

    #[error(transparent)]
    IoError(#[from] io::Error),

    #[error(transparent)]
    #[diagnostic(transparent)]
    NotCached(#[from] NotCached),
}

impl Http {
    /// Constructs a new instance.
    pub fn new(client: Client, http_cache: FileStore, hash_cache: FileStore) -> Self {
        Http {
            client,
            http_cache: Arc::new(http_cache),
            _hash_cache: Arc::new(hash_cache),
        }
    }

    /// Performs a single request caching the result internally if requested.
    pub async fn request(
        &self,
        url: Url,
        method: Method,
        headers: HeaderMap,
        cache_mode: CacheMode,
    ) -> Result<http::Response<StreamingOrLocal>, HttpRequestError> {
        tracing::info!(url=%url, cache_mode=?cache_mode, "executing request");

        // Construct a request using the reqwest client.
        let request = self
            .client
            .request(method.clone(), url.clone())
            .headers(headers.clone())
            .build()?;

        if cache_mode == CacheMode::NoStore {
            let mut response =
                convert_response(self.client.execute(request).await?.error_for_status()?)
                    .map(body_to_streaming_or_local);

            // Add the `CacheStatus` to the response
            response.extensions_mut().insert(CacheStatus::Uncacheable);

            Ok(response)
        } else {
            let key = key_for_request(&url, method, &headers);
            let lock = self.http_cache.lock(&key.as_slice())?;

            if let Some((old_policy, final_url, old_body)) = lock
                .reader()
                .and_then(|reader| read_cache(reader.detach_unlocked()).ok())
            {
                match old_policy.before_request(&request, SystemTime::now()) {
                    BeforeRequest::Fresh(parts) => {
                        tracing::debug!(url=%url, "is fresh");
                        let mut response = http::Response::from_parts(
                            parts,
                            StreamingOrLocal::Local(Box::new(old_body)),
                        );
                        response.extensions_mut().insert(CacheStatus::Fresh);
                        response.extensions_mut().insert(final_url);
                        Ok(response)
                    }
                    BeforeRequest::Stale {
                        request: new_parts,
                        matches: _,
                    } => {
                        if cache_mode == CacheMode::OnlyIfCached {
                            return Err(NotCached.into());
                        }

                        // Perform the request with the new headers to determine if the cache is up
                        // to date or not.
                        let request = convert_request(self.client.clone(), new_parts)?;
                        let response = self
                            .client
                            .execute(request.try_clone().expect("clone of request cannot fail"))
                            .await?;
                        let final_url = response.url().clone();

                        // Determine what to do based on the response headers.
                        match old_policy.after_response(&request, &response, SystemTime::now()) {
                            AfterResponse::NotModified(new_policy, new_parts) => {
                                tracing::debug!(url=%url, "stale, but not modified");
                                let new_body = fill_cache(&new_policy, &final_url, old_body, lock)?;
                                Ok(make_response(
                                    new_parts,
                                    StreamingOrLocal::Local(Box::new(new_body)),
                                    CacheStatus::StaleButValidated,
                                    final_url,
                                ))
                            }
                            AfterResponse::Modified(new_policy, parts) => {
                                tracing::debug!(url=%url, "stale, but *and* modified");
                                drop(old_body);
                                let new_body = if new_policy.is_storable() {
                                    let new_body = fill_cache_async(
                                        &new_policy,
                                        &final_url,
                                        response.bytes_stream(),
                                        lock,
                                    )
                                    .await?;
                                    StreamingOrLocal::Local(Box::new(new_body))
                                } else {
                                    lock.remove()?;
                                    body_to_streaming_or_local(response.bytes_stream())
                                };
                                Ok(make_response(
                                    parts,
                                    new_body,
                                    CacheStatus::StaleAndChanged,
                                    final_url,
                                ))
                            }
                        }
                    }
                }
            } else {
                if cache_mode == CacheMode::OnlyIfCached {
                    return Err(NotCached.into());
                }

                let response = self
                    .client
                    .execute(request.try_clone().expect("failed to clone request?"))
                    .await?
                    .error_for_status()?;
                let final_url = response.url().clone();
                let response = convert_response(response);

                let new_policy = CachePolicy::new(&request, &response);
                let (parts, body) = response.into_parts();

                let new_body = if new_policy.is_storable() {
                    let new_body = fill_cache_async(&new_policy, &final_url, body, lock).await?;
                    StreamingOrLocal::Local(Box::new(new_body))
                } else {
                    lock.remove()?;
                    body_to_streaming_or_local(body)
                };
                Ok(make_response(parts, new_body, CacheStatus::Miss, final_url))
            }
        }
    }
}

/// Constructs a `http::Response` from parts.
fn make_response(
    parts: http::response::Parts,
    body: StreamingOrLocal,
    cache_status: CacheStatus,
    url: Url,
) -> http::Response<StreamingOrLocal> {
    let mut response = http::Response::from_parts(parts, body);
    response.extensions_mut().insert(cache_status);
    response.extensions_mut().insert(url);
    response
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
    key.extend(uri.into_bytes());

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

/// Read a HTTP cached value from a readable stream.
fn read_cache<R>(mut f: R) -> std::io::Result<(CachePolicy, Url, impl ReadAndSeek)>
where
    R: Read + Seek,
{
    let data: CacheData = ciborium::de::from_reader(&mut f)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let start = f.stream_position()?;
    let end = f.seek(SeekFrom::End(0))?;
    let mut body = SeekSlice::new(f, start, end)?;
    body.rewind()?;
    Ok((data.policy, data.url, body))
}

#[derive(Serialize, Deserialize)]
struct CacheData {
    policy: CachePolicy,
    url: Url,
}

/// Fill the cache with the
fn fill_cache<R: Read>(
    policy: &CachePolicy,
    url: &Url,
    mut body: R,
    handle: FileLock,
) -> Result<impl Read + Seek, std::io::Error> {
    let mut cache_writer = handle.begin()?;
    ciborium::ser::into_writer(
        &CacheData {
            policy: policy.clone(),
            url: url.clone(),
        },
        &mut cache_writer,
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let body_start = cache_writer.stream_position()?;
    std::io::copy(&mut body, &mut cache_writer)?;
    drop(body);
    let body_end = cache_writer.stream_position()?;
    let cache_entry = cache_writer.commit()?.detach_unlocked();
    SeekSlice::new(cache_entry, body_start, body_end)
}

/// Fill the cache with the
async fn fill_cache_async(
    policy: &CachePolicy,
    url: &Url,
    mut body: impl Stream<Item = reqwest::Result<Bytes>> + Send + Unpin,
    handle: FileLock,
) -> Result<impl Read + Seek, std::io::Error> {
    let mut cache_writer = handle.begin()?;
    ciborium::ser::into_writer(
        &CacheData {
            policy: policy.clone(),
            url: url.clone(),
        },
        &mut cache_writer,
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let body_start = cache_writer.stream_position()?;

    while let Some(bytes) = body.next().await {
        cache_writer.write_all(
            bytes
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
                .as_ref(),
        )?;
    }

    let body_end = cache_writer.stream_position()?;
    let cache_entry = cache_writer.commit()?.detach_unlocked();
    SeekSlice::new(cache_entry, body_start, body_end)
}

/// Converts from a `http::request::Parts` into a `reqwest::Request`.
fn convert_request(
    client: Client,
    parts: http::request::Parts,
) -> Result<reqwest::Request, reqwest::Error> {
    client
        .request(
            parts.method,
            Url::from_str(&parts.uri.to_string()).expect("uris should be the same"),
        )
        .headers(parts.headers)
        .version(parts.version)
        .build()
}

fn convert_response(
    mut response: reqwest::Response,
) -> http::response::Response<impl Stream<Item = reqwest::Result<Bytes>>> {
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
    extensions.insert(response.url().clone());

    builder
        .body(response.bytes_stream())
        .expect("building should never fail")
}

fn body_to_streaming_or_local(
    stream: impl Stream<Item = reqwest::Result<Bytes>> + Send + Unpin + 'static,
) -> StreamingOrLocal {
    StreamingOrLocal::Streaming(Box::new(
        stream
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
            .into_async_read()
            .compat(),
    ))
}
