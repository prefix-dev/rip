use clap::{Parser, Subcommand};
use fs_err as fs;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic};
use rattler_installs_packages::index::PackageDb;
use rattler_installs_packages::install::InstallWheelOptions;
use rattler_installs_packages::python_env::{Pep508EnvMakers, PythonLocation, WheelTags};
use rattler_installs_packages::resolve::solve_options::{
    OnWheelBuildFailure, PreReleaseResolution, ResolveOptions, SDistResolution,
};
use rattler_installs_packages::resolve::PinnedPackage;
use rattler_installs_packages::types::Requirement;
use rattler_installs_packages::wheel_builder::WheelBuilder;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Serialize, Debug)]
struct Solution {
    resolved: bool,
    packages: HashMap<String, String>,
    error: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Resolve a set of requirements and output the resolved versions
    #[clap(alias = "r")]
    Resolve(ResolveArgs),

    /// Resolve and install a set of requirements
    #[clap(alias = "i")]
    Install(InstallArgs),
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct ResolveArgs {
    #[clap(num_args = 1.., required = true)]
    /// The specs to resolve
    specs: Vec<Requirement>,

    /// How to handle SDists
    #[clap(flatten)]
    sdist_resolution: SDistResolutionArgs,

    /// Path to the python interpreter to use for resolving environment markers and creating venvs
    #[clap(long, short)]
    python_interpreter: Option<PathBuf>,

    /// Disable inheritance of env variables.
    #[arg(short = 'c', long)]
    clean_env: bool,

    /// Save failed wheel build environments
    #[arg(long)]
    save_on_failure: bool,

    /// Prefer pre-releases to normal releases
    #[clap(long)]
    pre: bool,

    /// Output the result as json
    #[clap(long)]
    json: bool,
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct InstallArgs {
    #[clap(flatten)]
    resolve_args: ResolveArgs,

    /// The target directory to install into
    target: PathBuf,
}

#[derive(Parser)]
#[group(multiple = false)]
pub struct SDistResolutionArgs {
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

impl From<SDistResolutionArgs> for SDistResolution {
    fn from(value: SDistResolutionArgs) -> Self {
        if value.only_sdists {
            SDistResolution::OnlySDists
        } else if value.only_wheels {
            SDistResolution::OnlyWheels
        } else if value.prefer_sdists {
            SDistResolution::PreferSDists
        } else if value.prefer_wheels {
            SDistResolution::PreferWheels
        } else {
            SDistResolution::Normal
        }
    }
}

pub async fn execute(package_db: Arc<PackageDb>, commands: Commands) -> miette::Result<()> {
    let (args, target) = match commands {
        Commands::Resolve(args) => (args, None),
        Commands::Install(args) => (args.resolve_args, Some(args.target)),
    };

    // Determine the environment markers for the current machine
    let env_markers = Arc::new(match args.python_interpreter {
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
    }.0);
    tracing::debug!(
        "extracted the following environment markers from the system python interpreter:\n{:#?}",
        env_markers
    );

    let python_location = match args.python_interpreter {
        Some(python_interpreter) => PythonLocation::Custom(python_interpreter),
        None => PythonLocation::System,
    };

    let compatible_tags =
        WheelTags::from_python(python_location.executable().into_diagnostic()?.as_path())
            .await
            .into_diagnostic()
            .map(Arc::new)?;
    tracing::debug!(
        "extracted the following compatible wheel tags from the system python interpreter: {}",
        compatible_tags.tags().format(", ")
    );

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
        ..Default::default()
    };

    let wheel_builder = WheelBuilder::new(
        package_db.clone(),
        env_markers.clone(),
        Some(compatible_tags.clone()),
        resolve_opts.clone(),
    )
    .into_diagnostic()?;

    // Solve the environment
    let blueprint = match rattler_installs_packages::resolve::resolve(
        package_db.clone(),
        &args.specs,
        env_markers.clone(),
        Some(compatible_tags.clone()),
        wheel_builder.clone(),
        resolve_opts.clone(),
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
                return Ok(());
            } else {
                Err(err.wrap_err("Could not solve for requested requirements"))
            }
        }
    };

    // Output the selected versions
    println!(
        "{}:",
        console::style("Successfully resolved environment").bold()
    );
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

    if args.json {
        let solution = Solution {
            resolved: true,
            packages: blueprint
                .iter()
                .map(|p| (p.name.to_string(), p.version.to_string()))
                .collect(),
            error: None,
        };
        println!("{}", serde_json::to_string_pretty(&solution).unwrap());
    }

    // Install if requested
    if let Some(target) = target {
        install_packages(
            package_db,
            wheel_builder,
            blueprint,
            python_location,
            target,
        )
        .await?
    }

    Ok(())
}

/// Install resolved packages into a virtual environment
pub async fn install_packages(
    package_db: Arc<PackageDb>,
    wheel_builder: Arc<WheelBuilder>,
    pinned_packages: Vec<PinnedPackage>,
    python_location: PythonLocation,
    target: PathBuf,
) -> miette::Result<()> {
    println!(
        "\n\nInstalling into: {}",
        console::style(target.display()).bold()
    );
    if !target.exists() {
        std::fs::create_dir_all(&target).into_diagnostic()?;
    }

    let venv = rattler_installs_packages::python_env::VEnv::create(&target, python_location)
        .into_diagnostic()?;

    let longest = pinned_packages
        .iter()
        .map(|p| p.name.as_str().len())
        .max()
        .unwrap_or_default();
    let mut tabbed_stdout = tabwriter::TabWriter::new(std::io::stdout()).minwidth(longest);

    for pinned_package in pinned_packages
        .clone()
        .into_iter()
        .sorted_by(|a, b| a.name.cmp(&b.name))
    {
        writeln!(
            tabbed_stdout,
            "{name}\t{version}",
            name = console::style(pinned_package.name).bold().green(),
            version = console::style(pinned_package.version).italic()
        )
        .into_diagnostic()?;
        tabbed_stdout.flush().into_diagnostic()?;
        // println!(
        //     "\ninstalling: {} - {}",
        //     console::style(pinned_package.name).bold().green(),
        //     console::style(pinned_package.version).italic()
        // );
        let artifact_info = pinned_package.artifacts.first().unwrap();
        let (artifact, direct_url_json) = package_db
            .get_wheel(artifact_info, Some(wheel_builder.clone()))
            .await?;
        venv.install_wheel(
            &artifact,
            &InstallWheelOptions {
                direct_url_json,
                ..Default::default()
            },
        )
        .into_diagnostic()?;
    }

    println!(
        "\n{}",
        console::style("Successfully installed environment!").bold()
    );

    Ok(())
}
