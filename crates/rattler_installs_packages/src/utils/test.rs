use std::sync::Arc;

use reqwest::Client;
use reqwest_middleware::ClientWithMiddleware;
use tempfile::TempDir;

use crate::{
    index::{PackageDb, PackageSourcesBuilder},
    python_env::Pep508EnvMakers,
    resolve::solve_options::ResolveOptions,
    wheel_builder::WheelBuilder,
};

pub fn get_package_db() -> (Arc<PackageDb>, TempDir) {
    let tempdir = tempfile::tempdir().unwrap();
    let client = ClientWithMiddleware::from(Client::new());

    let url = url::Url::parse("https://pypi.org/simple/").unwrap();
    let sources = PackageSourcesBuilder::new(url).build().unwrap();

    (
        Arc::new(PackageDb::new(sources, client, tempdir.path(), Default::default()).unwrap()),
        tempdir,
    )
}

// Setup the test environment
pub async fn setup(resolve_options: ResolveOptions) -> (Arc<WheelBuilder>, TempDir) {
    let (package_db, tempdir) = get_package_db();
    let env_markers = Arc::new(Pep508EnvMakers::from_env().await.unwrap().0);

    (
        WheelBuilder::new(
            package_db.clone(),
            env_markers.clone(),
            None,
            resolve_options,
        )
        .unwrap(),
        tempdir,
    )
}
