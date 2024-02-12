use clap::{Parser, Subcommand};
use miette::IntoDiagnostic;
use rattler_installs_packages::index::PackageDb;
use std::sync::Arc;

#[derive(Parser)]
#[command(version, about, long_about = None)]
#[command(propagate_version = true)]
pub struct Args {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// List locally built wheels
    List,
}

pub fn wheels(package_db: Arc<PackageDb>, args: Args) -> miette::Result<()> {
    match args.command {
        Commands::List => list_wheels(package_db),
    }
}

fn list_wheels(package_db: Arc<PackageDb>) -> miette::Result<()> {
    let wheels = package_db.local_wheel_cache().wheels();
    for wheel in wheels {
        println!("{}", wheel.into_diagnostic()?);
    }

    Ok(())
}
