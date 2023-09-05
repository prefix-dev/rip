use clap::Parser;
use indexmap::IndexMap;
use miette::{Context, IntoDiagnostic};
use rattler_installs_packages::artifact_name::WheelName;
use rattler_installs_packages::core_metadata::WheelCoreMetadata;
use rattler_installs_packages::requirement::marker::EnvMarkerExpr;
use rattler_installs_packages::{
    ArtifactInfo, ArtifactName, PackageDb, PackageName, PackageRequirement, Version, Wheel,
};
use std::collections::{HashMap, HashSet, VecDeque};
use tracing::Level;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(num_args=1.., required=true)]
    specs: Vec<PackageRequirement>,
}

async fn actual_main() -> miette::Result<()> {
    let args = Args::parse();

    // Setup tracing subscriber
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .finish()
        .init();

    // Determine cache directory
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| miette::miette!("failed to determine cache directory"))?
        .join("rattler/pypi");
    tracing::info!("cache directory: {}", cache_dir.display());

    // Construct a package database
    let package_db = rattler_installs_packages::PackageDb::new(
        Default::default(),
        &[Url::parse("https://pypi.org/simple/").unwrap()],
        cache_dir.clone(),
    )
    .into_diagnostic()?;

    // Get metadata for all the packages
    let metadata = recursively_get_metadata(
        &package_db,
        args.specs.iter().map(|spec| spec.name.clone()).collect(),
    )
    .await?;

    // Count the total number of records
    let records: usize = metadata.iter().map(|(_, meta)| meta.len()).sum();

    println!("Total number of records: {}", records);

    Ok(())
}

/// Download all metadata needed to solve the specified packages.
async fn recursively_get_metadata(
    package_db: &PackageDb,
    packages: Vec<PackageName>,
) -> miette::Result<HashMap<PackageName, IndexMap<Version, (ArtifactInfo, WheelCoreMetadata)>>> {
    let mut queue = VecDeque::from_iter(packages.into_iter());
    let mut seen = HashSet::<PackageName>::from_iter(queue.iter().cloned());
    let mut package_metadata = HashMap::default();

    // TODO: https://peps.python.org/pep-0508/#environment-markers
    let env = HashMap::from_iter([
        // TODO: We should add some proper values here.
        // See: https://peps.python.org/pep-0508/#environment-markers
        ("os_name", ""),
        ("sys_platform", ""),
        ("platform_machine", ""),
        ("platform_python_implementation", ""),
        ("platform_release", ""),
        ("platform_system", ""),
        ("platform_version", ""),
        ("python_version", "3.9"),
        ("python_full_version", ""),
        ("implementation_name", ""),
        ("implementation_version", ""),
        // TODO: Add support for extras
        ("extra", ""),
    ]);

    while let Some(package) = queue.pop_front() {
        tracing::info!("Fetching metadata for {}", package.as_str());

        // Get all the metadata for this package
        let artifacts = package_db
            .available_artifacts(&package)
            .await
            .with_context(|| {
                format!(
                    "failed to fetch available artifacts for {}",
                    package.as_str()
                )
            })?;

        let mut per_version_metadata = IndexMap::default();

        // Fetch metadata per version
        for (version, artifacts) in artifacts.iter() {
            // Filter only artifacts we can work with
            let available_artifacts = artifacts
                .iter()
                .filter(|a| a.is::<Wheel>())
                .collect::<Vec<_>>();

            // Check if there are wheel artifacts for this version
            if available_artifacts.is_empty() {
                // If there are no wheel artifacts, we're just gonna skip it
                tracing::warn!(
                    "No available wheel artifact {} {version} (skipping)",
                    package.as_str()
                );
                continue;
            }

            // Filter yanked artifacts
            let non_yanked_artifacts = artifacts
                .iter()
                .filter(|a| !a.yanked.yanked)
                .collect::<Vec<_>>();

            if non_yanked_artifacts.is_empty() {
                tracing::info!("{} {version} was yanked (skipping)", package.as_str());
                continue;
            }

            let (info, metadata) = package_db
                .get_metadata::<Wheel, _>(artifacts)
                .await
                .with_context(|| {
                    format!(
                        "failed to download metadata for {} {version}",
                        package.as_str(),
                    )
                })?;

            // Iterate over all requirements and add them to the queue if we don't have information on them yet.
            for requirement in &metadata.requires_dist {
                // Evaluate environment markers
                if let Some(env_marker) = &requirement.env_marker_expr {
                    if !env_marker.eval(&env)? {
                        // tracing::info!("skipping dependency {requirement}");
                        continue;
                    }
                }

                // Add the package if we didnt see it yet.
                if !seen.contains(&requirement.name) {
                    println!(
                        "adding {} from requirement: {requirement}",
                        requirement.name.as_str()
                    );
                    queue.push_back(requirement.name.clone());
                    seen.insert(requirement.name.clone());
                }
            }

            per_version_metadata.insert(version.clone(), (info.clone(), metadata));
        }

        if per_version_metadata.is_empty() {
            tracing::error!(
                "could not find any suitable artifact for {}, does the package provide any wheels?",
                package.as_str()
            );
        }

        package_metadata.insert(package, per_version_metadata);
    }

    Ok(package_metadata)
}

#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{e:?}");
    }
}
