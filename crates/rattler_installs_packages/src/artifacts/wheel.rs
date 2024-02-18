use crate::types::HasArtifactName;
use crate::{
    types::{
        ArtifactFromBytes, NormalizedPackageName, PackageName, RFC822ish, WheelCoreMetaDataError,
        WheelCoreMetadata, WheelFilename,
    },
    utils::ReadAndSeek,
};
use fs_err as fs;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use pep440_rs::Version;
use std::{
    borrow::Cow,
    ffi::OsStr,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;
use zip::{result::ZipError, ZipArchive};

/// A wheel file (`.whl`) in its archived form that is stored somewhere on disk.
pub struct Wheel {
    /// Name of wheel
    pub name: WheelFilename,

    pub(crate) archive: Mutex<ZipArchive<Box<dyn ReadAndSeek + Send>>>,
}

impl HasArtifactName for Wheel {
    type Name = WheelFilename;

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

impl ArtifactFromBytes for Wheel {
    fn from_bytes(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        Ok(Self {
            name,
            archive: Mutex::new(ZipArchive::new(bytes).into_diagnostic()?),
        })
    }
}

impl Wheel {
    /// Open a wheel by reading a `.whl` file on disk.
    pub fn from_path(
        path: &Path,
        normalized_package_name: &NormalizedPackageName,
    ) -> miette::Result<Self> {
        let file_name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| miette::miette!("path does not contain a filename"))?;
        let wheel_name =
            WheelFilename::from_filename(file_name, normalized_package_name).into_diagnostic()?;
        let file = fs::File::open(path).into_diagnostic()?;
        Self::from_bytes(wheel_name, Box::new(file))
    }

    /// Create a wheel from URL and content.
    pub fn from_url_and_bytes(
        url: &str,
        normalized_package_name: &NormalizedPackageName,
        bytes: Box<dyn ReadAndSeek + Send>,
    ) -> miette::Result<Self> {
        let url_path = PathBuf::from_str(url).into_diagnostic()?;
        let file_name = url_path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| {
                miette::miette!("path {:?} does not contain a wheel filename", url_path)
            })?;
        let wheel_filename =
            WheelFilename::from_filename(file_name, normalized_package_name).into_diagnostic()?;

        Self::from_bytes(wheel_filename.clone(), Box::new(bytes))
    }

    /// Get the metadata from the wheel archive
    pub fn metadata(&self) -> Result<(Vec<u8>, WheelCoreMetadata), WheelVitalsError> {
        let mut archive = self.archive.lock();

        // Determine the name of the dist-info directory
        let dist_info_prefix =
            find_dist_info_metadata(&self.name, archive.file_names().map(|name| ((), name)))?
                .1
                .to_owned();

        // Read the METADATA file
        let metadata_path = format!("{dist_info_prefix}.dist-info/METADATA");
        let metadata_blob = read_entry_to_end(&mut archive, &metadata_path)?;
        let metadata = WheelCoreMetadata::try_from(metadata_blob.as_slice())?;

        // Verify the contents of the METADATA
        if metadata.name != self.name.distribution {
            return Err(WheelCoreMetaDataError::FailedToParse(format!(
                "name mismatch between {dist_info_prefix}.dist-info/METADATA and filename ({} != {}",
                metadata.name.as_source_str(),
                self.name.distribution.as_source_str()
            ))
                .into());
        }
        if metadata.version != self.name.version {
            return Err(WheelCoreMetaDataError::FailedToParse(format!(
                "version mismatch between {dist_info_prefix}.dist-info/METADATA and filename ({} != {})",
                metadata.version, self.name.version
            ))
                .into());
        }

        Ok((metadata_blob, metadata))
    }
}

#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum WheelVitalsError {
    #[error(".dist-info/ missing")]
    DistInfoMissing,

    #[error(".dist-info/WHEEL missing")]
    WheelMissing,

    #[error(".dist-info/METADATA missing")]
    MetadataMissing,

    #[error("found multiple {0} directories in wheel")]
    MultipleSpecialDirs(Cow<'static, str>),

    #[error("failed to parse WHEEL file")]
    FailedToParseWheel(#[source] <RFC822ish as FromStr>::Err),

    #[error("unsupported WHEEL version {0}")]
    UnsupportedWheelVersion(String),

    #[error("invalid METADATA")]
    InvalidMetadata(#[from] WheelCoreMetaDataError),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("Failed to read the wheel file {0}")]
    ZipError(String, #[source] ZipError),

    #[error("Failed to read the wheel file {0}: {1}")]
    AsyncZipError(String, #[source] async_zip::error::ZipError),

    #[error("missing key from WHEEL '{0}'")]
    MissingKeyInWheel(String),
}

impl WheelVitalsError {
    pub(crate) fn from_zip(file: String, err: ZipError) -> Self {
        match err {
            ZipError::Io(err) => WheelVitalsError::IoError(err),
            ZipError::FileNotFound => {
                if file.ends_with("WHEEL") {
                    WheelVitalsError::WheelMissing
                } else if file.ends_with("METADATA") {
                    WheelVitalsError::MetadataMissing
                } else {
                    WheelVitalsError::ZipError(file, err)
                }
            }
            _ => WheelVitalsError::ZipError(file, err),
        }
    }

    pub(crate) fn from_async_zip(file: String, err: async_zip::error::ZipError) -> Self {
        match err {
            async_zip::error::ZipError::UpstreamReadError(err) => WheelVitalsError::IoError(err),
            _ => WheelVitalsError::AsyncZipError(file, err),
        }
    }
}

/// Helper method to read a particular file from a zip archive.
fn read_entry_to_end<R: ReadAndSeek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<Vec<u8>, WheelVitalsError> {
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .map_err(|err| WheelVitalsError::from_zip(name.to_string(), err))?
        .read_to_end(&mut bytes)?;

    Ok(bytes)
}

/// Locates the `.dist-info` directory in a list of files. The function returns `.dist-info` prefix.
/// E.g. for `rich-13.6.0.dist-info` this function return `rich-13.6.0`.
///
/// Next to the filename the iterator also contains an additional object that is associated with the
/// filename. The return value will contain the object associated with the METADATA file inside the
/// `.dist-info` directory.
pub(crate) fn find_dist_info_metadata<'a, T>(
    wheel_name: &WheelFilename,
    files: impl IntoIterator<Item = (T, &'a str)>,
) -> Result<(T, &'a str), WheelVitalsError> {
    let mut dist_infos = files.into_iter().filter_map(|(t, path)| {
        let (dir_name, rest) = path.split_once(['/', '\\'])?;
        let dir_stem = dir_name.strip_suffix(".dist-info")?;
        let (name, version) = dir_stem.rsplit_once('-')?;
        if PackageName::from_str(name).ok()? == wheel_name.distribution
            && Version::from_str(version).ok()? == wheel_name.version
            && rest == "METADATA"
        {
            Some((t, dir_stem))
        } else {
            None
        }
    });

    match (dist_infos.next(), dist_infos.next()) {
        (Some(path), None) => Ok(path),
        (Some(_), Some(_)) => Err(WheelVitalsError::MultipleSpecialDirs("dist-info".into())),
        _ => Err(WheelVitalsError::DistInfoMissing),
    }
}
