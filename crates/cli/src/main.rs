use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cs2mod", version, about = "CS2 grenade lineup calculator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract a CS2 map into a collision mesh and cache it.
    Extract,
    /// Print information about a cached extraction.
    Info,
    /// Export a cached collision mesh to an OBJ file.
    ExportObj,
    /// Simulate a single grenade throw.
    #[command(alias = "simulate")]
    Throw,
    /// Simulate a smoke grenade and its resulting volume.
    Smoke,
    /// Check sightline occlusion between two points.
    Sightline,
    /// Find candidate stand spots for a lineup.
    Standspots,
    /// Solve for a lineup that hits a target.
    Solve,
    /// Calibrate throw constants from measured throws.
    Calibrate,
    /// Replay a recorded throw corpus offline.
    Replay,
    /// Run the HTTP API and web viewer server.
    Serve,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Extract => anyhow::bail!("extract is not implemented yet (planned for stage 2)"),
        Command::Info => anyhow::bail!("info is not implemented yet (planned for stage 2)"),
        Command::ExportObj => {
            anyhow::bail!("export-obj is not implemented yet (planned for stage 2)")
        }
        Command::Throw => anyhow::bail!("throw is not implemented yet (planned for stage 4)"),
        Command::Smoke => anyhow::bail!("smoke is not implemented yet (planned for stage 4)"),
        Command::Sightline => {
            anyhow::bail!("sightline is not implemented yet (planned for stage 4)")
        }
        Command::Standspots => {
            anyhow::bail!("standspots is not implemented yet (planned for stage 5)")
        }
        Command::Solve => anyhow::bail!("solve is not implemented yet (planned for stage 5)"),
        Command::Calibrate => {
            anyhow::bail!("calibrate is not implemented yet (planned for stage 4)")
        }
        Command::Replay => anyhow::bail!("replay is not implemented yet (planned for stage 4)"),
        Command::Serve => anyhow::bail!("serve is not implemented yet (planned for stage 6)"),
    }
}
