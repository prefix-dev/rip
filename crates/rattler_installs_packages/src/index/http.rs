use super::file_store::FileLock;
use super::file_store::FileStore;
use super::package_database::NotCached;
use crate::utils::{ReadAndSeek, SeekSlice, StreamingOrLocal};
use bytes::Bytes;
use futures::{Stream, StreamExt, TryStreamExt};
use http_cache_semantics::{AfterResponse, BeforeRequest, CachePolicy};
use miette::Diagnostic;
use reqwest::header::{ACCEPT, CACHE_CONTROL};
use reqwest::{header::HeaderMap, Method};
use reqwest_middleware::ClientWithMiddleware;
use serde::{Deserialize, Serialize};
use core::panic;
use std::io;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::{Read, Seek, SeekFrom, Write};
use std::mem;
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
    pub(crate) client: ClientWithMiddleware,
    http_cache: Arc<FileStore>,
}

#[derive(Debug, Error, Diagnostic)]
pub enum HttpRequestError {
    #[error(transparent)]
    HttpError(#[from] reqwest_middleware::Error),

    #[error(transparent)]
    IoError(#[from] io::Error),

    #[error(transparent)]
    #[diagnostic(transparent)]
    NotCached(#[from] NotCached),
}

impl From<reqwest::Error> for HttpRequestError {
    fn from(e: reqwest::Error) -> Self {
        Self::HttpError(e.into())
    }
}

impl Http {
    /// Constructs a new instance.
    pub fn new(client: ClientWithMiddleware, http_cache: FileStore) -> Self {
        Http {
            client,
            http_cache: Arc::new(http_cache),
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
            let lock = self.http_cache.lock(&key.as_slice()).await?;

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
                                println!("ITS STALE BUT NOT MODIFIED");
                                // let new_body = fill_cache(&new_policy, &final_url, old_body, lock)?;
                                Ok(make_response(
                                    new_parts,
                                    StreamingOrLocal::Local(Box::new(old_body)),
                                    CacheStatus::StaleButValidated,
                                    final_url,
                                ))
                            }
                            AfterResponse::Modified(new_policy, parts) => {
                                tracing::debug!(url=%url, "stale, but *and* modified");
                                drop(old_body);
                                println!("ITS STALE AND MODIFIED");
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
                println!("I FILL CACHE ASYNC BECAUSE I DONT HAVE METADATA?");
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
    println!("I TRY TO READ DATA?");

    let mut buff_reader = BufReader::new(&mut f);
    // let mut buf = vec![];
    let mut buffer = [0; 8];

    let ciborium_end = buff_reader.read_exact(&mut buffer).unwrap();

    // buff_reader.seek(SeekFrom::Start(buffer))).unwrap();
    // buff_reader.rewind().unwrap();
    
    // buff_reader.rewind().unwrap();


    let data: CacheData = ciborium::de::from_reader(buff_reader).unwrap();

    println!("I READ DATA?");
    
    // ciborium::ser::into_writer(value, writer))
    /// 335
    // let size = mem::size_of_val(&data);
// 
    // println!("SIZE IS {:?}", size);
    
    // let start = f.stream_position()?;

    let start = u64::from_le_bytes(buffer);
    let end = f.seek(SeekFrom::End(0))?;

    println!("START AND END IS {:?} {:?}", start, end);

    
    let mut body = SeekSlice::new(f, start, end)?;
    body.rewind()?;

    println!("I RETURN {:?} {:?}", data.policy, data.url);

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
    let mut buf_cached_writer = BufWriter::new(cache_writer);
    
    ciborium::ser::into_writer(
        &CacheData {
            policy: policy.clone(),
            url: url.clone(),
        },
        &mut buf_cached_writer,
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    
    
    
    
    let body_start = buf_cached_writer.stream_position()?;

    
    std::io::copy(&mut body, &mut buf_cached_writer)?;
    drop(body);
    let body_end = buf_cached_writer.stream_position()?;
    let cache_entry = buf_cached_writer.into_inner()?.commit()?.detach_unlocked();
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
    let mut buf_cache_writer = BufWriter::new(cache_writer);

        
    buf_cache_writer.rewind().unwrap();
    let file_contents_base64: [u8; 8] = [0; 8];
    buf_cache_writer.write_all(&file_contents_base64).unwrap();
    
    ciborium::ser::into_writer(
        &CacheData {
            policy: policy.clone(),
            url: url.clone(),
        },
        &mut buf_cache_writer,
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let body_start = buf_cache_writer.stream_position()?;


    buf_cache_writer.rewind().unwrap();

    let body_le_bytes = body_start.to_le_bytes();

    let file_contents_base64  = body_le_bytes.as_slice();
    let written = buf_cache_writer.write_all(&file_contents_base64).unwrap();
    
    buf_cache_writer.seek(SeekFrom::Start(body_start)).unwrap();



    while let Some(bytes) = body.next().await {
        buf_cache_writer.write_all(
            bytes
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
                .as_ref(),
        )?;
    }

    let body_end = buf_cache_writer.stream_position()?;
    let Ok(inner) = buf_cache_writer.into_inner() else {
        panic!("aa")
    } ;

    let cache_entry = inner.commit()?.detach_unlocked();
    println!("I WROTE {:?} {:?}", body_start, body_end);
    SeekSlice::new(cache_entry, body_start, body_end) 


    
}

/// Converts from a `http::request::Parts` into a `reqwest::Request`.
fn convert_request(
    client: ClientWithMiddleware,
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
