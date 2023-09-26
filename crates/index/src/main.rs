// Simple program to index and query pypi metadata

use std::collections::HashSet;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use indexmap::IndexSet;
use indicatif::ProgressBar;
use miette::IntoDiagnostic;
use rand::seq::SliceRandom;
use rusqlite::Connection;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::util::SubscriberInitExt;
use url::Url;

use rattler_installs_packages::{Extra, PackageName, PackageRequirement, Wheel};
use rip::writer::{global_multi_progress, IndicatifWriter};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Base URL of the Python Package Index (default https://pypi.org/simple). This should point
    /// to a repository compliant with PEP 503 (the simple repository API).
    #[clap(default_value = "https://pypi.org/simple/", long)]
    index_url: Url,

    #[clap(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Index,
    ListExtras,
}
fn normalize_index_url(mut url: Url) -> Url {
    let path = url.path();
    if !path.ends_with('/') {
        url.set_path(&format!("{path}/"));
    }
    url
}

pub async fn index(index_url: Url) -> Result<(), miette::Error> {
    let cache_dir = dirs::cache_dir()
        .ok_or_else(|| miette::miette!("failed to determine cache directory"))?
        .join("rattler/pypi");

    let package_db = rattler_installs_packages::PackageDb::new(
        Default::default(),
        &[normalize_index_url(index_url)],
        cache_dir.clone(),
    )
        .into_diagnostic()?;

    let mut names = package_db.get_package_names().await?;
    names.shuffle(&mut rand::thread_rng());

    let bar = global_multi_progress().add(ProgressBar::new(names.len() as u64));

    let conn = Connection::open("index.sqlite3").into_diagnostic()?;
    // let conn = Connection::open_in_memory().into_diagnostic()?;
    conn.execute("CREATE TABLE IF NOT EXISTS metadata (id INTEGER PRIMARY KEY, name TEXT, version TEXT, requires_dist TEXT, requires_python TEXT, extras TEXT)", ()).into_diagnostic()?;
    conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_name_version ON `metadata` (`name`, `version`)",
        (),
    )
        .into_diagnostic()?;

    let mut insert_stmt = conn.prepare("INSERT INTO metadata (name, version, requires_dist, requires_python, extras) VALUES (?, ?, ?, ?, ?)").into_diagnostic()?;
    let mut request = conn
        .prepare("SELECT * FROM metadata WHERE name = ? AND version = ?")
        .into_diagnostic()?;
    for n in names {
        bar.inc(1);
        let package_name = PackageName::from_str(&n)?;
        let mut artifacts_per_version = package_db
            .available_artifacts(package_name.clone())
            .await?
            .clone();
        artifacts_per_version.sort_keys();

        let (chosen_version, available_artifacts) =
            if let Some(available_artifacts) = artifacts_per_version.get_index(0) {
                available_artifacts
            } else {
                continue;
            };

        let mut rows = request
            .query([package_name.as_str(), chosen_version.to_string().as_str()])
            .into_diagnostic()?;
        if rows.next().into_diagnostic()?.is_some() {
            // Skip if we have it in the database
            continue;
        }
        let available_artifacts = available_artifacts
            .iter()
            // We are only interested in wheels
            .filter(|a| a.is::<Wheel>())
            // TODO: How to filter prereleases correctly?
            .filter(|a| a.filename.version().pre.is_none() && a.filename.version().dev.is_none())
            // Non-yanked only
            .filter(|a| !a.yanked.yanked)
            .collect::<Vec<_>>();

        if available_artifacts.is_empty() {
            continue;
        }

        let metadata = package_db
            .get_metadata::<Wheel, _>(&available_artifacts)
            .await
            .ok();

        // Continue if there was an error in downloading and skip for now :)
        let metadata = if let Some(metadata) = metadata {
            metadata.1
        } else {
            continue;
        };

        insert_stmt
            .insert([
                package_name.as_str(),
                chosen_version.to_string().as_str(),
                serde_json::to_string(&metadata.requires_dist)
                    .into_diagnostic()?
                    .as_str(),
                serde_json::to_string(&metadata.requires_python)
                    .into_diagnostic()?
                    .as_str(),
                serde_json::to_string(&metadata.extras)
                    .into_diagnostic()?
                    .as_str(),
            ])
            .into_diagnostic()?;
    }

    Ok(())
}

pub fn query_extras() -> Result<(), miette::Error> {
    // Just query extras for now
    let conn = Connection::open("index.sqlite3").into_diagnostic()?;
    let mut select_stmt = conn
        .prepare("SELECT extras FROM metadata")
        .into_diagnostic()?;
    let iter = select_stmt
        .query_map([], |row| {
            let extras: String = row.get(0)?;
            Ok(extras)
        })
        .into_diagnostic()?;

    println!("EXTRAS EXPORTED:");
    let mut exported = IndexSet::new();
    for extras in iter {
        let extras =
            serde_json::from_str::<HashSet<Extra>>(&extras.into_diagnostic()?).into_diagnostic()?;
        exported.extend(extras.into_iter());
    }
    exported.sort();
    exported.iter().for_each(|e| println!("{}", e.as_str()));

    println!("EXTRAS IN SPECS:");
    let mut select_stmt = conn
        .prepare("SELECT requires_dist FROM metadata")
        .into_diagnostic()?;

    let mut count = 0usize;
    let mut total = 0usize;
    let iter = select_stmt
        .query_map([], |row| {
            let extras: String = row.get(0)?;
            Ok(extras)
        })
        .into_diagnostic()?;
    for requirement in iter {
        let requires_dist = serde_json::from_str::<Vec<PackageRequirement>>(
            requirement.into_diagnostic()?.as_str(),
        )
            .into_diagnostic()?;
        total += requires_dist.len();
        for req in requires_dist {
            if !req.extras.is_empty() {
                println!(
                    "{}: {}",
                    req.name.as_str(),
                    req.extras
                        .iter()
                        .map(|e| e.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                count += 1;
            }
        }
    }
    println!("extras found in {count}/{total}");

    Ok(())
}

pub async fn actual_main() -> Result<(), miette::Error> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(IndicatifWriter::new(global_multi_progress()))
        .finish()
        .init();

    match args.command {
        Command::Index => index(args.index_url).await?,
        Command::ListExtras => query_extras()?,
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("{:?}", e);
        std::process::exit(1);
    }
}
