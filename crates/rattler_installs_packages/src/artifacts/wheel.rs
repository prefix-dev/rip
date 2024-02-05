use crate::python_env::{ByteCodeCompiler, CompilationError};
use crate::types::{DirectUrlJson, HasArtifactName};
use crate::{
    python_env::PythonInterpreterVersion,
    types::Artifact,
    types::EntryPoint,
    types::Extra,
    types::NormalizedPackageName,
    types::PackageName,
    types::RFC822ish,
    types::WheelFilename,
    types::{Record, RecordEntry},
    types::{WheelCoreMetaDataError, WheelCoreMetadata},
    utils::ReadAndSeek,
};
use async_http_range_reader::AsyncHttpRangeReader;
use async_zip::base::read::seek::ZipFileReader;
use configparser::ini::Ini;
use data_encoding::BASE64URL_NOPAD;
use fs_err as fs;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use pep440_rs::Version;
use rattler_digest::Sha256;
use std::io::{BufRead, BufReader};
use std::sync::mpsc::channel;
use std::{
    borrow::Cow,
    collections::HashMap,
    collections::HashSet,
    ffi::OsStr,
    io::{Read, Write},
    iter::FromIterator,
    path::{Component, Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;
use tokio_util::compat::TokioAsyncReadCompatExt;
use zip::{result::ZipError, ZipArchive};

use crate::win::launcher::{build_windows_launcher, LauncherType, WindowsLauncherArch};

/// Wheel file in the PyPI ecosystem.
/// See the [Reference Page](https://packaging.python.org/en/latest/specifications/binary-distribution-format/#binary-distribution-format)
/// for more information.
pub struct Wheel {
    name: WheelFilename,
    archive: Mutex<ZipArchive<Box<dyn ReadAndSeek + Send>>>,
}

impl HasArtifactName for Wheel {
    type Name = WheelFilename;

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

impl Artifact for Wheel {
    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        Ok(Self {
            name,
            archive: Mutex::new(ZipArchive::new(bytes).into_diagnostic()?),
        })
    }
}

impl Wheel {
    /// Open a wheel by reading a file on disk.
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
        Self::new(wheel_name, Box::new(file))
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

        Self::new(wheel_filename.clone(), Box::new(bytes))
    }

    /// A wheel file always contains a special directory that contains the metadata of the package.
    /// This function returns the name of that directory.
    fn find_special_wheel_dir<'a>(
        top_level_names: impl IntoIterator<Item = &'a str>,
        name: &PackageName,
        version: &Version,
        suffix: &str,
    ) -> Result<Option<&'a str>, WheelVitalsError> {
        // Find all directories that end in the suffix
        let mut candidates = top_level_names.into_iter().filter(|dir_name| {
            let Some(candidate) = dir_name.strip_suffix(suffix) else {
                return false;
            };
            let Some((candidate_name, candidate_version)) = candidate.rsplit_once('-') else {
                return false;
            };

            let Ok(candidate_name) = PackageName::from_str(candidate_name) else {
                return false;
            };
            let Ok(candidate_version) = Version::from_str(candidate_version) else {
                return false;
            };

            &candidate_name == name && &candidate_version == version
        });

        // Get the first candidate
        let candidate = match candidates.next() {
            Some(candidate) => candidate,
            None => return Ok(None),
        };

        // Error out if there are multiple directories
        if candidates.next().is_some() {
            return Err(WheelVitalsError::MultipleSpecialDirs(suffix.to_owned()));
        }

        Ok(Some(candidate))
    }

    async fn get_lazy_vitals(
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
        let top_level_names = reader
            .file()
            .entries()
            .iter()
            .filter_map(|e| e.entry().filename().as_str().ok())
            .map(|filename| {
                filename
                    .split_once(['/', '\\'])
                    .map_or_else(|| filename, |(base, _)| base)
            })
            .collect::<HashSet<_>>();

        // Determine the name of the dist-info directory
        let dist_info = Wheel::find_special_wheel_dir(
            top_level_names.iter().copied(),
            &name.distribution,
            &name.version,
            ".dist-info",
        )?
        .ok_or(WheelVitalsError::DistInfoMissing)?
        .to_owned();

        let metadata_path = format!("{dist_info}/METADATA");
        let (metadata_idx, metadata_entry) = reader
            .file()
            .entries()
            .iter()
            .enumerate()
            .find(|(_, p)| p.entry().filename().as_str().ok() == Some(metadata_path.as_str()))
            .ok_or(WheelVitalsError::MetadataMissing)?;

        // Get the size of the entry plus the header + size of the filename. We should also actually
        // include bytes for the extra fields but we don't have that information.
        let offset = metadata_entry.header_offset();
        let size = metadata_entry.entry().compressed_size()
            + 30 // Header size in bytes
            + metadata_entry.entry().filename().as_bytes().len() as u64;

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

    fn get_vitals(&self) -> Result<WheelVitals, WheelVitalsError> {
        let mut archive = self.archive.lock();

        // Determine the top level filenames in the wheel
        let top_level_names = archive
            .file_names()
            .map(|filename| {
                filename
                    .split_once(['/', '\\'])
                    .map_or_else(|| filename, |(base, _)| base)
            })
            .collect::<HashSet<_>>();

        // Determine the name of the dist-info directory
        let dist_info = Wheel::find_special_wheel_dir(
            top_level_names.iter().copied(),
            &self.name.distribution,
            &self.name.version,
            ".dist-info",
        )?
        .ok_or(WheelVitalsError::DistInfoMissing)?
        .to_owned();

        // Determine the name of the data directory
        let data = Wheel::find_special_wheel_dir(
            top_level_names,
            &self.name.distribution,
            &self.name.version,
            ".data",
        )?
        .map_or_else(
            || format!("{}.data", dist_info.strip_suffix(".dist-info").unwrap()),
            ToOwned::to_owned,
        );

        let wheel_path = format!("{dist_info}/WHEEL");
        let wheel_metadata = read_entry_to_end(&mut archive, &wheel_path)?;

        let mut parsed = parse_format_metadata_and_check_version(&wheel_metadata, "Wheel-Version")?;

        let root_is_purelib = match &parsed
            .take("Root-Is-Purelib")
            .map_err(|_| WheelCoreMetaDataError::MissingKey(String::from("Root-Is-Purelib")))?[..]
        {
            "true" => true,
            "false" => false,
            other => {
                return Err(WheelCoreMetaDataError::FailedToParse(format!(
                    "Expected 'true' or 'false' for Root-Is-Purelib, not {}",
                    other,
                ))
                .into());
            }
        };

        let metadata_path = format!("{dist_info}/METADATA");
        let metadata_blob = read_entry_to_end(&mut archive, &metadata_path)?;
        let metadata = WheelCoreMetadata::try_from(metadata_blob.as_slice())?;

        if metadata.name != self.name.distribution {
            return Err(WheelCoreMetaDataError::FailedToParse(format!(
                "name mismatch between {dist_info}/METADATA and filename ({} != {}",
                metadata.name.as_source_str(),
                self.name.distribution.as_source_str()
            ))
            .into());
        }
        if metadata.version != self.name.version {
            return Err(WheelCoreMetaDataError::FailedToParse(format!(
                "version mismatch between {dist_info}/METADATA and filename ({} != {})",
                metadata.version, self.name.version
            ))
            .into());
        }

        Ok(WheelVitals {
            dist_info,
            data,
            root_is_purelib,
            metadata_blob,
            metadata,
        })
    }

    /// Get the metadata from the wheel archive
    pub fn metadata(&self) -> miette::Result<(Vec<u8>, WheelCoreMetadata)> {
        let WheelVitals {
            metadata_blob,
            metadata,
            ..
        } = self.get_vitals().into_diagnostic()?;
        Ok((metadata_blob, metadata))
    }

    /// Read metadata from bytes-stream
    pub async fn read_metadata_bytes(
        name: &WheelFilename,
        stream: &mut AsyncHttpRangeReader,
    ) -> miette::Result<(Vec<u8>, WheelCoreMetadata)> {
        Self::get_lazy_vitals(name, stream).await.into_diagnostic()
    }
}

#[derive(Debug)]
/// Internals for the wheel that have to be present
pub struct WheelVitals {
    dist_info: String,
    data: String,
    root_is_purelib: bool,
    metadata_blob: Vec<u8>,
    metadata: WheelCoreMetadata,
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
    MultipleSpecialDirs(String),

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

fn parse_format_metadata_and_check_version(
    input: &[u8],
    version_field: &str,
) -> Result<RFC822ish, WheelVitalsError> {
    let input = String::from_utf8_lossy(input);
    let mut parsed = RFC822ish::from_str(&input).map_err(WheelVitalsError::FailedToParseWheel)?;

    let version = parsed
        .take(version_field)
        .map_err(|_| WheelVitalsError::MissingKeyInWheel(version_field.into()))?;
    if !version.starts_with("1.") {
        return Err(WheelVitalsError::UnsupportedWheelVersion(version));
    }

    Ok(parsed)
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

/// A struct of installation categories to where they should be stored relative to the
/// installation destination.
#[derive(Debug, Clone)]
pub struct InstallPaths {
    purelib: PathBuf,
    platlib: PathBuf,
    scripts: PathBuf,
    data: PathBuf,
    headers: PathBuf,
    windows: bool,
}

impl InstallPaths {
    /// Populates mappings of installation targets for a virtualenv layout. The mapping depends on
    /// the python version and whether or not the installation targets windows. Specifically on
    /// windows some of the paths are different. :shrug:
    pub fn for_venv<V: Into<PythonInterpreterVersion>>(version: V, windows: bool) -> Self {
        let version = version.into();

        let site_packages = if windows {
            Path::new("Lib").join("site-packages")
        } else {
            Path::new("lib").join(format!(
                "python{}.{}/site-packages",
                version.major, version.minor
            ))
        };
        let scripts = if windows {
            PathBuf::from("Scripts")
        } else {
            PathBuf::from("bin")
        };

        // Data should just be the root of the venv
        let data = PathBuf::from("");

        // purelib and platlib locations are not relevant when using venvs
        // https://stackoverflow.com/a/27882460/3549270
        Self {
            purelib: site_packages.clone(),
            platlib: site_packages,
            scripts,
            data,
            windows,
            headers: PathBuf::from("include"),
        }
    }

    /// Determines whether this is a windows InstallPath
    pub fn is_windows(&self) -> bool {
        self.windows
    }

    /// Returns the site-packages location. This is done by searching for the purelib location.
    pub fn site_packages(&self) -> &Path {
        &self.purelib
    }

    /// Reference to pure python library location.
    pub fn purelib(&self) -> &Path {
        &self.purelib
    }

    /// Reference to platform specific library location.
    pub fn platlib(&self) -> &Path {
        &self.platlib
    }

    /// Returns the binaries location.
    pub fn scripts(&self) -> &Path {
        &self.scripts
    }

    /// Returns the location of the data directory
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// Returns the location of the include directory
    pub fn include(&self) -> PathBuf {
        if self.windows {
            PathBuf::from("Include")
        } else {
            PathBuf::from("include")
        }
    }

    /// Returns the location of the headers directory. The location of headers is specific to a
    /// distribution name.
    pub fn headers(&self, distribution_name: &str) -> PathBuf {
        self.headers.join(distribution_name)
    }

    /// Matches the different categories to their install paths.
    pub fn match_category(&self, category: &str, distribution_name: &str) -> Option<Cow<Path>> {
        match category {
            "purelib" => Some(self.purelib().into()),
            "platlib" => Some(self.platlib().into()),
            "scripts" => Some(self.scripts().into()),
            "data" => Some(self.data().into()),
            "headers" => Some(self.headers(distribution_name).into()),
            &_ => None,
        }
    }
}

#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum UnpackError {
    #[error(transparent)]
    FailedToParseWheelVitals(#[from] WheelVitalsError),

    #[error("missing installation path for {0}")]
    MissingInstallPath(String),

    #[error("Failed to read the wheel file {0}")]
    ZipError(String, #[source] ZipError),

    #[error("failed to write {0}")]
    IoError(String, #[source] std::io::Error),

    #[error("RECORD file is invalid")]
    RecordCsv(#[from] csv::Error),

    #[error("RECORD file doesn't match wheel contents: {0}")]
    RecordFile(String),

    #[error("unrecognized .data directory: {0}")]
    UnsupportedDataDirectory(String),

    #[error("entry_points.txt invalid, {0}")]
    EntryPointsInvalid(String),

    #[error("could not create entry points because the windows architecture is unsupported")]
    UnsupportedWindowsArchitecture,

    #[error("bytecode compilation failed, {0}")]
    ByteCodeCompilationFailed(String, #[source] CompilationError),

    #[error("failed to write `direct_url.json` to .dist-info")]
    FailedToWriteDirectUrlJson(#[from] serde_json::Error),
}

impl UnpackError {
    pub(crate) fn from_zip_error(file: String, error: ZipError) -> Self {
        match error {
            ZipError::Io(err) => Self::IoError(file, err),
            _ => Self::ZipError(file, error),
        }
    }
}

/// Additional optional settings to pass to [`Wheel::unpack`].
///
/// Not all options in this struct are relevant. Typically you will default a number of fields.
#[derive(Default)]
pub struct UnpackWheelOptions<'i> {
    /// When specified an INSTALLER file is written to the dist-info folder of the package.
    /// INSTALLER files are used to track the installer of a package. See [PEP 376](https://peps.python.org/pep-0376/) for more information.
    pub installer: Option<String>,

    /// The extras of the wheel that should be activated. This affects the creation of entry points.
    /// If `None` is specified, extras are *not* taken into account. This is different from
    /// specifying an empty set because when specifying `None` no filtering based on extras is
    /// performed. This is the default.
    pub extras: Option<HashSet<Extra>>,

    /// The architecture of the launcher executable that is created for every entry point on windows.
    /// If this field is `None` the architecture will be determined based on the architecture of the
    /// current process.
    pub launcher_arch: Option<WindowsLauncherArch>,

    /// A reference to a bytecode compiler that can be used to compile the bytecode of the wheel. If
    /// this field is `None` bytecode compilation will be skipped.
    pub byte_code_compiler: Option<&'i ByteCodeCompiler>,

    /// The `direct_url.json` file that should be written to the dist-info folder of the package.
    /// because when using `unpack` on the wheel we do not know where it came from.
    /// This needs to be supplied manually.
    pub direct_url_json: Option<DirectUrlJson>,
}

#[derive(Debug)]
/// Information about a wheel that has been unpacked into the destination directory.
pub struct UnpackedWheel {
    /// The path to the *.dist-info directory of the unpacked wheel.
    pub dist_info: PathBuf,

    /// The metadata of the wheel
    pub metadata: WheelCoreMetadata,
}

impl Wheel {
    /// Unpacks a wheel to the given filesystem.
    /// TODO: Write better docs.
    /// The following functionality is still missing:
    /// - entry_points.txt
    /// - Rewrite #!python.
    /// - Generate script wrappers.
    /// - bytecode compilation
    /// - REQUESTED (<https://peps.python.org/pep-0376/#requested>)
    /// - direct_url.json (<https://peps.python.org/pep-0610/>)
    /// - support "headers" category
    pub fn unpack(
        &self,
        dest: &Path,
        paths: &InstallPaths,
        python_executable: &Path,
        options: &UnpackWheelOptions,
    ) -> Result<UnpackedWheel, UnpackError> {
        let vitals = self
            .get_vitals()
            .map_err(UnpackError::FailedToParseWheelVitals)?;

        let transformer = WheelPathTransformer {
            data: vitals.data,
            root_is_purelib: vitals.root_is_purelib,
            paths,
            name: self.name.distribution.as_str(),
        };

        let trampoline_maker = TrampolineMaker {
            python_executable: python_executable.to_path_buf(),
            kind: if paths.is_windows() {
                TrampolineMakerKind::Windows {
                    arch: options.launcher_arch,
                }
            } else {
                TrampolineMakerKind::Unix
            },
        };

        let site_packages = dest.join(paths.site_packages());
        let mut archive = self.archive.lock();

        // Read the RECORD file from the wheel
        let record_filename = format!("{}/RECORD", &vitals.dist_info);
        let record = Record::from_reader(
            &mut archive
                .by_name(&record_filename)
                .map_err(|err| WheelVitalsError::from_zip(record_filename.clone(), err))?,
        )?;
        let record_relative_path = Path::new(&record_filename);

        // Read `entry_points.txt` and parse any scripts we need to create.
        let scripts =
            Scripts::from_wheel(&mut archive, &vitals.dist_info, options.extras.as_ref())?;

        let mut resulting_records = Vec::new();
        let (pyc_tx, pyc_rx) = channel();
        for index in 0..archive.len() {
            let mut zip_entry = archive
                .by_index(index)
                .map_err(|e| UnpackError::from_zip_error(format!("<index {index}>"), e))?;
            let Some(relative_path) = zip_entry.enclosed_name().map(ToOwned::to_owned) else {
                // Skip invalid paths
                continue;
            };

            // Skip the RECORD file itself. We will overwrite it at the end of this operation to
            // reflect all files that were added. PEP 491 defines some extra files that refer to the
            // RECORD file that we can skip. See <https://peps.python.org/pep-0491/>
            // > 6. RECORD.jws is used for digital signatures. It is not mentioned in RECORD.
            // > 7. RECORD.p7s is allowed as a courtesy to anyone who would prefer to use S/MIME
            // >    signatures to secure their wheel files. It is not mentioned in RECORD.
            if relative_path == record_relative_path
                || relative_path == record_relative_path.with_extension("jws")
                || relative_path == record_relative_path.with_extension("p7s")
            {
                continue;
            }

            // Determine the destination path.
            let Some((relative_destination, is_script)) =
                transformer.analyze_path(&relative_path)?
            else {
                continue;
            };
            let destination = dest.join(relative_destination);

            // If the entry refers to a directory we simply create it.
            if zip_entry.is_dir() {
                fs::create_dir_all(&destination)
                    .map_err(|err| UnpackError::IoError(destination.display().to_string(), err))?;
                continue;
            }

            // Determine if the entry is executable
            let executable = zip_entry
                .unix_mode()
                .map(|v| v & 0o0111 != 0)
                .unwrap_or(false);

            // If the file is a script
            let (size, encoded_hash) = if is_script {
                if scripts.is_entrypoint_wrapper(&destination) {
                    continue;
                }

                // Use a BufReader to make it easy to peek at the first few bytes without actually
                // reading the contents of the file.
                let mut buf_reader = BufReader::new(zip_entry);
                let script_start = buf_reader
                    .fill_buf()
                    .map_err(|err| UnpackError::IoError(destination.display().to_string(), err))?;

                // Check if the script is a python script or a native binary
                if script_start.starts_with(b"#!python") {
                    // Determine the type of script
                    let launcher_type = if script_start.starts_with(b"#!pythonw") {
                        LauncherType::Gui
                    } else {
                        LauncherType::Console
                    };

                    // Read the shebang line from the script
                    buf_reader.read_line(&mut String::new()).map_err(|err| {
                        UnpackError::IoError(destination.display().to_string(), err)
                    })?;

                    // Read the rest of the script
                    let mut script = Vec::new();
                    buf_reader.read_to_end(&mut script).map_err(|err| {
                        UnpackError::IoError(destination.display().to_string(), err)
                    })?;

                    // Generate the launcher
                    let trampoline = trampoline_maker.make_trampoline(launcher_type, &script)?;
                    let relative_path = pathdiff::diff_paths(&destination, &site_packages).expect("can always create relative path from site-packages to the scripts directory");
                    let record =
                        write_generated_file(&relative_path, &site_packages, trampoline, true)?;
                    resulting_records.push(record);

                    // The hash has most likely changed so we don't check it.
                    continue;
                } else {
                    // Otherwise copy the file verbatim
                    write_wheel_file(&mut buf_reader, &destination, true)?
                }
            } else {
                // Otherwise copy the file to its final destination.
                write_wheel_file(&mut zip_entry, &destination, executable)?
            };

            // If the file is a python file we need to compile it to bytecode
            if let Some(bytecode_compiler) = options.byte_code_compiler.as_ref() {
                if destination.extension() == Some(OsStr::new("py")) {
                    let pyc_tx = pyc_tx.clone();
                    let cloned_destination = destination.clone();
                    bytecode_compiler
                        .compile(&destination, move |result| {
                            // Ignore any error that might occur due to the receiver being closed.
                            let _ = pyc_tx.send((cloned_destination, result));
                        })
                        .map_err(|err| {
                            UnpackError::ByteCodeCompilationFailed(
                                destination.display().to_string(),
                                err,
                            )
                        })?;
                }
            }

            // Make sure the hash matches with what we expect
            if let Some(encoded_hash) = encoded_hash {
                let relative_path_string = relative_path.display().to_string();

                // Find the record in the RECORD entries
                let recorded_hash = record
                    .iter()
                    .find(|entry| {
                        // Strip any preceding slashes from the path since all paths in the wheel
                        // RECORD should be relative.
                        entry.path.trim_start_matches('/') == relative_path_string
                    })
                    .and_then(|entry| entry.hash.as_ref())
                    .ok_or_else(|| {
                        UnpackError::RecordFile(format!(
                            "missing hash for {} (expected {})",
                            relative_path.display(),
                            encoded_hash
                        ))
                    })?;

                // Ensure that the hashes match
                if &encoded_hash != recorded_hash {
                    return Err(UnpackError::RecordFile(format!(
                        "hash mismatch for {}. Recorded: {}, Actual: {}",
                        relative_path.display(),
                        recorded_hash,
                        encoded_hash,
                    )));
                }

                // Store the hash
                resulting_records.push(RecordEntry {
                    path: pathdiff::diff_paths(&destination, &site_packages)
                        .unwrap_or_else(|| {
                            dunce::canonicalize(&destination).expect("failed to canonicalize path")
                        })
                        .display()
                        .to_string()
                        // Replace \ with /. This is not strictly necessary, and the spec even
                        // specifies that the OS separators should be used, but in the case that we
                        // are unpacking for a different OS from Windows, it makes sense to use
                        // forward slashes everywhere. Windows can work with both anyway.
                        .replace('\\', "/"),
                    hash: Some(encoded_hash),
                    size,
                })
            }
        }

        // Generate the script entrypoints
        write_script_entrypoint(
            dest,
            paths,
            &scripts.console_scripts,
            &trampoline_maker,
            LauncherType::Console,
            &mut resulting_records,
        )?;
        write_script_entrypoint(
            dest,
            paths,
            &scripts.gui_scripts,
            &trampoline_maker,
            LauncherType::Gui,
            &mut resulting_records,
        )?;

        // Add the RECORD file itself to the records
        resulting_records.push(RecordEntry {
            path: record_relative_path.display().to_string(),
            hash: None,
            size: None,
        });

        // Write the INSTALLER if requested
        if let Some(installer) = options.installer.as_ref() {
            resulting_records.push(write_generated_file(
                Path::new(&format!("{}/INSTALLER", &vitals.dist_info)),
                &site_packages,
                format!("{}\n", installer.trim()),
                false,
            )?);
        }

        // Write `direct_url.json` if requested
        if let Some(direct_url_json) = options.direct_url_json.as_ref() {
            resulting_records.push(write_generated_file(
                Path::new(&format!("{}/direct_url.json", &vitals.dist_info)),
                &site_packages,
                serde_json::to_string(direct_url_json)?,
                false,
            )?);
        }

        // Write all the compiled bytecode files to the RECORD file
        drop(pyc_tx);
        for (source, result) in pyc_rx {
            let absolute_path = match result {
                Ok(absolute_path) => absolute_path,
                Err(CompilationError::NotAPythonFile | CompilationError::SourceNotFound) => {
                    unreachable!("we check these guarantees")
                }
                Err(CompilationError::FailedToCompile) => {
                    // Compilation errors are silently ignore.. This is the same behavior pip has.
                    continue;
                }
                Err(err @ CompilationError::HostQuit) => {
                    return Err(UnpackError::ByteCodeCompilationFailed(
                        source.display().to_string(),
                        err,
                    ));
                }
            };
            let relative_path = pathdiff::diff_paths(&absolute_path, &site_packages)
                .expect("can always create relative path from site-packages");
            let record = RecordEntry {
                path: relative_path.display().to_string().replace('\\', "/"),
                hash: None,
                size: None,
            };
            resulting_records.push(record);
        }

        // Write the resulting RECORD file
        Record::from_iter(resulting_records)
            .write_to_path(&site_packages.join(record_relative_path))?;

        Ok(UnpackedWheel {
            dist_info: site_packages.join(&vitals.dist_info),
            metadata: vitals.metadata,
        })
    }
}

/// Construct trampolines for entry-points.
fn write_script_entrypoint(
    dest: &Path,
    install_paths: &InstallPaths,
    entry_points: &Vec<EntryPoint>,
    trampoline_maker: &TrampolineMaker,
    launcher_type: LauncherType,
    records: &mut Vec<RecordEntry>,
) -> Result<(), UnpackError> {
    // Make sure the script directory exists
    let scripts_dir = dest.join(install_paths.scripts());
    fs::create_dir_all(&scripts_dir)
        .map_err(|err| UnpackError::IoError(scripts_dir.display().to_string(), err))?;

    for entry_point in entry_points {
        // Determine the name of the script
        let script_name = if install_paths.is_windows() {
            // Convert the entry point filename. We strip `.py` from the filename and add `.exe`.
            Cow::Owned(format!(
                "{}.exe",
                entry_point
                    .script_name
                    .strip_suffix(".py")
                    .unwrap_or(&entry_point.script_name)
            ))
        } else {
            Cow::Borrowed(entry_point.script_name.as_str())
        };

        // Construct the trampoline
        let launch_script = entry_point.launch_script();
        let trampoline =
            trampoline_maker.make_trampoline(launcher_type, launch_script.as_bytes())?;

        // Write the launcher to the destination
        let script_path = dest
            .join(install_paths.scripts())
            .join(script_name.as_ref());
        let site_packages = dest.join(install_paths.site_packages());
        let relative_path = pathdiff::diff_paths(script_path, &site_packages).expect("should always be able to create relative path from site-packages to the scripts directory");
        let record = write_generated_file(&relative_path, &site_packages, &trampoline, true)?;
        records.push(record)
    }

    Ok(())
}

/// An object that can be used to generate trampolines.
///
/// Trampolines are executable that execute a certain python script using a certain python
/// interpreter. They are used to launch entry points.
///
/// On unix based systems this simply creates a script with a python shebang. On windows this
/// creates a separate executable that launches the python interpreter with the given script. See
/// [`crate::launcher`] for more information.
struct TrampolineMaker {
    python_executable: PathBuf,
    kind: TrampolineMakerKind,
}

/// The type of trampoline to create
enum TrampolineMakerKind {
    Windows { arch: Option<WindowsLauncherArch> },
    Unix,
}

impl TrampolineMaker {
    /// Returns the bytes of a launcher executable/script that can be used to launch the given
    /// script.
    pub fn make_trampoline(
        &self,
        launcher_type: LauncherType,
        script: &[u8],
    ) -> Result<Vec<u8>, UnpackError> {
        let shebang = get_shebang(&self.python_executable);
        match self.kind {
            TrampolineMakerKind::Windows { arch } => {
                let arch = match arch {
                    Some(windows_launcher_arch) => windows_launcher_arch,
                    None => match WindowsLauncherArch::current() {
                        Some(arch) => arch,
                        None => return Err(UnpackError::UnsupportedWindowsArchitecture),
                    },
                };

                Ok(build_windows_launcher(
                    &shebang,
                    script,
                    arch,
                    launcher_type,
                ))
            }
            TrampolineMakerKind::Unix => {
                let mut bytes = format!("{}\n", shebang).into_bytes();
                bytes.extend_from_slice(script);
                Ok(bytes)
            }
        }
    }
}

/// Returns the shebang to use when calling a python script.
/// TODO: In the future we should make this much more configurable. This is much more complex in pip:
///  <https://github.com/pypa/pip/blob/7f8a6844037fb7255cfd0d34ff8e8cf44f2598d4/src/pip/_vendor/distlib/scripts.py#L158>
fn get_shebang(python_executable: &Path) -> String {
    format!(r"#!{}", dunce::simplified(python_executable).display())
}

/// The scripts that should be installed as part of the wheel installation.
#[derive(Debug, Default)]
struct Scripts {
    console_scripts: Vec<EntryPoint>,
    gui_scripts: Vec<EntryPoint>,
}

impl Scripts {
    /// Read the `entry_points.txt` file from the wheel archive and parse the scripts.
    pub fn from_wheel(
        archive: &mut ZipArchive<Box<dyn ReadAndSeek + Send>>,
        dist_info_prefix: &str,
        extras: Option<&HashSet<Extra>>,
    ) -> Result<Self, UnpackError> {
        // Read the `entry_points.txt` file from the archive
        let entry_points_path = format!("{dist_info_prefix}/entry_points.txt");
        let mut entry_points_file = match archive.by_name(&entry_points_path) {
            Err(ZipError::FileNotFound) => return Ok(Default::default()),
            Ok(file) => file,
            Err(err) => return Err(UnpackError::from_zip_error(entry_points_path, err)),
        };

        // Parse the `entry_points.txt` file as an ini file.
        let mut entry_points_mapping = {
            let mut ini_contents = String::new();
            entry_points_file
                .read_to_string(&mut ini_contents)
                .map_err(|err| {
                    UnpackError::EntryPointsInvalid(format!(
                        "failed to read entry_points.txt contents: {}",
                        err
                    ))
                })?;
            Ini::new_cs().read(ini_contents).map_err(|err| {
                UnpackError::EntryPointsInvalid(format!(
                    "failed to parse entry_points.txt contents: {}",
                    err
                ))
            })?
        };

        // Parse the script entry points
        let console_scripts = entry_points_mapping
            .remove("console_scripts")
            .map(|e| parse_entry_points_from_ini_section(e, extras))
            .transpose()?
            .unwrap_or_default();

        let gui_scripts = entry_points_mapping
            .remove("gui_scripts")
            .map(|e| parse_entry_points_from_ini_section(e, extras))
            .transpose()?
            .unwrap_or_default();

        Ok(Scripts {
            console_scripts,
            gui_scripts,
        })
    }

    /// Returns true if there is an entry point script with the given name.
    pub fn contains(&self, name: &str) -> bool {
        self.console_scripts.iter().any(|e| e.script_name == name)
            || self.gui_scripts.iter().any(|e| e.script_name == name)
    }

    /// Returns true if the script at the given path is an entry point script.
    ///
    /// Setuptools generates wrapper scripts for entry-points. This function checks if the script at
    /// the given path is such a script.
    pub fn is_entrypoint_wrapper(&self, path: &Path) -> bool {
        let file_name = path.file_name().map(OsStr::to_string_lossy);
        let Some(file_name) = file_name else {
            return false;
        };

        let script_name = file_name
            .strip_suffix(".exe")
            .or_else(|| file_name.strip_suffix("-script.py"))
            .or_else(|| file_name.strip_suffix(".pya"))
            .unwrap_or(&file_name);

        self.contains(script_name)
    }
}

/// Parse entry points from a section in the `entry_points.txt` file.
fn parse_entry_points_from_ini_section(
    entry_points: HashMap<String, Option<String>>,
    extras: Option<&HashSet<Extra>>,
) -> Result<Vec<EntryPoint>, UnpackError> {
    let mut result = Vec::new();
    for (script_name, entry_point) in entry_points {
        let entry_point = entry_point.ok_or_else(|| {
            UnpackError::EntryPointsInvalid(format!("missing entry point for {}", script_name))
        })?;
        match EntryPoint::parse(script_name.clone(), &entry_point, extras) {
            Ok(None) => {}
            Ok(Some(entry_point)) => result.push(entry_point),
            Err(err) => {
                return Err(UnpackError::EntryPointsInvalid(format!(
                    "failed to parse entry point for {}: {}",
                    script_name, err
                )));
            }
        }
    }
    Ok(result)
}

fn write_generated_file(
    relative_path: &Path,
    site_packages: &Path,
    content: impl AsRef<[u8]>,
    _executable: bool,
) -> Result<RecordEntry, UnpackError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use fs::os::unix::fs::OpenOptionsExt;
        if _executable {
            options.mode(0o777);
        } else {
            options.mode(0o666);
        }
    }

    let (size, digest) = options
        .open(site_packages.join(relative_path))
        .map(rattler_digest::HashingWriter::<_, Sha256>::new)
        .and_then(|mut file| {
            let content = content.as_ref();
            file.write_all(content)?;
            let (_, digest) = file.finalize();
            Ok((content.len(), digest))
        })
        .map_err(|err| UnpackError::IoError(relative_path.display().to_string(), err))?;

    Ok(RecordEntry {
        path: relative_path.display().to_string().replace('\\', "/"),
        hash: Some(format!("sha256={}", BASE64URL_NOPAD.encode(&digest))),
        size: Some(size as _),
    })
}

/// Write a file from a wheel archive to disk.
fn write_wheel_file(
    mut reader: &mut impl Read,
    destination: &Path,
    _executable: bool,
) -> Result<(Option<u64>, Option<String>), UnpackError> {
    let mut reader = rattler_digest::HashingReader::<_, Sha256>::new(&mut reader);

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use fs::os::unix::fs::OpenOptionsExt;
        if _executable {
            options.mode(0o777);
        } else {
            options.mode(0o666);
        }
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| UnpackError::IoError(parent.display().to_string(), err))?;
    }
    let mut file = options
        .open(destination)
        .map_err(|err| UnpackError::IoError(destination.display().to_string(), err))?;
    let size = std::io::copy(&mut reader, &mut file)
        .map_err(|err| UnpackError::IoError(destination.display().to_string(), err))?;
    let (_, digest) = reader.finalize();
    Ok((
        Some(size),
        Some(format!("sha256={}", BASE64URL_NOPAD.encode(&digest))),
    ))
}

/// Implements the logic to determine where a files from a wheel should be placed on the filesystem
/// and whether we should apply special logic.
///
/// This implements the logic from <https://peps.python.org/pep-0427/#details>
struct WheelPathTransformer<'a> {
    /// The name of the data directory in the wheel archive
    data: String,

    /// Whether the wheel is a purelib or a platlib.
    root_is_purelib: bool,

    /// The location in the filesystem where to place files from the data directory.
    paths: &'a InstallPaths,

    /// The name of the distribution
    name: &'a str,
}

impl<'a> WheelPathTransformer<'a> {
    /// Given a path from a wheel zip, analyze the path and determine its final destination path.
    ///
    /// Returns `None` if the path should be ignored.
    fn analyze_path(&self, path: &Path) -> Result<Option<(PathBuf, bool)>, UnpackError> {
        let (category, rest_of_path) = if let Ok(data_path) = path.strip_prefix(&self.data) {
            let mut components = data_path.components();
            if let Some(category) = components.next() {
                let Component::Normal(name) = category else {
                    // TODO: Better error handling
                    panic!("invalid path")
                };
                (name.to_string_lossy(), components.as_path())
            } else {
                // This is the data directory itself. Discard that.
                return Ok(None);
            }
        } else {
            let category = if self.root_is_purelib {
                Cow::Borrowed("purelib")
            } else {
                Cow::Borrowed("platlib")
            };
            (category, path)
        };

        match self.paths.match_category(category.as_ref(), self.name) {
            Some(basepath) => Ok(Some((basepath.join(rest_of_path), category == "scripts"))),
            None => Err(UnpackError::UnsupportedDataDirectory(category.into_owned())),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::python_env::{system_python_executable, PythonLocation, VEnv, WheelTags};
    use crate::types::{DirectUrlHashes, DirectUrlJson, DirectUrlSource};
    use rstest::rstest;
    use tempfile::{tempdir, TempDir};
    use test_utils::download_and_cache_file_async;
    use url::Url;

    const INSTALLER: &str = "pixi_test";

    #[rstest]
    #[case("https://files.pythonhosted.org/packages/58/76/705b5c776f783d1ba7c630347463d4ae323282bbd859a8e9420c7ff79581/selenium-4.1.0-py3-none-any.whl", "27e7b64df961d609f3d57237caa0df123abbbe22d038f2ec9e332fb90ec1a939")]
    #[case("https://files.pythonhosted.org/packages/1e/27/47f73510c6b80d1ff0829474947537ae9ab8d516cc48c6320b7f3677fa54/selenium-2.53.2-py2.py3-none-any.whl", "fa8333cf3013497e60d87ba68cae65ead8e7fa208be88ab9c561556103f540ef")]
    fn test_wheels(#[case] url: Url, #[case] sha256: &str) {
        test_wheel_unpack(
            test_utils::download_and_cache_file(url, sha256).unwrap(),
            &"selenium".parse().unwrap(),
        );
    }

    #[test]
    fn test_wheel_platlib_and_purelib() {
        test_wheel_unpack(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../test-data/wheels/purelib_and_platlib-1.0.0-cp38-cp38-linux_x86_64.whl",
            ),
            &"purelib_and_platlib".parse().unwrap(),
        );
    }

    #[test]
    fn test_wheel_miniblack() {
        test_wheel_unpack(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-data/wheels/miniblack-23.1.0-py3-none-any.whl"),
            &"miniblack".parse().unwrap(),
        );
    }

    struct UnpackedWheel {
        tmpdir: TempDir,
        _metadata: WheelCoreMetadata,
        dist_info: PathBuf,
        _install_paths: InstallPaths,
    }

    fn unpack_wheel(
        path: &Path,
        normalized_package_name: &NormalizedPackageName,
        byte_code_compiler: Option<&ByteCodeCompiler>,
    ) -> UnpackedWheel {
        let wheel = Wheel::from_path(path, normalized_package_name).unwrap();
        let tmpdir = tempdir().unwrap();

        // Construct the path lookup to install packages to
        let install_paths = InstallPaths::for_venv((3, 8, 5), false);

        // Unpack the wheel
        let unpacked = wheel
            .unpack(
                tmpdir.path(),
                &install_paths,
                Path::new("/invalid"),
                &UnpackWheelOptions {
                    installer: Some(String::from(INSTALLER)),
                    byte_code_compiler,
                    ..Default::default()
                },
            )
            .unwrap();

        UnpackedWheel {
            tmpdir,
            _metadata: unpacked.metadata,
            dist_info: unpacked.dist_info,
            _install_paths: install_paths,
        }
    }

    fn test_wheel_unpack(path: PathBuf, normalized_package_name: &NormalizedPackageName) {
        let filename = path
            .file_name()
            .and_then(OsStr::to_str)
            .expect("could not determine filename");
        let unpacked = unpack_wheel(&path, normalized_package_name, None);

        // Determine the location where we would expect the RECORD file to exist
        let record_path = unpacked.dist_info.join("RECORD");
        let record_content = fs::read_to_string(&unpacked.tmpdir.path().join(&record_path))
            .unwrap_or_else(|_| panic!("failed to read RECORD from {}", record_path.display()));

        insta::assert_snapshot!(filename, record_content);
    }

    #[test]
    fn test_installer() {
        let unpacked = unpack_wheel(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../test-data/wheels/purelib_and_platlib-1.0.0-cp38-cp38-linux_x86_64.whl",
            ),
            &"purelib-and-platlib".parse().unwrap(),
            None,
        );

        let relative_path = unpacked.dist_info.join("INSTALLER");
        let installer_content =
            fs::read_to_string(unpacked.tmpdir.path().join(relative_path)).unwrap();
        assert_eq!(installer_content, format!("{INSTALLER}\n"));
    }

    #[test]
    fn test_byte_code_compilation() {
        // We check this specific package because some of the files will fail to compile.
        let package_path = test_utils::download_and_cache_file(
            "https://files.pythonhosted.org/packages/2a/e8/4e05b0daceb19463339b2616bdb9d5ad6573e6259e4e665239e663c7ac3b/debugpy-1.5.1-cp38-cp38-manylinux_2_5_x86_64.manylinux1_x86_64.manylinux_2_12_x86_64.manylinux2010_x86_64.whl".parse().unwrap(),
            "b2df2c373e85871086bd55271c929670cd4e1dba63e94a08d442db830646203b").unwrap();

        let python_path = system_python_executable().unwrap();
        let compiler = ByteCodeCompiler::new(python_path).unwrap();
        let unpacked = unpack_wheel(&package_path, &"debugpy".parse().unwrap(), Some(&compiler));

        // Determine the location where we would expect the RECORD file to exist
        let record_path = unpacked.dist_info.join("RECORD");
        let record_content = fs::read_to_string(&unpacked.tmpdir.path().join(&record_path))
            .unwrap_or_else(|_| panic!("failed to read RECORD from {}", record_path.display()));

        // Replace all cpython references with cpython-xxx to ensure that no matter the version of
        // python the snapshot will match.
        let regex = regex::Regex::new("cpython-([0-9]+)").unwrap();
        let record_content = regex.replace_all(&record_content, "cpython-<version>");

        insta::assert_snapshot!(record_content);
    }

    #[test]
    fn test_headers() {
        // Create a virtual environment in a temporary directory
        let tmpdir = tempdir().unwrap();
        let venv = VEnv::create(tmpdir.path(), PythonLocation::System).unwrap();

        // Download our wheel file and install it in the virtual environment we just created
        let package_path = test_utils::download_and_cache_file(
            "https://files.pythonhosted.org/packages/02/72/36fb2c35547fdf473629579fc35d9a2034592ea3f01710702d81ef596e16/greenlet-3.0.1-cp310-cp310-win_amd64.whl".parse().unwrap(),
            "52e93b28db27ae7d208748f45d2db8a7b6a380e0d703f099c949d0f0d80b70e9").unwrap();
        let wheel = Wheel::from_path(&package_path, &"greenlet".parse().unwrap()).unwrap();
        venv.install_wheel(&wheel, &Default::default()).unwrap();

        // Check to make sure that the headers directory was created
        assert!(venv.root().join("include/greenlet/greenlet.h").is_file());
    }

    #[test]
    fn test_direct_url() {
        let tmpdir = tempdir().unwrap();
        let venv = VEnv::create(tmpdir.path(), PythonLocation::System).unwrap();

        // Download our wheel file and install it in the virtual environment we just created
        let package_path = test_utils::download_and_cache_file(
            "https://files.pythonhosted.org/packages/02/72/36fb2c35547fdf473629579fc35d9a2034592ea3f01710702d81ef596e16/greenlet-3.0.1-cp310-cp310-win_amd64.whl".parse().unwrap(),
            "52e93b28db27ae7d208748f45d2db8a7b6a380e0d703f099c949d0f0d80b70e9").unwrap();
        let wheel = Wheel::from_path(&package_path, &"greenlet".parse().unwrap()).unwrap();

        let direct_url = DirectUrlJson {
            url: Url::from_directory_path(&package_path).unwrap(),
            source: DirectUrlSource::Archive {
                hashes: DirectUrlHashes {
                    sha256: "95a7e86f46de9b5da6ec9365e1e96d1644c67328".to_string(),
                },
            },
        };
        let wheel = venv
            .install_wheel(
                &wheel,
                &UnpackWheelOptions {
                    direct_url_json: Some(direct_url),
                    ..Default::default()
                },
            )
            .unwrap();

        // Test if the direct_url.json file was written correctly
        assert!(wheel.dist_info.join("direct_url.json").exists());
    }

    #[test]
    fn test_entry_points() {
        // Create a virtual environment in a temporary directory
        let tmpdir = tempdir().unwrap();
        let venv = VEnv::create(tmpdir.path(), PythonLocation::System).unwrap();

        // Download our wheel file and install it in the virtual environment we just created
        let package_path = test_utils::download_and_cache_file(
            "https://files.pythonhosted.org/packages/29/a2/76daec910034d765f1018d22660c0970fb99f77143a42841d067b522903e/cowpy-1.1.5-py3-none-any.whl".parse().unwrap(),
            "de5ae7646dd30b4936013666c6bd019af9cf411cc3b377c8538cfd8414262921").unwrap();
        let wheel = Wheel::from_path(&package_path, &"cowpy".parse().unwrap()).unwrap();
        venv.install_wheel(&wheel, &Default::default()).unwrap();

        // Determine the location of the installed script
        let script_name = if venv.install_paths().is_windows() {
            "cowpy.exe"
        } else {
            "cowpy"
        };
        let script_path = venv
            .root()
            .join(venv.install_paths().scripts())
            .join(script_name);

        // Execute the script
        let output = std::process::Command::new(script_path)
            .arg("--list-eyes")
            .output()
            .unwrap();

        if !output.status.success() {
            panic!(
                "failed to execute script: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        insta::assert_snapshot!(stdout);
    }

    async fn download_best_ruff_wheel() -> PathBuf {
        download_best_matching_wheel("ruff",
            &[
            ("https://files.pythonhosted.org/packages/00/45/0907965db0e7640d8695a8c22fd8beed865fb21553359fa03d9ca71560e1/ruff-0.1.0-py3-none-macosx_10_7_x86_64.whl", "87114e254dee35e069e1b922d85d4b21a5b61aec759849f393e1dbb308a00439"),
            ("https://files.pythonhosted.org/packages/55/4b/ac3b1c94eaa9039108bde3882bf3edb01c3ed98de5a3e95c10d3229a49ea/ruff-0.1.0-py3-none-macosx_10_9_x86_64.macosx_11_0_arm64.macosx_10_9_universal2.whl", "764f36d2982cc4a703e69fb73a280b7c539fd74b50c9ee531a4e3fe88152f521"),
            ("https://files.pythonhosted.org/packages/e2/cd/02ba37dc8f45a5a3c79969cddc869f4bf1fa0d1a97c234e04b99fb5990e9/ruff-0.1.0-py3-none-manylinux_2_17_aarch64.manylinux2014_aarch64.whl", "65f4b7fb539e5cf0f71e9bd74f8ddab74cabdd673c6fb7f17a4dcfd29f126255"),
            ("https://files.pythonhosted.org/packages/c9/3d/f25c2e2e08e94699999a1a79faaf8a1a5afd7bf75f9083fb72f28c953bae/ruff-0.1.0-py3-none-manylinux_2_17_armv7l.manylinux2014_armv7l.whl", "299fff467a0f163baa282266b310589b21400de0a42d8f68553422fa6bf7ee01"),
            ("https://files.pythonhosted.org/packages/29/ac/a730ea13a1b94a897f1eb843711176e076b1730f586beec5dd6761833d13/ruff-0.1.0-py3-none-manylinux_2_17_i686.manylinux2014_i686.whl", "0d412678bf205787263bb702c984012a4f97e460944c072fd7cfa2bd084857c4"),
            ("https://files.pythonhosted.org/packages/ef/18/a9f77c44fe3f8c481e414307f8c891fd2c70fb52112d18734b1eec660e9b/ruff-0.1.0-py3-none-manylinux_2_17_ppc64.manylinux2014_ppc64.whl", "a5391b49b1669b540924640587d8d24128e45be17d1a916b1801d6645e831581"),
            ("https://files.pythonhosted.org/packages/5b/bf/8795534dffc59cc18c7a363b9db48af23cd8338108f59abf5e72899cea1e/ruff-0.1.0-py3-none-manylinux_2_17_ppc64le.manylinux2014_ppc64le.whl", "ee8cd57f454cdd77bbcf1e11ff4e0046fb6547cac1922cc6e3583ce4b9c326d1"),
            ("https://files.pythonhosted.org/packages/c0/64/8835980bfb0dddccb1e75d12b6372610ea39a594f5dc931e38d8fa15a381/ruff-0.1.0-py3-none-manylinux_2_17_s390x.manylinux2014_s390x.whl", "fa7aeed7bc23861a2b38319b636737bf11cfa55d2109620b49cf995663d3e888"),
            ("https://files.pythonhosted.org/packages/ac/22/0fc6119373ee9335a6ff41761eff4997e45c4773555100d150d4efba7395/ruff-0.1.0-py3-none-manylinux_2_17_x86_64.manylinux2014_x86_64.whl", "b04cd4298b43b16824d9a37800e4c145ba75c29c43ce0d74cad1d66d7ae0a4c5"),
            ("https://files.pythonhosted.org/packages/ed/df/285f1ab2028a29e402da421eeb6523d56153d3a5f9f9d4e4e5df4e0a9ab7/ruff-0.1.0-py3-none-musllinux_1_2_aarch64.whl", "7186ccf54707801d91e6314a016d1c7895e21d2e4cd614500d55870ed983aa9f"),
            ("https://files.pythonhosted.org/packages/fc/36/fd2d66b1e58a3dfb9211795ee060ecda9aa6e5ded5312e7a20f110f1bbd1/ruff-0.1.0-py3-none-musllinux_1_2_armv7l.whl", "d88adfd93849bc62449518228581d132e2023e30ebd2da097f73059900d8dce3"),
            ("https://files.pythonhosted.org/packages/03/0a/d5df874a40fa3eae09626e072f4b1580b51025b964f699170404277678ed/ruff-0.1.0-py3-none-musllinux_1_2_i686.whl", "ad2ccdb3bad5a61013c76a9c1240fdfadf2c7103a2aeebd7bcbbed61f363138f"),
            ("https://files.pythonhosted.org/packages/84/45/fd7cad3391108f5e4189af607f20c82eb3be85c7243162252ffb97e1e42c/ruff-0.1.0-py3-none-musllinux_1_2_x86_64.whl", "b77f6cfa72c6eb19b5cac967cc49762ae14d036db033f7d97a72912770fd8e1c"),
            ("https://files.pythonhosted.org/packages/cc/12/7e37f538bf393a8df563d9b149631116a6a3d0ee3495e2ba224838dfbade/ruff-0.1.0-py3-none-win32.whl", "480bd704e8af1afe3fd444cc52e3c900b936e6ca0baf4fb0281124330b6ceba2"),
            ("https://files.pythonhosted.org/packages/be/cd/da574980bf389f632a9da89aaa5baa5199a1b8860a1cf70a5b2e9a14c083/ruff-0.1.0-py3-none-win_amd64.whl", "a76ba81860f7ee1f2d5651983f87beb835def94425022dc5f0803108f1b8bfa2"),
            ("https://files.pythonhosted.org/packages/88/79/aaf84a13905f98072c06826f85e0dbf9e8d8b7c811722cba1893d98edcfa/ruff-0.1.0-py3-none-win_arm64.whl", "45abdbdab22509a2c6052ecf7050b3f5c7d6b7898dc07e82869401b531d46da4")]).await
    }

    async fn download_best_matching_wheel(
        package_name: &str,
        candidates: &[(&str, &str)],
    ) -> PathBuf {
        let tags = WheelTags::from_env().await.unwrap();
        let package_name = NormalizedPackageName::from_str(package_name).unwrap();

        let (_, url, sha) = candidates
            .iter()
            .flat_map(|(url, sha)| {
                let url = Url::parse(url).unwrap();
                let file_name = url.path_segments().unwrap().last().unwrap();
                let file_name = WheelFilename::from_filename(file_name, &package_name).unwrap();
                file_name
                    .all_tags()
                    .into_iter()
                    .filter_map(|tag| tags.compatibility(&tag))
                    .map(move |compatibility| (compatibility, url.clone(), *sha))
            })
            .max_by_key(|(compatibility, _, _)| *compatibility)
            .unwrap();

        download_and_cache_file_async(url, sha).await.unwrap()
    }

    #[tokio::test]
    async fn test_scripts_with_ruff() {
        // Create a virtual environment in a temporary directory
        let tmpdir = tempdir().unwrap();
        let venv = VEnv::create(tmpdir.path(), PythonLocation::System).unwrap();

        // Download our wheel file and install it in the virtual environment we just created
        let package_path = download_best_ruff_wheel().await;
        let wheel = Wheel::from_path(&package_path, &"ruff".parse().unwrap()).unwrap();
        venv.install_wheel(&wheel, &Default::default()).unwrap();

        // Determine the location of the installed script
        let script_name = if venv.install_paths().is_windows() {
            "ruff.exe"
        } else {
            "ruff"
        };
        let script_path = venv
            .root()
            .join(venv.install_paths().scripts())
            .join(script_name);

        // Execute the script
        let output = std::process::Command::new(script_path)
            .arg("--version")
            .output()
            .unwrap();

        if !output.status.success() {
            panic!(
                "failed to execute script: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "ruff 0.1.0");
    }
}
