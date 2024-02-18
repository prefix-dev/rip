use crate::artifacts::wheel::{find_dist_info_metadata, WheelVitalsError};
use crate::types::{WheelCoreMetadata, WheelFilename};
use async_http_range_reader::AsyncHttpRangeReader;
use async_zip::base::read::seek::ZipFileReader;
use tokio_util::compat::TokioAsyncReadCompatExt;

/// Reads the metadata from a wheel by only reading parts of the wheel zip.
///
/// This function uses [`AsyncHttpRangeReader`] which allows reading parts of a file by performing
/// http range requests. First the end of the file is read to index the central directory of the
/// zip. This provides an index into the file which allows accessing the exact bytes that contain
/// the METADATA file.
pub(crate) async fn lazy_read_wheel_metadata(
    name: &WheelFilename,
    stream: &mut AsyncHttpRangeReader,
) -> Result<(Vec<u8>, WheelCoreMetadata), WheelVitalsError> {
    // Make sure we have the back part of the stream.
    // Best guess for the central directory size inside the zip
    const CENTRAL_DIRECTORY_SIZE: u64 = 16384;
    // Because the zip index is at the back
    stream
        .prefetch(stream.len().saturating_sub(CENTRAL_DIRECTORY_SIZE)..stream.len())
        .await;

    // Construct a zip reader to uses the stream.
    let mut reader = ZipFileReader::new(stream.compat())
        .await
        .map_err(|err| WheelVitalsError::from_async_zip("/".into(), err))?;

    // Collect all top-level filenames
    let file_names = reader
        .file()
        .entries()
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| Some(((idx, entry), entry.filename().as_str().ok()?)));

    // Determine the name of the dist-info directory
    let ((metadata_idx, metadata_entry), dist_info_prefix) =
        find_dist_info_metadata(name, file_names)?;
    let metadata_path = format!("{dist_info_prefix}.dist-info/METADATA");

    // Get the size of the entry plus the header + size of the filename. We should also actually
    // include bytes for the extra fields but we don't have that information.
    let offset = metadata_entry.header_offset();
    let size = metadata_entry.compressed_size()
        + 30 // Header size in bytes
        + metadata_entry.filename().as_bytes().len() as u64;

    // The zip archive uses as BufReader which reads in chunks of 8192. To ensure we prefetch
    // enough data we round the size up to the nearest multiple of the buffer size.
    let buffer_size = 8192;
    let size = ((size + buffer_size - 1) / buffer_size) * buffer_size;

    // Fetch the bytes from the zip archive that contain the requested file.
    reader
        .inner_mut()
        .get_mut()
        .prefetch(offset..offset + size)
        .await;

    // Read the contents of the metadata.json file
    let mut contents = Vec::new();
    reader
        .reader_with_entry(metadata_idx)
        .await
        .map_err(|e| WheelVitalsError::from_async_zip(metadata_path.clone(), e))?
        .read_to_end_checked(&mut contents)
        .await
        .map_err(|e| WheelVitalsError::from_async_zip(metadata_path, e))?;

    // Parse the wheel data
    let metadata = WheelCoreMetadata::try_from(contents.as_slice())?;

    let stream = reader.into_inner().into_inner();
    let ranges = stream.requested_ranges().await;
    let total_bytes_fetched: u64 = ranges.iter().map(|r| r.end - r.start).sum();
    tracing::debug!(
        "fetched {} ranges, total of {} bytes, total file length {} ({}%)",
        ranges.len(),
        total_bytes_fetched,
        stream.len(),
        (total_bytes_fetched as f64 / stream.len() as f64 * 100000.0).round() / 100.0
    );

    Ok((contents, metadata))
}
