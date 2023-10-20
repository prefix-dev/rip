use crate::{
    artifact_name::{InnerAsArtifactName, WheelName},
    core_metadata::{WheelCoreMetaDataError, WheelCoreMetadata},
    package_name::PackageName,
    record::{Record, RecordEntry},
    rfc822ish::RFC822ish,
    utils::ReadAndSeek,
    Version,
};
use async_trait::async_trait;
use data_encoding::BASE64URL_NOPAD;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use rattler_digest::Sha256;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fs,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;
use zip::{read::ZipFile, result::ZipError, ZipArchive};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Trait that represents an artifact type in the PyPI ecosystem.
/// Currently implemented for [`Wheel`] files.
#[async_trait]
pub trait Artifact: Sized {
    /// The name of the artifact which describes the artifact.
    ///
    /// Artifacts are describes by a string. [`super::artifact_name::ArtifactName`] describes the
    /// general format.
    type Name: Clone + InnerAsArtifactName;

    /// Construct a new artifact from the given bytes
    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self>;

    /// Returns the name of this instance
    fn name(&self) -> &Self::Name;
}

/// Wheel file in the PyPI ecosystem.
/// See the [Reference Page](https://packaging.python.org/en/latest/specifications/binary-distribution-format/#binary-distribution-format)
/// for more information.
pub struct Wheel {
    name: WheelName,
    archive: Mutex<ZipArchive<Box<dyn ReadAndSeek + Send>>>,
}

impl Wheel {
    /// Open a wheel by reading a file on disk.
    pub fn from_path(path: &Path) -> miette::Result<Self> {
        let file_name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| miette::miette!("path does not contain a filename"))?;
        let wheel_name = WheelName::from_str(file_name).into_diagnostic()?;
        let file = File::open(path).into_diagnostic()?;
        Self::new(wheel_name, Box::new(file))
    }
}

impl Artifact for Wheel {
    type Name = WheelName;

    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        Ok(Self {
            name,
            archive: Mutex::new(ZipArchive::new(bytes).into_diagnostic()?),
        })
    }

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

/// Trait that represents an artifact that contains metadata.
/// Currently implemented for [`Wheel`] files.
pub trait MetadataArtifact: Artifact {
    /// Associated type for the metadata of this artifact.
    type Metadata;

    /// Parses the metadata associated with an artifact.
    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata>;

    /// Parses the metadata from the artifact itself. Also returns the metadata bytes so we can
    /// cache it for later.
    fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)>;
}

#[allow(dead_code)]
struct WheelVitals {
    dist_info: String,
    data: String,
    root_is_purelib: bool,
    metadata_blob: Vec<u8>,
    metadata: WheelCoreMetadata,
}

#[derive(Debug, Error)]
pub enum WheelVitalsError {
    #[error(".dist-info/ missing")]
    DistInfoMissing,

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

    #[error("missing key from WHEEL '{0}'")]
    MissingKeyInWheel(String),
}

impl WheelVitalsError {
    pub fn from_zip(file: String, err: ZipError) -> Self {
        match err {
            ZipError::Io(err) => WheelVitalsError::IoError(err),
            _ => WheelVitalsError::ZipError(file, err),
        }
    }
}

impl Wheel {
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
                .into())
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
}

#[async_trait]
impl MetadataArtifact for Wheel {
    type Metadata = WheelCoreMetadata;

    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata> {
        WheelCoreMetadata::try_from(bytes).into_diagnostic()
    }

    fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)> {
        let WheelVitals {
            metadata_blob,
            metadata,
            ..
        } = self.get_vitals().into_diagnostic()?;
        Ok((metadata_blob, metadata))
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

/// A dictionary of installation categories to where they should be stored relative to the
/// installation destination.
#[derive(Debug, Clone)]
pub struct InstallPaths {
    /// Mapping from category to installation path
    pub mapping: HashMap<String, PathBuf>,
}

impl InstallPaths {
    /// Populates mappings of installation targets for a virtualenv layout. The mapping depends on
    /// the python version and whether or not the installation targets windows. Specifucally on
    /// windows some of the paths are different. :shrug:
    pub fn for_venv(python_version: (u32, u32), windows: bool) -> Self {
        let site_packages = if windows {
            Path::new("Lib").join("site-packages")
        } else {
            Path::new("lib").join(format!(
                "python{}.{}/site-packages",
                python_version.0, python_version.1
            ))
        };
        Self {
            mapping: HashMap::from([
                (
                    String::from("scripts"),
                    if windows {
                        PathBuf::from("Scripts")
                    } else {
                        PathBuf::from("bin")
                    },
                ),
                // purelib and platlib locations are not relevant when using venvs
                // https://stackoverflow.com/a/27882460/3549270
                (String::from("purelib"), site_packages.clone()),
                (String::from("platlib"), site_packages),
                // Move the content of the folder to the root of the venv
                (String::from("data"), PathBuf::from("")),
            ]),
        }
    }
}

#[derive(Debug, Error)]
pub enum UnpackError {
    #[error(transparent)]
    FailedToParseWheelVitals(#[from] WheelVitalsError),

    #[error("missing installation path for {0}")]
    MissingInstallPath(String),

    #[error("Failed to read the wheel file {0}")]
    ZipError(String, #[source] ZipError),

    #[error(transparent)]
    IoError(#[from] std::io::Error),

    #[error("RECORD file is invalid")]
    RecordCsv(#[from] csv::Error),

    #[error("RECORD file doesn't match wheel contents: {0}")]
    RecordFile(String),
}

impl UnpackError {
    pub fn from_zip_error(file: String, error: ZipError) -> Self {
        match error {
            ZipError::Io(err) => Self::IoError(err),
            _ => Self::ZipError(file, error),
        }
    }
}

impl Wheel {
    /// Unpacks a wheel to the given filesystem.
    /// TODO: Write better docs.
    /// The following functionality is still missing:
    /// - entry_points.txt
    /// - Rewrite #!python.
    /// - Generate script wrappers.
    /// - bytecode compilation
    /// - INSTALLER (<https://peps.python.org/pep-0376/#installer>)
    /// - REQUESTED (<https://peps.python.org/pep-0376/#requested>)
    /// - direct_url.json (<https://peps.python.org/pep-0610/>)
    /// - support "headers" category
    pub fn unpack(&self, dest: &Path, paths: &InstallPaths) -> Result<(), UnpackError> {
        let vitals = self
            .get_vitals()
            .map_err(UnpackError::FailedToParseWheelVitals)?;

        let transformer = WheelPathTransformer {
            data: vitals.data,
            root_is_purelib: vitals.root_is_purelib,
            paths,
        };
        let site_packages = dest.join(
            paths
                .mapping
                .get("purelib")
                .ok_or_else(|| UnpackError::MissingInstallPath(String::from("purelib")))?
                .as_path(),
        );

        let mut archive = self.archive.lock();

        // Read the RECORD file from the wheel
        let record_filename = format!("{}/RECORD", &vitals.dist_info);
        let record = Record::from_reader(
            &mut archive
                .by_name(&record_filename)
                .map_err(|err| WheelVitalsError::from_zip(record_filename.clone(), err))?,
        )?;
        let record_relative_path = Path::new(&record_filename);

        let mut resulting_records = Vec::new();
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
                fs::create_dir_all(&destination)?;
                continue;
            }

            // Determine if the entry is executable
            let executable = zip_entry
                .unix_mode()
                .map(|v| v & 0o0111 != 0)
                .unwrap_or(false);

            // If the file is a script
            let (size, encoded_hash) = if is_script {
                todo!("implement scripts");
            } else {
                // Otherwise copy the file to its final destination.
                write_wheel_file(&mut zip_entry, &destination, executable)?
            };

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

        // Add the RECORD file itself to the records
        resulting_records.push(RecordEntry {
            path: record_relative_path.display().to_string(),
            hash: None,
            size: None,
        });

        // Write the resulting RECORD file
        Record::from_iter(resulting_records)
            .write_to_path(&site_packages.join(record_relative_path))?;

        Ok(())
    }
}

/// Write a file from a wheel archive to disk.
fn write_wheel_file(
    mut zip_entry: &mut ZipFile,
    destination: &PathBuf,
    _executable: bool,
) -> Result<(Option<u64>, Option<String>), UnpackError> {
    let mut reader = rattler_digest::HashingReader::<_, Sha256>::new(&mut zip_entry);

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    if _executable {
        options.mode(0o777);
    } else {
        options.mode(0o666);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = options.open(destination)?;
    let size = std::io::copy(&mut reader, &mut file)?;
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
}

impl<'a> WheelPathTransformer<'a> {
    /// Given a path from a wheel zip, analyze the path and determine its final destination path.
    ///
    /// Returns `None` if the path should be ignored.
    fn analyze_path(&self, path: &Path) -> std::io::Result<Option<(PathBuf, bool)>> {
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

        let basepath = self.paths.mapping.get(category.as_ref()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                format!("unrecognized wheel file category {category}"),
            )
        })?;

        Ok(Some((basepath.join(rest_of_path), category == "scripts")))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use rstest::rstest;
    use tempfile::tempdir;
    use url::Url;

    #[rstest]
    #[case("https://files.pythonhosted.org/packages/58/76/705b5c776f783d1ba7c630347463d4ae323282bbd859a8e9420c7ff79581/selenium-4.1.0-py3-none-any.whl", "27e7b64df961d609f3d57237caa0df123abbbe22d038f2ec9e332fb90ec1a939")]
    #[case("https://files.pythonhosted.org/packages/1e/27/47f73510c6b80d1ff0829474947537ae9ab8d516cc48c6320b7f3677fa54/selenium-2.53.2-py2.py3-none-any.whl", "fa8333cf3013497e60d87ba68cae65ead8e7fa208be88ab9c561556103f540ef")]
    fn test_wheels(#[case] url: Url, #[case] sha256: &str) {
        let name = url
            .path_segments()
            .into_iter()
            .flatten()
            .last()
            .map(ToOwned::to_owned);
        let path = test_utils::download_and_cache_file(url, sha256).unwrap();

        test_wheel_unpack(name, path);
    }

    #[test]
    fn test_wheel_platlib_and_purelib() {
        test_wheel_unpack(
            None,
            Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../test-data/wheels/purelib_and_platlib-1.0.0-cp38-cp38-linux_x86_64.whl",
            ),
        );
    }

    #[test]
    fn test_wheel_miniblack() {
        test_wheel_unpack(
            None,
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-data/wheels/miniblack-23.1.0-py3-none-any.whl"),
        );
    }

    fn test_wheel_unpack(name: Option<String>, path: PathBuf) {
        let wheel = Wheel::from_path(&path).unwrap();
        let tmpdir = tempdir().unwrap();

        // Get the wheel vitals
        let vitals = wheel.get_vitals().unwrap();

        // Construct the path lookup to install packages to
        let install_paths = InstallPaths::for_venv((3, 8), false);

        // Unpack the wheel
        wheel.unpack(tmpdir.path(), &install_paths).unwrap();

        // Determine the location where we would expect the RECORD file to exist
        let record_path = Path::new(install_paths.mapping.get("purelib").unwrap())
            .join(format!("{}/RECORD", vitals.dist_info,));
        let record_content = std::fs::read_to_string(&tmpdir.path().join(&record_path)).expect(
            &format!("failed to read RECORD from {}", record_path.display()),
        );

        if let Some(name) = name {
            insta::assert_snapshot!(name, record_content);
        } else {
            insta::assert_snapshot!(record_content);
        }
    }
}
