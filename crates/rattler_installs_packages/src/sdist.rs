use crate::core_metadata::WheelCoreMetadata;
use crate::utils::ReadAndSeek;
use crate::{Artifact, MetadataArtifact, SDistFormat, SDistName};
use flate2::read::GzDecoder;
use miette::{miette, IntoDiagnostic};
use parking_lot::Mutex;
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;
use tar::Archive;

/// Represents a source distribution artifact.
pub struct SDist {
    /// Name of the source distribution
    name: SDistName,

    /// Source dist archive
    archive: Mutex<Archive<Box<dyn Read + Send>>>,
}

impl SDist {
    /// Create this struct from a path
    #[allow(dead_code)]
    pub fn from_path(path: &Path) -> miette::Result<Self> {
        let file_name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| miette::miette!("path does not contain a filename"))?;
        let name = SDistName::from_str(file_name).into_diagnostic()?;
        let bytes = std::fs::File::open(path).into_diagnostic()?;
        Self::new(name, Box::new(bytes))
    }

    /// Find entry in tar archive
    fn find_entry(&self, name: impl AsRef<str>) -> miette::Result<Option<Vec<u8>>> {
        let mut archive = self.archive.lock();
        // Loop over entries
        for entry in archive.entries().into_diagnostic()? {
            let mut entry = entry.into_diagnostic()?;

            // Find name in archive and return this
            if entry.path().into_diagnostic()?.ends_with(name.as_ref()) {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).into_diagnostic()?;
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Read .PKG-INFO from the archive
    pub fn read_package_info(&self) -> miette::Result<(Vec<u8>, WheelCoreMetadata)> {
        if let Some(bytes) = self.find_entry("PKG-INFO")? {
            let metadata = Self::parse_metadata(&bytes)?;
            Ok((bytes, metadata))
        } else {
            Err(miette!("no PKG-INFO found in archive"))
        }
    }

    /// Read the build system info from the pyproject.toml
    #[allow(dead_code)]
    pub fn read_build_info(&self) -> miette::Result<pyproject_toml::BuildSystem> {
        if let Some(bytes) = self.find_entry("pyproject.toml")? {
            let source = String::from_utf8(bytes).into_diagnostic()?;
            let project = pyproject_toml::PyProjectToml::new(&source).into_diagnostic()?;
            Ok(project
                .build_system
                .ok_or_else(|| miette!("no build-system found in pyproject.toml"))?)
        } else {
            Err(miette!("no pyproject.toml found in archive"))
        }
    }
}

impl Artifact for SDist {
    type Name = SDistName;

    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        let sdist = match name.format {
            SDistFormat::TarGz => {
                let bytes = Box::new(GzDecoder::new(bytes));
                Self {
                    name,
                    archive: Mutex::new(Archive::new(bytes)),
                }
            }
            SDistFormat::Tar => {
                let bytes: Box<dyn Read + Send> = Box::new(bytes);
                Self {
                    name,
                    archive: Mutex::new(Archive::new(bytes)),
                }
            }
            _ => return Err(miette!("unsupported format {:?}", name.format)),
        };
        Ok(sdist)
    }

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

/// We can re-use the metadata type from the wheel if the SDist has a PKG-INFO.
type SDistMetadata = WheelCoreMetadata;

impl MetadataArtifact for SDist {
    type Metadata = SDistMetadata;

    fn parse_metadata(bytes: &[u8]) -> miette::Result<Self::Metadata> {
        WheelCoreMetadata::try_from(bytes).into_diagnostic()
    }

    fn metadata(&self) -> miette::Result<(Vec<u8>, Self::Metadata)> {
        // Assume we have a PKG-INFO
        self.read_package_info()
    }
}

#[cfg(test)]
mod tests {
    use crate::MetadataArtifact;
    use insta::{assert_debug_snapshot, assert_ron_snapshot};
    use std::path::Path;

    #[test]
    pub fn read_rich_metadata() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = super::SDist::from_path(&path).unwrap();

        let metadata = sdist.metadata().unwrap().1;
        assert_debug_snapshot!(metadata);
    }

    #[test]
    pub fn read_rich_build_info() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = super::SDist::from_path(&path).unwrap();

        let build_system = sdist.read_build_info().unwrap();

        assert_ron_snapshot!(build_system, @r###"
        BuildSystem(
          requires: [
            "poetry-core >=1.0.0",
          ],
          r#build-backend: Some("poetry.core.masonry.api"),
          r#backend-path: None,
        )
        "###);
    }
}
