// Purpose: to check out what the extras field contains in pypi

use clap::Parser;
use miette::IntoDiagnostic;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {

    /// Base URL of the Python Package Index (default https://pypi.org/simple). This should point
    /// to a repository compliant with PEP 503 (the simple repository API).
    #[clap(default_value = "https://pypi.org/simple/", long)]
    index_url: Url,
}


fn normalize_index_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    url
}
pub async fn actual_main() -> Result<(), miette::Error> {
    let args = Args::parse();


    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .finish()
        .init();

    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| miette::miette!("failed to determine cache directory"))?
        .join("rattler/pypi");

    let package_db = rattler_installs_packages::PackageDb::new(
        Default::default(),
        &[normalize_index_url(args.index_url)],
        cache_dir.clone(),
    )
        .into_diagnostic()?;
    let names = package_db.get_package_names().await?;

    // TODO: get extra's

    Ok(())
}


#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }
}
