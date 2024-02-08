#![cfg(feature = "resolvo")]

use pep508_rs::{MarkerEnvironment, Requirement};
use rattler_installs_packages::resolve::solve_options::{ResolveOptions, SDistResolution};
use rattler_installs_packages::{
    index::PackageDb,
    python_env::{WheelTag, WheelTags},
    resolve::resolve,
    resolve::PinnedPackage,
    types::NormalizedPackageName,
};
use std::{collections::HashMap, path::Path, str::FromStr, sync::OnceLock};

#[tokio::test(flavor = "multi_thread")]
async fn no_sdists() {
    let error = ResolveBuilder::default()
        .with_requirement("sdist")
        .with_sdist_resolution(SDistResolution::OnlyWheels)
        .resolve()
        .await
        .unwrap_err();

    insta::assert_display_snapshot!(error)
}

#[tokio::test(flavor = "multi_thread")]
async fn local_sdists() {
    let res = ResolveBuilder::default()
        .with_requirement(
            "rich@file:///Users/graf/projects/oss/rip/test-data/sdists/rich-13.6.0.tar.gz",
        )
        .resolve()
        .await
        .unwrap();

    insta::assert_display_snapshot!(error)
}

/// Tests that the `SDistResolution::PreferWheels` option selects the highest version with a wheel
/// over any version with an sdist.
///
/// PySDL2 is a package that has newer versions that only have sdists available.
#[tokio::test(flavor = "multi_thread")]
async fn prefer_wheel() {
    let result = ResolveBuilder::default()
        .with_requirement("pysdl2")
        .with_sdist_resolution(SDistResolution::PreferWheels)
        .resolve()
        .await;

    // Get the pysdl2 package from the resolution
    let packages = result.expect("expected a valid solution");
    let pysdl_pkg = packages
        .iter()
        .find(|p| p.name.as_str() == "pysdl2")
        .unwrap();

    // A version should be selected that has wheels (which is not the latest version!)
    assert_eq!(pysdl_pkg.version.to_string(), "0.9.12");
}

/// Returns a package database that uses pypi as its index. The cache directory is stored in the
/// `target/` folder to make it easier to share the cache between tests.
/// TODO: Instead of relying on the public mutable pypi index, it would be very nice to have a copy
///  locally that we can run tests against.
fn package_database() -> &'static PackageDb {
    static PACKAGE_DB: OnceLock<PackageDb> = OnceLock::new();
    PACKAGE_DB.get_or_init(|| {
        PackageDb::new(
            Default::default(),
            &["https://pypi.org/simple/".parse().unwrap()],
            &Path::new(env!("CARGO_TARGET_TMPDIR")).join("pypi-cache"),
        )
        .unwrap()
    })
}

/// Returns a `MarkerEnvironment` instance for a Windows system.
pub fn win_environment_markers() -> MarkerEnvironment {
    MarkerEnvironment {
        implementation_name: "cpython".to_string(),
        implementation_version: "3.10.4".parse().unwrap(),
        os_name: "nt".to_string(),
        platform_machine: "AMD64".to_string(),
        platform_python_implementation: "CPython".to_string(),
        platform_release: "10".to_string(),
        platform_system: "Windows".to_string(),
        platform_version: "10.0.22635".to_string(),
        python_full_version: "3.10.4".parse().unwrap(),
        python_version: "3.10".parse().unwrap(),
        sys_platform: "win32".to_string(),
    }
}

/// Returns `WheelTags` instance for a Windows system.
pub fn win_compatible_tags() -> WheelTags {
    [
        "cp310-cp310-win_amd64",
        "cp310-abi3-win_amd64",
        "cp310-none-win_amd64",
        "cp39-abi3-win_amd64",
        "cp38-abi3-win_amd64",
        "cp37-abi3-win_amd64",
        "cp36-abi3-win_amd64",
        "cp35-abi3-win_amd64",
        "cp34-abi3-win_amd64",
        "cp33-abi3-win_amd64",
        "cp32-abi3-win_amd64",
        "py310-none-win_amd64",
        "py3-none-win_amd64",
        "py39-none-win_amd64",
        "py38-none-win_amd64",
        "py37-none-win_amd64",
        "py36-none-win_amd64",
        "py35-none-win_amd64",
        "py34-none-win_amd64",
        "py33-none-win_amd64",
        "py32-none-win_amd64",
        "py31-none-win_amd64",
        "py30-none-win_amd64",
        "cp310-none-any",
        "py310-none-any",
        "py3-none-any",
        "py39-none-any",
        "py38-none-any",
        "py37-none-any",
        "py36-none-any",
        "py35-none-any",
        "py34-none-any",
        "py33-none-any",
        "py32-none-any",
        "py31-none-any",
        "py30-none-any",
    ]
    .iter()
    .map(|s| WheelTag::from_str(s).unwrap())
    .collect()
}

/// A helper struct that makes writing tests easier. This struct allows customizing the a resolve
/// task without having to specify all the parameters required. If a parameter is not specified a
/// sane default is chosen.
#[derive(Default, Clone)]
struct ResolveBuilder {
    requirements: Vec<Requirement>,
    marker_env: Option<MarkerEnvironment>,
    compatible_tags: Option<Option<WheelTags>>,
    locked_packages: HashMap<NormalizedPackageName, PinnedPackage<'static>>,
    pinned_packages: HashMap<NormalizedPackageName, PinnedPackage<'static>>,
    options: ResolveOptions,
}

impl ResolveBuilder {
    pub fn with_requirement(mut self, req: &str) -> Self {
        let req = Requirement::from_str(req).unwrap();
        self.requirements.push(req);
        self
    }

    pub fn with_sdist_resolution(mut self, sdist_resolution: SDistResolution) -> Self {
        self.options.sdist_resolution = sdist_resolution;
        self
    }

    pub async fn resolve(self) -> Result<Vec<PinnedPackage<'static>>, String> {
        resolve(
            package_database(),
            self.requirements.iter(),
            &self.marker_env.unwrap_or(win_environment_markers()),
            self.compatible_tags
                .unwrap_or(Some(win_compatible_tags()))
                .as_ref(),
            self.locked_packages,
            self.pinned_packages,
            &self.options,
        )
        .await
        .map_err(|e| e.to_string())
    }
}
