use std::io::Write;

use clap::Parser;
use miette::IntoDiagnostic;
use resolvo::{DefaultSolvableDisplay, DependencyProvider, Solver};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use url::Url;

use rattler_installs_packages::{requirement::Requirement, PackageRequirement};
use rip::{
    pypi_provider::{PypiDependencyProvider, PypiPackageName},
    writer::{global_multi_progress, IndicatifWriter},
};

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

async fn actual_main() -> miette::Result<()> {
    let args = Args::parse();

    // Setup tracing subscriber
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(IndicatifWriter::new(global_multi_progress())))
        .with(EnvFilter::from_default_env())
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
        cache_dir,
    )
    .into_diagnostic()?;

    let provider = PypiDependencyProvider::new(package_db);

    // Create a task to solve the specs passed on the command line.
    let mut root_requirements = Vec::with_capacity(args.specs.len());
    for Requirement {
        name,
        specifiers,
        extras,
        ..
    } in args.specs.iter().map(PackageRequirement::as_inner)
    {
        let dependency_package_name = provider
            .pool()
            .intern_package_name(PypiPackageName::Base(name.clone().into()));
        let version_set_id = provider
            .pool()
            .intern_version_set(dependency_package_name, specifiers.clone().into());
        root_requirements.push(version_set_id);

        for extra in extras {
            let dependency_package_name = provider
                .pool()
                .intern_package_name(PypiPackageName::Extra(name.clone().into(), extra.clone()));
            let version_set_id = provider
                .pool()
                .intern_version_set(dependency_package_name, specifiers.clone().into());
            root_requirements.push(version_set_id);
        }
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
        assert!(Version::parse("1.2.1").is_some());
    }
}
