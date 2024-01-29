use fs_err as fs;
use rattler_installs_packages::resolve::PreReleaseResolution;
use rip_bin::{global_multi_progress, IndicatifWriter};
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use tracing_subscriber::filter::Directive;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use url::Url;

use rattler_installs_packages::artifacts::wheel::UnpackWheelOptions;
use rattler_installs_packages::python_env::{PythonLocation, WheelTags};
use rattler_installs_packages::resolve::OnWheelBuildFailure;
use rattler_installs_packages::wheel_builder::WheelBuilder;
use rattler_installs_packages::{
    normalize_index_url, python_env::Pep508EnvMakers, resolve, resolve::resolve,
    resolve::ResolveOptions, types::Requirement,
};

#[derive(Serialize, Debug)]
struct Solution {
    resolved: bool,
    packages: HashMap<String, String>,
    error: Option<String>,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[clap(num_args = 1.., required = true)]
    specs: Vec<Requirement>,

    /// Create a venv and install into this environment
    /// Does not check for any installed packages for now
    #[clap(long)]
    install_into: Option<PathBuf>,

    /// Base URL of the Python Package Index (default <https://pypi.org/simple>). This should point
    /// to a repository compliant with PEP 503 (the simple repository API).
    #[clap(default_value = "https://pypi.org/simple/", long)]
    index_url: Url,

    /// Verbose logging from resolvo
    #[clap(short)]
    verbose: bool,

    /// How to handle sidsts
    #[clap(flatten)]
    sdist_resolution: SDistResolution,

    /// Path to the python interpreter to use for resolving environment markers and creating venvs
    #[clap(long, short)]
    python_interpreter: Option<PathBuf>,

    #[arg(short = 'c', long)]
    /// Disable inheritance of env variables.
    clean_env: bool,

    #[arg(long)]
    /// Save failed wheel build environments
    save_on_failure: bool,

    /// Prefer pre-releases over normal releases
    #[clap(long)]
    pre: bool,

    #[clap(long)]
    json: bool,
}

#[derive(Parser)]
#[group(multiple = false)]
struct SDistResolution {
    /// Prefer any version with wheels over any version with sdists
    #[clap(long)]
    prefer_wheels: bool,

    /// Prefer any version with sdists over any version with wheels
    #[clap(long)]
    prefer_sdists: bool,

    /// Only select versions with wheels, ignore versions with sdists
    #[clap(long)]
    only_wheels: bool,

    /// Only select versions with sdists, ignore versions with wheels
    #[clap(long)]
    only_sdists: bool,
}

impl From<SDistResolution> for resolve::SDistResolution {
    fn from(value: SDistResolution) -> Self {
        if value.only_sdists {
            resolve::SDistResolution::OnlySDists
        } else if value.only_wheels {
            resolve::SDistResolution::OnlyWheels
        } else if value.prefer_sdists {
            resolve::SDistResolution::PreferSDists
        } else if value.prefer_wheels {
            resolve::SDistResolution::PreferWheels
        } else {
            resolve::SDistResolution::Normal
        }
    }
}

async fn actual_main() -> miette::Result<()> {
    use reqwest::Client;
    use reqwest_middleware::ClientWithMiddleware;

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
    let client = ClientWithMiddleware::from(Client::new());
    let package_db = rattler_installs_packages::index::PackageDb::new(
        client,
        &[normalize_index_url(args.index_url.clone())],
        &cache_dir,
    )
    .into_diagnostic()
    .wrap_err_with(|| {
        format!(
            "failed to construct package database for index {}",
            args.index_url
        )
    })?;

    // Determine the environment markers for the current machine
    let env_markers = match args.python_interpreter {
        Some(ref python) => {
            let python = fs::canonicalize(python).into_diagnostic()?;
            Pep508EnvMakers::from_python(&python).await.into_diagnostic()
                .wrap_err_with(|| {
                    format!(
                        "failed to determine environment markers for the current machine (could not run Python in path: {:?})"
                        , python
                    )
                })?
        }
        None => Pep508EnvMakers::from_env().await.into_diagnostic()
            .wrap_err_with(|| {
                "failed to determine environment markers for the current machine (could not run Python)"
            })?,
    };
    tracing::debug!(
        "extracted the following environment markers from the system python interpreter:\n{:#?}",
        env_markers
    );

    let compatible_tags = WheelTags::from_env().await.into_diagnostic()?;
    tracing::debug!(
        "extracted the following compatible wheel tags from the system python interpreter: {}",
        compatible_tags.tags().format(", ")
    );

    let python_location = match args.python_interpreter {
        Some(python_interpreter) => PythonLocation::Custom(python_interpreter),
        None => PythonLocation::System,
    };

    let on_wheel_build_failure = if args.save_on_failure {
        OnWheelBuildFailure::SaveBuildEnv
    } else {
        OnWheelBuildFailure::DeleteBuildEnv
    };

    let pre_release_resolution = if args.pre {
        PreReleaseResolution::Allow
    } else {
        PreReleaseResolution::from_specs(&args.specs)
    };

    let resolve_opts = ResolveOptions {
        sdist_resolution: args.sdist_resolution.into(),
        python_location: python_location.clone(),
        clean_env: args.clean_env,
        on_wheel_build_failure,
        pre_release_resolution,
    };

    // Solve the environment
    let blueprint = match resolve(
        &package_db,
        &args.specs,
        &env_markers,
        Some(&compatible_tags),
        HashMap::default(),
        HashMap::default(),
        &resolve_opts,
        HashMap::default(),
    )
    .await
    {
        Ok(blueprint) => blueprint,
        Err(err) => {
            return if args.json {
                let solution = Solution {
                    resolved: false,
                    packages: HashMap::default(),
                    error: Some(format!("{}", err)),
                };
                println!("{}", serde_json::to_string_pretty(&solution).unwrap());
                Ok(())
            } else {
                Err(err.wrap_err("Could not solve for requested requirements"))
            }
        }
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
    for pinned_package in blueprint.iter().sorted_by(|a, b| a.name.cmp(&b.name)) {
        write!(tabbed_stdout, "{name}", name = pinned_package.name.as_str()).into_diagnostic()?;
        if !pinned_package.extras.is_empty() {
            write!(
                tabbed_stdout,
                "[{}]",
                pinned_package.extras.iter().map(|e| e.as_str()).join(",")
            )
            .into_diagnostic()?;
        }
        writeln!(
            tabbed_stdout,
            "\t{version}",
            version = pinned_package.version
        )
        .into_diagnostic()?;
    }
    tabbed_stdout.flush().into_diagnostic()?;

    // Try to install into this environment
    if let Some(install) = args.install_into {
        println!(
            "\n\nInstalling into: {}",
            console::style(install.display()).bold()
        );
        if !install.exists() {
            std::fs::create_dir_all(&install).into_diagnostic()?;
        }

        let venv = rattler_installs_packages::python_env::VEnv::create(&install, python_location)
            .into_diagnostic()?;
        let wheel_builder = WheelBuilder::new(
            &package_db,
            &env_markers,
            Some(&compatible_tags),
            &resolve_opts,
            Default::default(),
        )
        .into_diagnostic()?;

        for pinned_package in blueprint
            .clone()
            .into_iter()
            .sorted_by(|a, b| a.name.cmp(&b.name))
        {
            println!(
                "\ninstalling: {} - {}",
                console::style(pinned_package.name).bold().green(),
                console::style(pinned_package.version).italic()
            );
            let artifact_info = pinned_package.artifacts.first().unwrap();
            let artifact = package_db
                .get_wheel(artifact_info, Some(&wheel_builder))
                .await
                .expect("could not get artifact");
            venv.install_wheel(&artifact, &UnpackWheelOptions::default())
                .into_diagnostic()?;
        }
    }

    println!(
        "\n{}",
        console::style("Successfully installed environment!").bold()
    );

    if args.json {
        let solution = Solution {
            resolved: true,
            packages: blueprint
                .into_iter()
                .map(|p| (p.name.to_string(), p.version.to_string()))
                .collect(),
            error: None,
        };
        println!("{}", serde_json::to_string_pretty(&solution).unwrap());
    }

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
