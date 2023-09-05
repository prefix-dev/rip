mod writer;

use crate::writer::{global_multi_progress, IndicatifWriter};
use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use rattler_installs_packages::requirement::Requirement;
use rattler_installs_packages::{
    NormalizedPackageName, PackageDb, PackageName, PackageRequirement, Specifiers, Version, Wheel,
};
use rattler_libsolv_rs::{
    DependencyProvider, Mapping, Pool, SolvableId, SolveJobs, Solver, VersionSet, VersionSetId,
    VersionTrait,
};
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::{Debug, Display, Formatter};
use std::io::Write;
use std::time::Duration;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
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
        .with_span_events(FmtSpan::ENTER)
        .with_writer(IndicatifWriter::new(global_multi_progress()))
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
    let mut pool = recursively_get_metadata(
        &package_db,
        args.specs.iter().map(|spec| spec.name.clone()).collect(),
        global_multi_progress(),
    )
    .await?;

    // Create a task to solve the specs passed on the command line.
    let mut jobs = SolveJobs::default();
    for Requirement {
        name, specifiers, ..
    } in args.specs.iter().map(PackageRequirement::as_inner)
    {
        let dependency_package_name = pool.intern_package_name(name.clone());
        let version_set_id =
            pool.intern_version_set(dependency_package_name, specifiers.clone().into());
        jobs.install(version_set_id);
    }

    // Solve the jobs
    let mut solver = Solver::new(pool, PypiDependencyProvider {});
    let result = solver.solve(jobs);
    let artifacts = match result {
        Err(e) => {
            eprintln!("Could not solve:\n{}", e.display_user_friendly(&solver));
            return Ok(());
        }
        Ok(transaction) => transaction
            .steps
            .into_iter()
            .map(|result| {
                let pool = solver.pool();
                let solvable = pool.resolve_solvable(result);
                let name = pool.resolve_package_name(solvable.name_id());
                (name.clone(), solvable.inner().0.clone())
            })
            .collect::<Vec<_>>(),
    };

    // Output the selected versions
    println!("{}:", console::style("Resolved environment").bold());
    for spec in args.specs.iter() {
        println!("- {}", spec);
    }

    println!();
    let mut tabbed_stdout = tabwriter::TabWriter::new(std::io::stdout());
    writeln!(
        tabbed_stdout,
        "{}\t{}",
        console::style("Name").bold(),
        console::style("Version").bold()
    )
    .into_diagnostic()?;
    for (name, artifact) in artifacts {
        writeln!(tabbed_stdout, "{name}\t{artifact}").into_diagnostic()?;
    }
    tabbed_stdout.flush().unwrap();

    Ok(())
}

/// Download all metadata needed to solve the specified packages.
async fn recursively_get_metadata(
    package_db: &PackageDb,
    packages: Vec<PackageName>,
    multi_progress: MultiProgress,
) -> miette::Result<Pool<PypiVersionSet>> {
    let mut queue = VecDeque::from_iter(packages.into_iter());
    let mut seen = HashSet::<PackageName>::from_iter(queue.iter().cloned());

    let progress_bar = multi_progress.add(ProgressBar::new(0));
    progress_bar.set_style(
        ProgressStyle::with_template("{spinner:.green} fetching metadata ({pos}/{len}) {wide_msg}")
            .unwrap(),
    );
    progress_bar.enable_steady_tick(Duration::from_millis(100));

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

    let mut pool = Pool::new();
    let repo = pool.new_repo();

    progress_bar.set_length(seen.len() as u64);

    while let Some(package) = queue.pop_front() {
        tracing::info!("Fetching metadata for {}", package.as_str());

        let package_name_id = pool.intern_package_name(package.clone());

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

        let mut num_solvables = 0;

        // Fetch metadata per version
        for (version, artifacts) in artifacts.iter() {
            // Filter only artifacts we can work with
            let available_artifacts = artifacts
                .iter()
                // We are only interested in wheels
                .filter(|a| a.is::<Wheel>())
                // TODO: How to filter prereleases correctly?
                .filter(|a| {
                    a.filename.version().pre.is_none() && a.filename.version().dev.is_none()
                })
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

            let (_, metadata) = package_db
                .get_metadata::<Wheel, _>(artifacts)
                .await
                .with_context(|| {
                    format!(
                        "failed to download metadata for {} {version}",
                        package.as_str(),
                    )
                })?;

            // TODO: Can we get rid of this clone?
            let solvable_id = pool.add_package(repo, package_name_id, PypiVersion(version.clone()));

            // Iterate over all requirements and add them to the queue if we don't have information on them yet.
            for requirement in metadata.requires_dist {
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

                // Add the dependency to the pool
                let Requirement {
                    name, specifiers, ..
                } = requirement.into_inner();
                let dependency_name_id = pool.intern_package_name(name);
                pool.add_dependency(solvable_id, dependency_name_id, specifiers.into());
            }

            num_solvables += 1;
        }

        if num_solvables == 0 {
            tracing::error!(
                "could not find any suitable artifact for {}, does the package provide any wheels?",
                package.as_str()
            );
        }

        progress_bar.set_length(seen.len() as u64);
        progress_bar.set_position(seen.len().saturating_sub(queue.len()) as u64);
        progress_bar.set_message(format!(
            "{}..",
            queue
                .iter()
                .take(10)
                .format_with(",", |p, f| f(&p.as_str()))
        ))
    }

    Ok(pool)
}

#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{e:?}");
    }
}

#[repr(transparent)]
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct PypiVersionSet(Specifiers);

impl From<Specifiers> for PypiVersionSet {
    fn from(value: Specifiers) -> Self {
        Self(value)
    }
}

impl Display for PypiVersionSet {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[repr(transparent)]
#[derive(Clone, Debug)]
struct PypiVersion(Version);

impl VersionSet for PypiVersionSet {
    type V = PypiVersion;

    fn contains(&self, v: &Self::V) -> bool {
        match self.0.satisfied_by(&v.0) {
            Err(e) => {
                tracing::error!("failed to determine if '{}' contains '{}': {e}", &self.0, v);
                false
            }
            Ok(result) => result,
        }
    }
}

impl VersionTrait for PypiVersion {
    type Name = NormalizedPackageName;
    type Version = Version;

    fn version(&self) -> Self::Version {
        self.0.clone()
    }
}

impl Display for PypiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0)
    }
}

struct PypiDependencyProvider {}

impl DependencyProvider<PypiVersionSet> for PypiDependencyProvider {
    fn sort_candidates(
        &mut self,
        pool: &Pool<PypiVersionSet>,
        solvables: &mut [SolvableId],
        _match_spec_to_candidates: &Mapping<VersionSetId, OnceCell<Vec<SolvableId>>>,
    ) {
        solvables.sort_by(|&a, &b| {
            let solvable_a = pool.resolve_solvable(a);
            let solvable_b = pool.resolve_solvable(b);

            let a = &solvable_a.inner().0;
            let b = &solvable_b.inner().0;

            // Sort in reverse order from highest to lowest.
            b.cmp(a)
        })
    }
}

#[cfg(test)]
mod test {
    use rattler_installs_packages::Version;

    #[test]
    fn valid_version() {
        assert!(Version::parse("2011k").is_some());
    }
}
