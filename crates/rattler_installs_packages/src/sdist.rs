use crate::core_metadata::WheelCoreMetadata;
use crate::utils::ReadAndSeek;
use crate::venv::{PythonLocation, VEnv};
use crate::{
    resolve, Artifact, MetadataArtifact, PackageDb, Pep508EnvMakers, SDistFormat, SDistName,
    UnpackWheelOptions, Wheel,
};
use flate2::read::GzDecoder;
use miette::{miette, IntoDiagnostic};
use parking_lot::Mutex;
use pep508_rs::{MarkerEnvironment, Requirement};
use serde::Serialize;
use std::ffi::OsStr;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use tar::Archive;

/// Represents a source distribution artifact.
pub struct SDist {
    /// Name of the source distribution
    name: SDistName,

    /// Source dist archive
    file: Mutex<Box<dyn ReadAndSeek + Send>>,
}

#[derive(Debug, Serialize)]
pub struct BuildSystem {
    backend_path: Vec<PathBuf>,
    build_backend: String,
}

// include static build_frontend.py string
const BUILD_FRONTEND_PY: &str = include_str!("./build_frontend.py");

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
        let mut lock = self.file.lock();
        let mut archive = Self::as_archive(&mut lock, self.name.format)?;

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

    /// Try to build the wheel file
    pub async fn build(&self, package_db: &PackageDb) -> miette::Result<()> {
        let build_info = self.read_build_info()?;

        // requires: vec!["setuptools".into(), "wheel".into()],
        // build_backend: "setuptools.build_meta:__legacy__".into(),

        let backend = build_info
            .build_backend
            .unwrap_or("setuptools.build_meta:__legacy__".to_string());

        let requirements = if build_info.requires.is_empty() {
            vec![
                Requirement::from_str("setuptools").into_diagnostic()?,
                Requirement::from_str("wheel").into_diagnostic()?,
            ]
        } else {
            build_info.requires.clone()
        };

        // create a venv
        let venv_dir = tempfile::tempdir().into_diagnostic()?;
        let venv = VEnv::create(venv_dir.path(), PythonLocation::System).into_diagnostic()?;

        // now resolve for the right wheels
        let env_markers = Pep508EnvMakers::from_env().await.into_diagnostic()?;

        let resolved_wheels = resolve(
            &package_db,
            requirements.iter(),
            &env_markers,
            None,                // compatible_tags: Option<&WheelTags>,
            Default::default(), // locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
            Default::default(), // favored_packages: HashMap<NormalizedPackageName, PinnedPackage<'db>>,
            &Default::default(), // options: &ResolveOptions,
        )
        .await?;

        let options = UnpackWheelOptions { installer: None };

        for package_info in resolved_wheels {
            let artifact_info = package_info.artifacts.first().unwrap();
            println!("Installing artifact: {}", artifact_info.filename);
            let artifact = package_db.get_artifact::<Wheel>(artifact_info).await?;
            venv.install_wheel(&artifact, &options).into_diagnostic()?;
        }

        println!(
            "Finished installing wheels into {}",
            venv_dir.path().display()
        );

        let venv_path = venv_dir.into_path();

        let work_dir = tempfile::tempdir().into_diagnostic()?;
        let work_dir = work_dir.into_path();

        self.extract_to(&work_dir)?;

        std::fs::write(work_dir.join("build_frontend.py"), BUILD_FRONTEND_PY).into_diagnostic()?;
        std::fs::write(
            work_dir.join("build-system.json"),
            serde_json::to_string_pretty(&BuildSystem {
                backend_path: Default::default(),
                build_backend: backend,
            })
            .unwrap(),
        )
        .into_diagnostic()?;

        let pkg_dir = work_dir.join(format!(
            "{}-{}",
            self.name.distribution.as_source_str(),
            self.name.version
        ));

        // three args: work_dir, goal, binary_wheel_tag
        let output = Command::new(venv.python_executable())
            .current_dir(&pkg_dir)
            .arg(work_dir.join("build_frontend.py"))
            .arg(&work_dir)
            .arg("WheelMetadata")
            .arg("xyz")
            .output()
            .into_diagnostic()?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stderr);
            return Err(miette!("failed to run build_frontend.py: {}", stdout));
        }

        let metadata_link =
            std::fs::read_to_string(work_dir.join("prepare_metadata_for_build_wheel.out"))
                .into_diagnostic()?;
        let dist_info_folder = work_dir
            .join("prepare_metadata_for_build_wheel")
            .join(metadata_link);
        let metadata_contents =
            std::fs::read(dist_info_folder.join("METADATA")).into_diagnostic()?;
        let metadata =
            WheelCoreMetadata::try_from(metadata_contents.as_slice()).into_diagnostic()?;

        println!("Metadata: {:#?}", metadata);

        Ok(())
    }

    fn as_archive<'a>(
        file: &'a mut Box<dyn ReadAndSeek + Send>,
        format: SDistFormat,
    ) -> miette::Result<Archive<SDistArchiveReader<'a>>> {
        file.rewind().into_diagnostic()?;

        match format {
            SDistFormat::TarGz => {
                let bytes = GzDecoder::new(file);
                Ok(Archive::new(SDistArchiveReader::Gz(bytes)))
            }
            SDistFormat::Tar => Ok(Archive::new(SDistArchiveReader::Raw(file))),
            _ => return Err(miette!("unsupported format {:?}", format)),
        }
    }

    fn extract_to(&self, work_dir: &Path) -> miette::Result<()> {
        let mut lock = self.file.lock();
        let mut archive = Self::as_archive(&mut lock, self.name.format)?;
        // reset archive
        archive.unpack(work_dir).into_diagnostic()?;
        Ok(())
    }
}

impl Artifact for SDist {
    type Name = SDistName;

    fn new(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        Ok(Self {
            name,
            file: Mutex::new(bytes),
        })
    }

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

enum SDistArchiveReader<'a> {
    Raw(&'a mut Box<dyn ReadAndSeek + Send>),
    Gz(GzDecoder<&'a mut Box<dyn ReadAndSeek + Send>>),
}

impl<'a> Read for SDistArchiveReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Raw(r) => r.read(buf),
            Self::Gz(r) => r.read(buf),
        }
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
        let (bytes, metadata) = self.read_package_info()?;

        // Only SDIST metadata from version 2.2 and up is considered reliable
        // Filter out older versions
        // TODO: when we have wheel building, build the wheel instead relying on this
        if !metadata.metadata_version.implements_pep643() {
            return Err(miette!("only consider SDist Metadata higher than 2.2"));
        }
        Ok((bytes, metadata))
    }
}

#[cfg(test)]
mod tests {
    use crate::sdist::SDist;
    use crate::MetadataArtifact;
    use insta::assert_ron_snapshot;
    use std::path::Path;

    #[test]
    pub fn reject_rich_metadata() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = SDist::from_path(&path).unwrap();

        // Rich has an old metadata version
        let metadata = sdist.metadata();
        assert!(metadata.is_err());
    }

    #[test]
    pub fn correct_metadata_fake_flask() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/fake-flask-3.0.0.tar.gz");

        let sdist = SDist::from_path(&path).unwrap();
        // Should not fail as it is a valid PKG-INFO
        // and considered reliable
        sdist.metadata().unwrap();
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

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = super::SDist::from_path(&path).unwrap();
        let tempdir = tempfile::tempdir().unwrap();

        let package_db = crate::PackageDb::new(
            Default::default(),
            &[url::Url::parse("https://pypi.org/simple/").unwrap()],
            tempdir.path(),
        )
        .unwrap();

        // Build the sdist
        let result = sdist.build(&package_db).await.unwrap();
        println!("result: {:#?}", result);
    }
}
