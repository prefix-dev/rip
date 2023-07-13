use crate::artifact_name::{InnerAsArtifactName, WheelName};
use crate::core_metadata::WheelCoreMetadata;
use crate::package_name::PackageName;
use crate::utils::AsyncReadAndSeek;
use async_trait::async_trait;
use async_zip::base::read::seek::ZipFileReader;
use futures::lock::Mutex;
use miette::IntoDiagnostic;
use pep440::Version;
use std::collections::HashSet;
use std::str::FromStr;

#[async_trait]
pub trait Artifact: Sized {
    /// The name of the artifact which describes the artifact.
    ///
    /// Artifacts are describes by a string. [`super::artifact_name::ArtifactName`] describes the
    /// general format.
    type Name: Clone + InnerAsArtifactName;

    /// Construct a new artifact from the given bytes
    async fn new(
        name: Self::Name,
        bytes: Box<dyn AsyncReadAndSeek + Unpin + Send>,
    ) -> miette::Result<Self>;

    /// Returns the name of this instance
    fn name(&self) -> &Self::Name;
}

pub struct Wheel {
    name: WheelName,
    archive: Mutex<ZipFileReader<Box<dyn AsyncReadAndSeek + Unpin + Send>>>,
}

#[async_trait]
impl Artifact for Wheel {
    type Name = WheelName;

    async fn new(
        name: Self::Name,
        bytes: Box<dyn AsyncReadAndSeek + Unpin + Send>,
    ) -> miette::Result<Self> {
        Ok(Self {
            name,
            archive: Mutex::new(ZipFileReader::new(bytes).await.into_diagnostic()?),
        })
    }

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

#[async_trait]
pub trait MetadataArtifact: Artifact {
    type Metadata;

    /// Parses the metadata associated with an artifact.
    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata>;

    /// Parses the metadata from the artifact itself. Also returns the metadata bytes so we can
    /// cache it for later.
    async fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)>;
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
        let mut candidates = top_level_names
            .into_iter()
            .filter(|name| name.ends_with(suffix));

        // Get the first candidate
        let candidate = match candidates.next() {
            Some(candidate) => candidate,
            None => return Ok(None),
        };

        // Error out if there are multiple directories
        if candidates.next().is_some() {
            miette::bail!("found multiple {suffix}/ directories in wheel");
        }

        // Make sure that the candidate has the right format.
        let (candidate_package_name, candidate_version) = candidate
            .strip_suffix(suffix)
            .unwrap()
            .rsplit_once('-')
            .ok_or_else(|| {
                miette::miette!("invalid {suffix} name: could not find name and/or version")
            })?;
        let candidate_package_name = PackageName::from_str(candidate_package_name)?;
        if &candidate_package_name != name {
            miette::bail!(
                "wrong name in {candidate}{suffix}, expected {name}",
                name = name.as_str()
            );
        }

        let candidate_version = Version::parse(candidate_version)
            .ok_or_else(|| miette::miette!("failed to parse version {version}"))?;
        if &candidate_version != version {
            miette::bail!("wrong version in {candidate}{suffix}, expected {version}");
        }

        Ok(Some(candidate))
    }
}

#[async_trait]
impl MetadataArtifact for Wheel {
    type Metadata = WheelCoreMetadata;

    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata> {
        WheelCoreMetadata::try_from(bytes)
    }

    async fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)> {
        let mut archive = self.archive.lock().await;

        // Determine the top level filenames in the wheel
        let top_level_names = archive
            .file()
            .entries()
            .iter()
            .filter_map(|entry| entry.entry().filename().as_str().ok())
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

        // // Determine the name of the data directory
        // let data = Wheel::find_special_wheel_dir(
        //     top_level_names,
        //     &self.name.distribution,
        //     &self.name.version,
        //     ".data",
        // )?
        // .map_or_else(
        //     || format!("{}.data", dist_info.strip_suffix(".dist-info").unwrap()),
        //     ToOwned::to_owned,
        // );

        // let wheel_path = format!("{dist_info}/WHEEL");
        // let wheel_metadata = read_entry_to_end(&mut archive, &wheel_path).await?;
        //
        // // TODO: Verify integrity of wheel metadata

        let metadata_path = format!("{dist_info}/METADATA");
        let metadata_bytes = read_entry_to_end(&mut archive, &metadata_path).await?;
        let metadata = Self::parse_metadata(&metadata_bytes)?;

        Ok((metadata_bytes, metadata))
    }
}

/// Helper method to read a particular file from a zip archive.
async fn read_entry_to_end<T: AsyncReadAndSeek + Unpin + Send>(
    archive: &mut ZipFileReader<T>,
    name: &str,
) -> miette::Result<Vec<u8>> {
    // Locate the entry in the zip archive
    let entry_idx = archive
        .file()
        .entries()
        .iter()
        .enumerate()
        .find_map(|(idx, entry)| {
            if entry.entry().filename().as_str().ok() == Some(&name) {
                Some(idx)
            } else {
                None
            }
        })
        .ok_or_else(|| miette::miette!("could not find {name} in wheel file"))?;

    let mut bytes = Vec::new();
    archive
        .reader_with_entry(entry_idx)
        .await
        .into_diagnostic()?
        .read_to_end_checked(&mut bytes)
        .await
        .into_diagnostic()?;

    Ok(bytes)
}
