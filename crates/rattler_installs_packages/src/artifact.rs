use crate::artifact_name::{InnerAsArtifactName, WheelName};
use crate::core_metadata::WheelCoreMetadata;
use crate::fs::Filesystem;
use crate::package_name::PackageName;
use crate::rfc822ish::RFC822ish;
use crate::utils::ReadAndSeek;
use crate::Version;
use async_trait::async_trait;
use miette::IntoDiagnostic;
use parking_lot::Mutex;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
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

        let root_is_purelib = match &parsed.take("Root-Is-Purelib")?[..] {
            "true" => true,
            "false" => false,
            other => miette::bail!(
                "Expected 'true' or 'false' for Root-Is-Purelib, not {}",
                other,
            ),
        };

        let metadata_path = format!("{dist_info}/METADATA");
        let metadata_blob = read_entry_to_end(&mut archive, &metadata_path)?;
        let metadata = Self::parse_metadata(&metadata_blob)?;

        if metadata.name != self.name.distribution {
            miette::bail!(
                "name mismatch between {dist_info}/METADATA and filename ({} != {}",
                metadata.name.as_source_str(),
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
    let input: &str = std::str::from_utf8(input).into_diagnostic()?;
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

impl Wheel {
    /// Unpacks a wheel to the given fileystem.
    /// TODO: Write better docs.
    /// The following functionality is still missing:
    /// - Checking and writing of RECORD file
    /// - entry_points.txt
    /// - Rewrite #!python.
    /// - Generate script wrappers.
    /// - bytecode compilation
    /// - INSTALLER (https://peps.python.org/pep-0376/#installer)
    /// - REQUESTED (https://peps.python.org/pep-0376/#requested)
    /// - direct_url.json (https://peps.python.org/pep-0610/)
    /// - support "headers" category
    pub fn unpack<FS: Filesystem>(&self, mut dest: FS, paths: &InstallPaths) -> miette::Result<()> {
        let vitals = self.get_vitals()?;

        let transformer = WheelPathTransformer {
            data: vitals.data,
            root_is_purelib: vitals.root_is_purelib,
            paths,
        };

        let mut archive = self.archive.lock();
        for index in 0..archive.len() {
            let mut zip_entry = archive.by_index(index).into_diagnostic()?;
            let relative_path = zip_entry
                .enclosed_name()
                .ok_or_else(|| miette::miette!("file {} is an invalid path", zip_entry.name()))?;

            // Determine the destination path.
            let Some((destination, is_script)) =
                transformer.analyze_path(relative_path).into_diagnostic()?
            else {
                continue;
            };

            // If the entry refers to a directory we simply create it.
            if zip_entry.is_dir() {
                dest.mkdir(&destination).into_diagnostic()?;
                continue;
            }

            // Determine if the entry is executable
            let executable = zip_entry
                .unix_mode()
                .map(|v| v & 0o0111 != 0)
                .unwrap_or(false);

            // If the file is a script
            if is_script {
                todo!("implement scripts")
            } else {
                dest.write_file(&destination, &mut zip_entry, executable)
                    .into_diagnostic()?;
            }
        }

        Ok(())
    }
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

    /// The location in the fileystem where to place files from the data directory.
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
    use crate::fs::RootedFilesystem;
    use tempfile::tempdir;

    /// A test to use as an entry point for wheel extraction.
    #[test]
    fn test_wheel_unpack() {
        let wheel = Wheel::from_path(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../test-data/wheels/miniblack-23.1.0-py3-none-any.whl"),
        )
        .unwrap();
        let tmpdir = tempdir().unwrap();

        wheel
            .unpack(
                RootedFilesystem::from(tmpdir.path()),
                &InstallPaths::for_venv((3, 8), false),
            )
            .unwrap();

        let retained_path = tmpdir.into_path();
        println!("Outputted to: {}", &retained_path.display());
    }
}
