mod writer;

use crate::writer::{global_multi_progress, IndicatifWriter};
use clap::Parser;
use miette::{IntoDiagnostic};
use rattler_installs_packages::requirement::Requirement;
use rattler_installs_packages::{
    NormalizedPackageName, PackageDb, PackageRequirement, Specifiers, Version, Wheel,
};
use rattler_libsolv_rs::{Candidates, DefaultSolvableDisplay, Dependencies, DependencyProvider, NameId, Pool, SolvableId, Solver, SolverCache, VersionSet};
use std::collections::{HashMap};
use std::fmt::{Debug, Display, Formatter};
use std::io::Write;
use tokio::runtime::Handle;
use tokio::task;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(num_args=1.., required=true)]
    specs: Vec<PackageRequirement>,

    /// Base URL of the Python Package Index (default https://pypi.org/simple). This should point
    /// to a repository compliant with PEP 503 (the simple repository API).
    #[clap(default_value = "https://pypi.org/simple/", long)]
    index_url: Url,
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
#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
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

impl Display for PypiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", &self.0)
    }
}

struct PypiDependencyProvider {
    pool: Pool<PypiVersionSet, NormalizedPackageName>,
    package_db: PackageDb,
}

impl DependencyProvider<PypiVersionSet, NormalizedPackageName> for PypiDependencyProvider {
    fn pool(&self) -> &Pool<PypiVersionSet, NormalizedPackageName> {
        &self.pool
    }

    fn sort_candidates(
        &self,
        solver: &SolverCache<PypiVersionSet, NormalizedPackageName, Self>,
        solvables: &mut [SolvableId],
    ) {
        solvables.sort_by(|&a, &b| {
            let solvable_a = solver.pool().resolve_solvable(a);
            let solvable_b = solver.pool().resolve_solvable(b);

            let a = &solvable_a.inner().0;
            let b = &solvable_b.inner().0;

            // Sort in reverse order from highest to lowest.
            b.cmp(a)
        })
    }

    fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let package_name = self.pool.resolve_package_name(name);
        tracing::info!("Fetching metadata for {}", package_name.as_str());

        // Get all the metadata for this package
        let result = task::block_in_place(move || {
            Handle::current().block_on(
                self.package_db
                    .available_artifacts(&package_name.clone().into()),
            )
        });
        let artifacts = match result {
            Ok(artifacts) => artifacts,
            Err(err) => {
                tracing::error!(
                    "failed to fetch artifacts of '{package_name}': {err:?}, skipping.."
                );
                return None;
            }
        };
        let mut candidates = Candidates::default();
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
                tracing::warn!("No available wheel artifact {package_name} {version} (skipping)");
                continue;
            }

            // Filter yanked artifacts
            let non_yanked_artifacts = artifacts
                .iter()
                .filter(|a| !a.yanked.yanked)
                .collect::<Vec<_>>();

            if non_yanked_artifacts.is_empty() {
                tracing::info!("{package_name} {version} was yanked (skipping)");
                continue;
            }
            let solvable_id = self
                .pool
                .intern_solvable(name, PypiVersion(version.clone()));
            candidates.candidates.push(solvable_id);
        }
        Some(candidates)
    }

    fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
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

        let solvable = self.pool.resolve_solvable(solvable);
        let package_name = self.pool.resolve_package_name(solvable.name_id());

        let mut dependencies = Dependencies::default();
        let result = task::block_in_place(move || {
            Handle::current().block_on(
                self.package_db
                    .available_artifacts(&package_name.clone().into()),
            )
        });

        let artifacts_per_version = match result {
            Ok(artifacts) => artifacts,
            Err(e) => {
                tracing::error!("failed to fetch artifacts of '{package_name}': {e:?}, skipping..");
                return dependencies;
            }
        };

        let artifacts = artifacts_per_version
            .get(&solvable.inner().0.clone())
            .expect("strange, no artificats are available");

        // Filter yanked artifacts
        let non_yanked_artifacts = artifacts
            .iter()
            .filter(|a| !a.yanked.yanked)
            .collect::<Vec<_>>();

        if non_yanked_artifacts.is_empty() {
            panic!("no artifacts are available after removing yanked artifacts");
        }

        let (_, metadata) = task::block_in_place(|| {
            Handle::current()
                .block_on(
                    self.package_db
                        .get_metadata::<Wheel, _>(&non_yanked_artifacts),
                )
                .unwrap()
        });

        for requirement in metadata.requires_dist {
            // Evaluate environment markers
            if let Some(env_marker) = &requirement.env_marker_expr {
                if !env_marker.eval(&env).unwrap() {
                    // tracing::info!("skipping dependency {requirement}");
                    continue;
                }
            }

            // Add the dependency to the pool
            let Requirement {
                name, specifiers, ..
            } = requirement.into_inner();

            let dependency_name_id = self.pool.intern_package_name(name);
            let version_set_id = self
                .pool
                .intern_version_set(dependency_name_id, specifiers.into());
            dependencies.requirements.push(version_set_id)
        }
        dependencies
    }
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
        &[normalize_index_url(args.index_url)],
        cache_dir.clone(),
    )
    .into_diagnostic()?;

    let provider = PypiDependencyProvider {
        pool: Pool::new(),
        package_db,
    };

    // Create a task to solve the specs passed on the command line.
    let mut root_requirements = Vec::with_capacity(args.specs.len());
    for Requirement {
        name, specifiers, ..
    } in args.specs.iter().map(PackageRequirement::as_inner)
    {
        let dependency_package_name = provider.pool().intern_package_name(name.clone());
        let version_set_id = provider
            .pool()
            .intern_version_set(dependency_package_name, specifiers.clone().into());
        root_requirements.push(version_set_id);
    }

    // Solve the jobs
    let mut solver = Solver::new(provider);
    let result = solver.solve(root_requirements);
    let artifacts = match result {
        Err(e) => {
            eprintln!(
                "Could not solve:\n{}",
                e.display_user_friendly(&solver, &DefaultSolvableDisplay)
            );
            return Ok(());
        }
        Ok(transaction) => transaction
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

#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{e:?}");
    }
}

fn normalize_index_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    url
}

#[cfg(test)]
mod test {
    use rattler_installs_packages::Version;

    #[test]
    fn valid_version() {
        assert!(Version::parse("2011k").is_some());
    }
}
