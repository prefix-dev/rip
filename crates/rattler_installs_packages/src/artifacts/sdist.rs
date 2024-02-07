use crate::resolve::PypiVersion;
use crate::types::{
    ArtifactFromBytes, ArtifactFromSource, HasArtifactName, NormalizedPackageName,
    ReadPyProjectError, SDistFilename, SDistFormat, SourceArtifactName,
};
use crate::types::{WheelCoreMetaDataError, WheelCoreMetadata};
use crate::utils::ReadAndSeek;
use flate2::read::GzDecoder;

use fs_err as fs;
use miette::IntoDiagnostic;

use std::ffi::OsStr;
use std::io::{ErrorKind, Read, Seek};
use std::path::{Path, PathBuf};
use tar::Archive;
use zip::ZipArchive;

/// Represents a source distribution artifact.
pub struct SDist {
    /// Name of the source distribution
    pub name: SDistFilename,

    /// Source dist archive
    file: parking_lot::Mutex<Box<dyn ReadAndSeek + Send>>,
}

#[derive(thiserror::Error, Debug)]
pub enum SDistError {
    #[error("IO error while reading PKG-INFO: {0}")]
    PkgInfoIOError(#[source] std::io::Error),

    #[error("No PKG-INFO found in archive")]
    NoPkgInfoFound,

    #[error(transparent)]
    PyProjectTomlError(#[from] ReadPyProjectError),

    #[error("Could not parse metadata")]
    WheelCoreMetaDataError(#[from] WheelCoreMetaDataError),
}

impl SDist {
    /// Create this struct from a path
    #[allow(dead_code)]
    pub fn from_path(
        path: &Path,
        normalized_package_name: &NormalizedPackageName,
    ) -> miette::Result<Self> {
        let file_name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| miette::miette!("path does not contain a filename"))?;
        let name =
            SDistFilename::from_filename(file_name, normalized_package_name).into_diagnostic()?;
        let bytes = fs::File::open(path).into_diagnostic()?;
        Self::from_bytes(name, Box::new(bytes))
    }

    /// Find entry in tar archive
    fn find_entry(&self, name: impl AsRef<Path>) -> std::io::Result<Option<Vec<u8>>> {
        let mut lock = self.file.lock();
        let archives = generic_archive_reader(&mut lock, self.name.format)?;

        fn skip_first_component(path: &Path) -> PathBuf {
            path.components().skip(1).collect()
        }

        match archives {
            Archives::TarArchive(mut archive) => {
                // Loop over entries
                for entry in archive.entries()? {
                    let mut entry = entry?;

                    // Find name in archive and return this
                    if skip_first_component(entry.path()?.as_ref()) == name.as_ref() {
                        let mut bytes = Vec::new();
                        entry.read_to_end(&mut bytes)?;
                        return Ok(Some(bytes));
                    }
                }
                Ok(None)
            }
            Archives::Zip(mut archive) => {
                // Loop over zip entries and extract zip file by index
                // If file's path is not safe, ignore it and record a warning message
                for i in 0..archive.len() {
                    let mut file = archive.by_index(i)?;
                    if let Some(file_path) = file.enclosed_name() {
                        if skip_first_component(file_path) == name.as_ref() {
                            let mut bytes = Vec::new();
                            file.read_to_end(&mut bytes)?;
                            return Ok(Some(bytes));
                        }
                    } else {
                        tracing::warn!(
                            "Ignoring {0} as it cannot be converted to a valid path",
                            file.name()
                        );
                    }
                }
                Ok(None)
            }
        }
    }

    /// Read .PKG-INFO from the archive
    pub fn read_package_info(&self) -> Result<(Vec<u8>, WheelCoreMetadata), SDistError> {
        if let Some(bytes) = self
            .find_entry("PKG-INFO")
            .map_err(SDistError::PkgInfoIOError)?
        {
            let metadata = WheelCoreMetadata::try_from(bytes.as_slice())?;

            Ok((bytes, metadata))
        } else {
            Err(SDistError::NoPkgInfoFound)
        }
    }

    /// Checks if this artifact implements PEP 643
    /// and returns the metadata if it does
    pub fn pep643_metadata(&self) -> Result<Option<(Vec<u8>, WheelCoreMetadata)>, SDistError> {
        // Assume we have a PKG-INFO
        let (bytes, metadata) = self.read_package_info()?;
        if metadata.metadata_version.implements_pep643() {
            Ok(Some((bytes, metadata)))
        } else {
            Ok(None)
        }
    }

    /// Get a lock on the inner data
    pub fn lock_data(&self) -> parking_lot::MutexGuard<Box<dyn ReadAndSeek + Send>> {
        self.file.lock()
    }
}

impl HasArtifactName for SDist {
    type Name = SDistFilename;

    fn name(&self) -> &Self::Name {
        &self.name
    }
}

impl ArtifactFromBytes for SDist {
    fn from_bytes(name: Self::Name, bytes: Box<dyn ReadAndSeek + Send>) -> miette::Result<Self> {
        Ok(Self {
            name,
            file: parking_lot::Mutex::new(bytes),
        })
    }
}

impl ArtifactFromSource for SDist {
    fn try_get_bytes(&self) -> Result<Vec<u8>, std::io::Error> {
        let mut vec = vec![];
        let mut inner = self.lock_data();
        inner.rewind()?;
        inner.read_to_end(&mut vec)?;
        Ok(vec)
    }

    fn distribution_name(&self) -> String {
        self.name().distribution.as_source_str().to_owned()
    }

    fn version(&self) -> PypiVersion {
        PypiVersion::Version {
            version: self.name().version.clone(),
            package_allows_prerelease: false,
        }
    }

    fn artifact_name(&self) -> SourceArtifactName {
        SourceArtifactName::SDist(self.name().to_owned())
    }

    fn read_build_info(&self) -> Result<pyproject_toml::BuildSystem, ReadPyProjectError> {
        if let Some(bytes) = self.find_entry("pyproject.toml")? {
            let source = String::from_utf8(bytes).map_err(|e| {
                ReadPyProjectError::PyProjectTomlParseError(format!(
                    "could not parse pyproject.toml (bad encoding): {}",
                    e
                ))
            })?;
            let project = pyproject_toml::PyProjectToml::new(&source).map_err(|e| {
                ReadPyProjectError::PyProjectTomlParseError(format!(
                    "could not parse pyproject.toml (bad toml): {}",
                    e
                ))
            })?;
            Ok(project
                .build_system
                .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "no build-system found"))?)
        } else {
            Err(ReadPyProjectError::NoPyProjectTomlFound)
        }
    }

    /// Extract the contents of the sdist archive to the given directory
    fn extract_to(&self, work_dir: &Path) -> std::io::Result<()> {
        let mut lock = self.file.lock();
        let archives = generic_archive_reader(&mut lock, self.name.format)?;
        match archives {
            Archives::TarArchive(mut archive) => {
                archive.unpack(work_dir)?;
                Ok(())
            }
            Archives::Zip(mut archive) => {
                archive.extract(work_dir)?;
                Ok(())
            }
        }
    }
}

enum RawAndGzReader<'a> {
    Raw(&'a mut Box<dyn ReadAndSeek + Send>),
    Gz(GzDecoder<&'a mut Box<dyn ReadAndSeek + Send>>),
}

impl<'a> Read for RawAndGzReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Raw(r) => r.read(buf),
            Self::Gz(r) => r.read(buf),
        }
    }
}

enum Archives<'a> {
    TarArchive(Box<Archive<RawAndGzReader<'a>>>),
    Zip(Box<ZipArchive<&'a mut Box<dyn ReadAndSeek + Send>>>),
}

fn generic_archive_reader(
    file: &mut Box<dyn ReadAndSeek + Send>,
    format: SDistFormat,
) -> std::io::Result<Archives> {
    file.rewind()?;

    match format {
        SDistFormat::TarGz => {
            let bytes = GzDecoder::new(file);
            Ok(Archives::TarArchive(Box::new(Archive::new(RawAndGzReader::Gz(bytes)))))
        }
        SDistFormat::Tar => Ok(Archives::TarArchive(Box::new(Archive::new(RawAndGzReader::Raw(file))))),
        SDistFormat::Zip => {
            let zip = ZipArchive::new(file)?;
            Ok(Archives::Zip(Box::new(zip)))
        },
        unsupported_format => Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("sdist archive format currently {unsupported_format} unsupported (only tar | tar.gz | zip are supported)"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::artifacts::SDist;
    use crate::index::{ArtifactRequest, PackageSourcesBuilder};
    use crate::python_env::Pep508EnvMakers;
    use crate::resolve::PypiVersion;
    use crate::resolve::SDistResolution;
    use crate::types::PackageName;
    use crate::types::{
        ArtifactFromSource, ArtifactInfo, ArtifactName, DistInfoMetadata, Extra, STreeFilename,
        Yanked,
    };
    use crate::wheel_builder::WheelBuilder;
    use crate::{index::PackageDb, resolve::ResolveOptions};
    use insta::{assert_debug_snapshot, assert_ron_snapshot};
    use pep440_rs::Version;
    use reqwest::Client;
    use reqwest_middleware::ClientWithMiddleware;
    use std::collections::{HashMap, HashSet};
    use std::env;
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use url::Url;

    fn get_package_db() -> (Arc<PackageDb>, TempDir) {
        let tempdir = tempfile::tempdir().unwrap();
        let client = ClientWithMiddleware::from(Client::new());

        let url = url::Url::parse("https://pypi.org/simple/").unwrap();
        let sources = PackageSourcesBuilder::new(url).build().unwrap();

        (
            Arc::new(PackageDb::new(sources, client, tempdir.path()).unwrap()),
            tempdir,
        )
    }

    #[tokio::test]
    pub async fn correct_metadata_fake_flask() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/fake-flask-3.0.0.tar.gz");

        let sdist = SDist::from_path(&path, &"fake-flask".parse().unwrap()).unwrap();
        // Should not fail as it is a valid PKG-INFO
        // and considered reliable
        let _package_db = get_package_db();
        sdist.pep643_metadata().unwrap().unwrap();
    }

    #[test]
    pub fn read_rich_build_info() {
        // Read path
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        // Load sdist
        let sdist = super::SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

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
    pub async fn sdist_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let result = wheel_builder
            .get_sdist_metadata::<SDist>(&sdist)
            .await
            .unwrap();

        assert_debug_snapshot!(result.1);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_with_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        // Build the wheel
        wheel_builder.get_sdist_metadata(&sdist).await.unwrap();
        let wheel = wheel_builder.build_wheel(&sdist).await.unwrap();

        let (_, metadata) = wheel.metadata().unwrap();
        assert_debug_snapshot!(metadata);
    }
    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_no_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        // Build the wheel
        let wheel = wheel_builder.build_wheel(&sdist).await.unwrap();

        let (_, metadata) = wheel.metadata().unwrap();
        assert_debug_snapshot!(metadata);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_wheel_and_pass_env_variables() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/env_package-0.1.tar.gz");

        let sdist = SDist::from_path(&path, &"env_package".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let resolve_options = ResolveOptions {
            ..Default::default()
        };

        let mut mandatory_env = HashMap::new();

        // In order to build wheel, we need to pass specific ENV that setup.py expect
        mandatory_env.insert("MY_ENV_VAR".to_string(), "SOME_VALUE".to_string());

        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            resolve_options,
            mandatory_env,
        )
        .unwrap();

        // Build the wheel
        let wheel = wheel_builder.build_wheel(&sdist).await.unwrap();

        let (_, metadata) = wheel.metadata().unwrap();
        assert_debug_snapshot!(metadata);
    }

    // On windows these tests will fail because python interpreter
    // should have SYSTEMROOT
    // https://github.com/pyinstaller/pyinstaller/issues/6878
    #[cfg(not(target_os = "windows"))]
    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_wheel_and_with_clean_env_and_pass_env_variables() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/env_package-0.1.tar.gz");

        let sdist = SDist::from_path(&path, &"env_package".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let resolve_options = ResolveOptions {
            clean_env: true,
            ..Default::default()
        };

        let mut mandatory_env = HashMap::new();

        // In order to build wheel, we need to pass specific ENV that setup.py expect
        mandatory_env.insert(String::from("MY_ENV_VAR"), String::from("SOME_VALUE"));

        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            resolve_options,
            mandatory_env,
        )
        .unwrap();

        // Build the wheel
        let wheel = wheel_builder.build_wheel(&sdist).await.unwrap();

        let (_, metadata) = wheel.metadata().unwrap();
        assert_debug_snapshot!(metadata);
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_wheel_and_will_fail_when_clean_env_is_used() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/env_package-0.1.tar.gz");

        let sdist = SDist::from_path(&path, &"env_package".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let resolve_options = ResolveOptions {
            clean_env: true,
            ..Default::default()
        };

        // Do not pass any mandatory env for wheel builder, and do not inherit
        // this should fail
        let mandatory_env = HashMap::new();

        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            resolve_options,
            mandatory_env,
        )
        .unwrap();

        // Build the wheel
        let wheel = wheel_builder.build_wheel(&sdist).await;
        let err_string = wheel.err().unwrap().to_string();

        assert!(err_string.contains("could not build wheel"));
        assert!(err_string.contains("MY_ENV_VAR should be set in order to build wheel"));
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn read_zip_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/filterpy-1.4.5.zip");

        let sdist = SDist::from_path(&path, &"filterpy".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        );

        let result = wheel_builder
            .unwrap()
            .get_sdist_metadata(&sdist)
            .await
            .unwrap();

        assert_debug_snapshot!(result.1);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn read_zip_archive_for_a_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/zip_read_package-1.0.0.zip");

        let sdist = SDist::from_path(&path, &"zip_read_package".parse().unwrap()).unwrap();

        let content = sdist.find_entry("test_file.txt").unwrap().unwrap();
        let content_text = String::from_utf8(content).unwrap();

        assert!(content_text.contains("hello world"));

        let content = sdist
            .find_entry("inner_folder/inner_file.txt")
            .unwrap()
            .unwrap();
        let content_text = String::from_utf8(content).unwrap();

        assert!(content_text.contains("hello inner world"));
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn read_tar_gz_archive_for_a_file() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let pkg_info = sdist.find_entry("PKG-INFO").unwrap().unwrap();
        let pkg_info_text = String::from_utf8(pkg_info).unwrap();
        assert_debug_snapshot!(pkg_info_text);

        let init_file = sdist.find_entry("rich/__init__.py").unwrap().unwrap();
        let init_file_text = String::from_utf8(init_file).unwrap();
        assert_debug_snapshot!(init_file_text);
    }

    #[tracing_test::traced_test]
    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_wheel_with_backend_path() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/setuptools-69.0.2.tar.gz");

        let sdist = SDist::from_path(&path, &"setuptools".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let resolve_options = ResolveOptions {
            sdist_resolution: SDistResolution::OnlySDists,
            ..Default::default()
        };

        let wheel_builder = WheelBuilder::new(
            package_db.0,
            env_markers,
            None,
            resolve_options,
            HashMap::default(),
        )
        .unwrap();

        // Build the wheel
        let wheel = wheel_builder.build_wheel(&sdist).await.unwrap();

        let (_, metadata) = wheel.metadata().unwrap();
        let mut metadata = metadata.clone();
        let extras: HashSet<Extra> = HashSet::from_iter(vec![
            Extra::from_str("certs").unwrap(),
            Extra::from_str("ssl").unwrap(),
            Extra::from_str("testing-integration").unwrap(),
            Extra::from_str("docs").unwrap(),
            Extra::from_str("testing").unwrap(),
        ]);
        assert_eq!(metadata.extras, extras);

        // hashset does not have a deterministic order
        metadata.extras = Default::default();
        assert_debug_snapshot!(metadata);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_sdist_as_source_dependency() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-data/sdists/rich-13.6.0.tar.gz");

        let url = Url::from_file_path(path.canonicalize().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let content = package_db
            .0
            .available_artifacts(ArtifactRequest::DirectUrl {
                name: norm_name.into(),
                url: url.clone(),
                wheel_builder: &wheel_builder,
            })
            .await
            .unwrap();
        let artifact_info = content.get(&PypiVersion::Url(url)).unwrap();

        assert_debug_snapshot!(artifact_info[0].filename);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_sdist_but_without_metadata_in_path_as_source_dependency() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/sdists/rich_without_metadata_in_path.tar.gz");

        let url = Url::from_file_path(path.canonicalize().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let content = package_db
            .0
            .available_artifacts(ArtifactRequest::DirectUrl {
                name: norm_name.into(),
                url: url.clone(),
                wheel_builder: &wheel_builder,
            })
            .await
            .unwrap();
        let artifact_info = content.get(&PypiVersion::Url(url)).unwrap();

        assert_debug_snapshot!(artifact_info[0].filename);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_as_folder_as_source_dependency() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/stree/dev_folder_with_rich");

        let url = Url::from_file_path(path.canonicalize().unwrap()).unwrap();

        // let sdist = SDist::from_path(&path, &"rich".parse().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let content = package_db
            .0
            .available_artifacts(ArtifactRequest::DirectUrl {
                name: norm_name.into(),
                url: url.clone(),
                wheel_builder: &wheel_builder,
            })
            .await
            .unwrap();
        let artifact_info = content.get(&PypiVersion::Url(url)).unwrap();

        assert_debug_snapshot!(artifact_info[0].filename.as_stree().unwrap().distribution);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_http_reference_source_code() {
        let url =
            Url::parse("https://github.com/Textualize/rich/archive/refs/tags/v13.7.0.zip").unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let content = package_db
            .0
            .available_artifacts(ArtifactRequest::DirectUrl {
                name: norm_name.into(),
                url: url.clone(),
                wheel_builder: &wheel_builder,
            })
            .await
            .unwrap();

        let artifact_info = content
            .iter()
            .flat_map(|(_, artifacts)| artifacts.iter().cloned())
            .collect::<Vec<_>>();

        let wheel_metadata = package_db
            .0
            .get_metadata(artifact_info.as_slice(), None)
            .await
            .unwrap()
            .unwrap();

        // assert_debug_snapshot!(wheel_metadata.0);
        assert_debug_snapshot!(wheel_metadata.1);

        // assert_debug_snapshot!(artifact_info[0].filename);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_git_reference_source_code() {
        let url = Url::parse("git+https://github.com/Textualize/rich.git").unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let content = package_db
            .0
            .available_artifacts(ArtifactRequest::DirectUrl {
                name: norm_name.into(),
                url: url.clone(),
                wheel_builder: &wheel_builder,
            })
            .await
            .unwrap();
        let artifact_info = content.get(&PypiVersion::Url(url)).unwrap();

        assert_debug_snapshot!(artifact_info[0].filename);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn build_rich_git_reference_with_tag_source_code() {
        // Let's checkout some old version that have different requirements as new one
        let url = Url::parse("git+https://github.com/Textualize/rich.git@v1.0.0").unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let content = package_db
            .0
            .available_artifacts(ArtifactRequest::DirectUrl {
                name: norm_name.into(),
                url: url.clone(),
                wheel_builder: &wheel_builder,
            })
            .await
            .unwrap();

        let artifact_info = content
            .iter()
            .flat_map(|(_, artifacts)| artifacts.iter().cloned())
            .collect::<Vec<_>>();

        let wheel_metadata = package_db
            .0
            .get_metadata(artifact_info.as_slice(), None)
            .await
            .unwrap()
            .unwrap();

        assert_debug_snapshot!(wheel_metadata.1);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn get_only_metadata_for_local_stree_rich_without_calling_available_artifacts() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/stree/dev_folder_with_rich");

        let url = Url::from_file_path(path.canonicalize().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let stree_file_name = STreeFilename {
            distribution: norm_name,
            version: Version::from_str("0.0.0").unwrap(),
            url: url.clone(),
        };

        let artifact_info = vec![ArtifactInfo {
            filename: ArtifactName::STree(stree_file_name),
            url: url,
            hashes: None,
            requires_python: None,
            dist_info_metadata: DistInfoMetadata::default(),
            yanked: Yanked::default(),
        }];

        let wheel_metadata = package_db
            .0
            .get_metadata(artifact_info.as_slice(), Some(&wheel_builder))
            .await
            .unwrap()
            .unwrap();

        assert_debug_snapshot!(wheel_metadata.1);
    }

    #[tokio::test(flavor = "multi_thread")]
    pub async fn get_whl_for_local_stree_rich() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-data/stree/dev_folder_with_rich");

        let url = Url::from_file_path(path.canonicalize().unwrap()).unwrap();

        let package_db = get_package_db();
        let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);
        let wheel_builder = WheelBuilder::new(
            package_db.0.clone(),
            env_markers,
            None,
            ResolveOptions::default(),
            HashMap::default(),
        )
        .unwrap();

        let norm_name = PackageName::from_str("rich").unwrap();
        let stree_file_name = STreeFilename {
            distribution: norm_name,
            version: Version::from_str("0.0.0").unwrap(),
            url: url.clone(),
        };

        let artifact_info = ArtifactInfo {
            filename: ArtifactName::STree(stree_file_name),
            url: url,
            hashes: None,
            requires_python: None,
            dist_info_metadata: DistInfoMetadata::default(),
            yanked: Yanked::default(),
        };

        let whl = package_db
            .0
            .get_wheel(&artifact_info, Some(&wheel_builder))
            .await
            .unwrap();

        let whl_metadata = whl.metadata().unwrap();

        assert_debug_snapshot!(whl_metadata.1);
    }
}
