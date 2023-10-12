use crate::artifact_name::{InnerAsArtifactName, WheelName};
use crate::core_metadata::WheelCoreMetadata;
use crate::package_name::PackageName;
use crate::rfc822ish::RFC822ish;
use crate::utils::ReadAndSeek;
use crate::Version;
use async_trait::async_trait;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use zip::ZipArchive;

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

struct WheelVitals {
    dist_info: String,
    data: String,
    root_is_purelib: bool,
    metadata_blob: Vec<u8>,
    metadata: WheelCoreMetadata,
}

impl Wheel {
    /// A wheel file always contains a special directory that contains the metadata of the package.
    /// This function returns the name of that directory.
    fn find_special_wheel_dir<'a>(
        top_level_names: impl IntoIterator<Item = &'a str>,
        name: &PackageName,
        version: &Version,
        suffix: &str,
    ) -> miette::Result<Option<&'a str>> {
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
            miette::bail!("found multiple {suffix}/ directories in wheel");
        }

        Ok(Some(candidate))
    }

    fn get_vitals(&self) -> miette::Result<WheelVitals> {
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
        .ok_or_else(|| miette::miette!(".dist-info/ missing"))?
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

        let root_is_purelib = match &parsed.take_the("Root-Is-Purelib")?[..] {
            "true" => true,
            "false" => false,
            other => miette::bail!(
                "Expected 'true' or 'false' for Root-Is-Purelib, not {}",
                other,
            ),
        };

        let metadata_path = format!("{dist_info}/METADATA");
        let metadata_bytes = read_entry_to_end(&mut archive, &metadata_path)?;
        let metadata = Self::parse_metadata(&metadata_bytes)?;

        if metadata.name != self.name.distribution {
            miette::bail!(
                "name mismatch between {dist_info}/METADATA and filename ({} != {}",
                metadata.name.as_given(),
                self.name.distribution.as_source_str()
            );
        }
        if metadata.version != self.name.version {
            miette::bail!(
                "version mismatch between {dist_info}/METADATA and filename ({} != {})",
                metadata.version,
                self.name.version
            );
        }

        Ok(WheelVitals {
            dist_info: dist_info,
            data: data,
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
        WheelCoreMetadata::try_from(bytes)
    }

    fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)> {
        let WheelVitals {
            metadata_blob,
            metadata,
            ..
        } = self.get_vitals()?;
        Ok((metadata_blob, metadata))
    }
}

fn parse_format_metadata_and_check_version(
    input: &[u8],
    version_field: &str,
) -> miette::Result<RFC822ish> {
    let input: &str = std::str::from_utf8(input)?;
    let mut parsed = RFC822ish::parse(input)?;

    let version = parsed.take(version_field)?;
    if !version.starts_with("1.") {
        miette::bail!("unsupported {}: {:?}", version_field, version);
    }

    Ok(parsed)
}

/// Helper method to read a particular file from a zip archive.
fn read_entry_to_end<R: ReadAndSeek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> miette::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .map_err(|_| miette::miette!("could not find {name} in wheel file"))?
        .read_to_end(&mut bytes)
        .into_diagnostic()?;

    Ok(bytes)
}

pub trait Filesystem {
    fn mkdir(&mut self, path: &Path) -> std::io::Result<()>;
    fn write_file(
        &mut self,
        path: &Path,
        data: &mut dyn Read,
        executable: bool,
    ) -> std::io::Result<()>;
    fn write_symlink(&mut self, source: &Path, target: &Path) -> std::io::Result<()>;
}

impl<FS: Filesystem> Filesystem for &mut FS {
    fn mkdir(&mut self, path: &Path) -> std::io::Result<()> {
        self.mkdir(path)
    }

    fn write_file(
        &mut self,
        path: &Path,
        data: &mut dyn Read,
        executable: bool,
    ) -> std::io::Result<()> {
        self.write_file(path, data, executable)
    }

    fn write_symlink(&mut self, source: &Path, target: &Path) -> std::io::Result<()> {
        self.write_symlink(source, target)
    }
}

struct RootedFilesystem {
    root: PathBuf,
}

impl Filesystem for RootedFilesystem {
    fn mkdir(&mut self, path: &Path) -> std::io::Result<()> {
        debug_assert!(path.is_relative());
        std::fs::create_dir_all(self.root.join(path))
    }

    fn write_file(
        &mut self,
        path: &Path,
        data: &mut dyn Read,
        executable: bool,
    ) -> std::io::Result<()> {
        debug_assert!(path.is_relative());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if executable {
            options.mode(0o777);
        } else {
            options.mode(0o666);
        }
        let mut file = options.open(self.root.join(path))?;
        std::io::copy(data, &mut file)?;
        Ok(())
    }

    fn write_symlink(&mut self, source: &Path, target: &Path) -> std::io::Result<()> {
        debug_assert!(source.is_relative());
        debug_assert!(target.is_relative());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, self.root.join(source))
        }
        #[cfg(not(unix))]
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks not supported on this platform",
        ))
    }
}

impl Wheel {
    pub fn unpack<FS: Filesystem>(&self, mut dest: FS) -> std::io::Result<()> {
        self.met
    }
}
