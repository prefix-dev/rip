use rip_bin::{global_multi_progress, IndicatifWriter};
use std::io::Write;
use std::str::FromStr;

use clap::Parser;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use tracing_subscriber::filter::Directive;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use url::Url;

use rattler_installs_packages::tags::WheelTags;
use rattler_installs_packages::{normalize_index_url, resolve, Pep508EnvMakers, UserRequirement};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(num_args=1.., required=true)]
    specs: Vec<UserRequirement>,

    /// Base URL of the Python Package Index (default <https://pypi.org/simple>). This should point
    /// to a repository compliant with PEP 503 (the simple repository API).
    #[clap(default_value = "https://pypi.org/simple/", long)]
    index_url: Url,

    #[clap(short)]
    verbose: bool,
}

async fn actual_main() -> miette::Result<()> {
    let args = Args::parse();

    // Setup tracing subscriber
    tracing_subscriber::registry()
        .with(fmt::layer().with_writer(IndicatifWriter::new(global_multi_progress())))
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| get_default_env_filter(args.verbose)),
        )
        .init();

    // Determine cache directory
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| miette::miette!("failed to determine cache directory"))?
        .join("rattler/pypi");
    tracing::info!("cache directory: {}", cache_dir.display());

    // Construct a package database
    let package_db = rattler_installs_packages::PackageDb::new(
        Default::default(),
        &[normalize_index_url(args.index_url.clone())],
        cache_dir,
    )
    .into_diagnostic()
    .wrap_err_with(|| {
        format!(
            "failed to construct package database for index {}",
            args.index_url
        )
    })?;

    // Determine the environment markers for the current machine
    let env_markers = Pep508EnvMakers::from_env()
        .await
        .into_diagnostic()
        .wrap_err_with(|| {
            "failed to determine environment markers for the current machine (could not run Python)"
        })?;
    tracing::info!(
        "extracted the following environment markers from the system python interpreter:\n{:#?}",
        env_markers
    );

    let compatible_tags = WheelTags::from_env().await.into_diagnostic()?;
    tracing::info!(
        "extracted the following compatible wheel tags from the system python interpreter: {}",
        compatible_tags.tags().format(", ")
    );

    // Solve the environment
    let blueprint = match resolve(
        &package_db,
        &args.specs,
        &env_markers,
        Some(&compatible_tags),
    )
    .await
    {
        Ok(blueprint) => blueprint,
        Err(err) => miette::bail!("Could not solve for the requested requirements:\n{err}"),
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
    for (name, (version, extras)) in blueprint.into_iter().sorted_by(|(a, _), (b, _)| a.cmp(b)) {
        write!(tabbed_stdout, "{name}", name = name.as_str()).into_diagnostic()?;
        if !extras.is_empty() {
            write!(
                tabbed_stdout,
                "[{}]",
                extras.iter().map(|e| e.as_str()).join(",")
            )
            .into_diagnostic()?;
        }
        writeln!(tabbed_stdout, "\t{version}").into_diagnostic()?;
    }
    tabbed_stdout.flush().into_diagnostic()?;

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{e:?}");
    }
}

/// Constructs a default [`EnvFilter`] that is used when the user did not specify a custom RUST_LOG.
pub fn get_default_env_filter(verbose: bool) -> EnvFilter {
    let mut result = EnvFilter::new("rip=info")
        .add_directive(Directive::from_str("rattler_installs_packages=info").unwrap());

    if verbose {
        result = result.add_directive(Directive::from_str("resolvo=info").unwrap());
    }

    result
}
