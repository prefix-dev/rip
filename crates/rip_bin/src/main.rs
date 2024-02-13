use rip_bin::{cli, global_multi_progress, IndicatifWriter};

use std::str::FromStr;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use miette::Context;
use tracing_subscriber::filter::Directive;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use rattler_installs_packages::index::PackageSourcesBuilder;

use rattler_installs_packages::normalize_index_url;
use reqwest::Client;
use reqwest_middleware::ClientWithMiddleware;
use rip_bin::cli::wheels::wheels;
use tracing::metadata::LevelFilter;
use url::Url;

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    /// Subcommand to run
    #[command(subcommand)]
    command: Commands,

    /// Sets the logging level
    #[command(flatten)]
    verbose: clap_verbosity_flag::Verbosity,

    /// Base URL of the Python Package Index (default <https://pypi.org/simple>). This should point
    /// to a repository compliant with PEP 503 (the simple repository API).
    #[clap(default_value = "https://pypi.org/simple/", long, global = true)]
    index_url: Url,
}

#[derive(Subcommand)]
enum Commands {
    /// Options w.r.t locally built wheels
    Wheels(cli::wheels::Args),

    #[command(flatten)]
    InstallOrResolve(cli::resolve::Commands),
}

async fn actual_main() -> miette::Result<()> {
    let args = Cli::parse();

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
    let index_url = normalize_index_url(args.index_url.clone());
    let sources = PackageSourcesBuilder::new(index_url).build()?;

    let client = ClientWithMiddleware::from(Client::new());
    let package_db = Arc::new(
        rattler_installs_packages::index::PackageDb::new(sources, client, &cache_dir)
            .wrap_err_with(|| {
                format!(
                    "failed to construct package database for index {}",
                    args.index_url
                )
            })?,
    );

    match args.command {
        Commands::InstallOrResolve(cmds) => cli::resolve::execute(package_db.clone(), cmds).await,
        Commands::Wheels(args) => wheels(package_db.clone(), args),
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{e:?}");
    }
}

/// Constructs a default [`EnvFilter`] that is used when the user did not specify a custom RUST_LOG.
pub fn get_default_env_filter(verbose: clap_verbosity_flag::Verbosity) -> EnvFilter {
    // Always log info for rattler_installs_packages
    let (rip, rest) = match verbose.log_level_filter() {
        clap_verbosity_flag::LevelFilter::Off => (LevelFilter::OFF, LevelFilter::OFF),
        clap_verbosity_flag::LevelFilter::Error => (LevelFilter::INFO, LevelFilter::ERROR),
        clap_verbosity_flag::LevelFilter::Warn => (LevelFilter::INFO, LevelFilter::WARN),
        clap_verbosity_flag::LevelFilter::Info => (LevelFilter::INFO, LevelFilter::INFO),
        clap_verbosity_flag::LevelFilter::Debug => (LevelFilter::DEBUG, LevelFilter::DEBUG),
        clap_verbosity_flag::LevelFilter::Trace => (LevelFilter::TRACE, LevelFilter::TRACE),
    };

    EnvFilter::builder()
        .with_default_directive(rest.into())
        .from_env()
        .expect("failed to get env filter")
        .add_directive(
            Directive::from_str(&format!("rattler_installs_packages={}", rip))
                .expect("cannot parse directive"),
        )
}
