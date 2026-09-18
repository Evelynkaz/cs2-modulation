mod cmd_extract;
mod cmd_res;
mod cmd_vpk;
mod game_path;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "cs2mod", version, about = "CS2 grenade lineup calculator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Extract one or more CS2 maps into a collision mesh and cache them.
    Extract {
        /// Map name(s) (e.g. `de_mirage`).
        #[arg(required = true)]
        maps: Vec<String>,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; defaults to `<repo-or-cwd>/cache`.
        #[arg(long)]
        cache: Option<PathBuf>,
        /// Re-extract even if a cache entry for this build already exists.
        #[arg(long)]
        force: bool,
    },
    /// Print information about a cached extraction.
    Info {
        /// Map name (e.g. `de_mirage`).
        map: String,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; defaults to `<repo-or-cwd>/cache`.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Export a cached collision mesh to an OBJ file, auto-extracting first if there is no cache.
    ExportObj {
        /// Map name (e.g. `de_mirage`).
        map: String,
        /// Output `.obj` path; a `.mtl` is written alongside it.
        #[arg(long)]
        out: PathBuf,
        /// `grenade`, `player`, `all`, or `attrs:Name1,Name2`.
        #[arg(long, default_value = "all")]
        filter: String,
        /// `minx,miny,minz,maxx,maxy,maxz`.
        #[arg(long)]
        region: Option<String>,
        /// Write vertices as Source's native (x,y,z) instead of swapping to Y-up.
        #[arg(long)]
        no_y_up: bool,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; defaults to `<repo-or-cwd>/cache`.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
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
    /// Inspect and extract VPK archives.
    Vpk {
        #[command(subcommand)]
        command: VpkCommand,
    },
    /// Dump a Source 2 resource container (`*_c`).
    Res(ResCliArgs),
}

#[derive(Subcommand)]
enum VpkCommand {
    /// List entry paths, one per line.
    Ls {
        /// A VPK path, a bare map name (e.g. `de_mirage`), or `pak01`.
        vpk: String,
        /// Only entries whose path contains this substring.
        #[arg(long)]
        filter: Option<String>,
        /// Only entries with this extension (without the leading dot).
        #[arg(long)]
        ext: Option<String>,
        /// Also print size, archive index and CRC.
        #[arg(long)]
        long: bool,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
    },
    /// Extract one entry to a file.
    Cat {
        /// A VPK path, a bare map name (e.g. `de_mirage`), or `pak01`.
        vpk: String,
        /// The entry's path inside the VPK.
        path: String,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        game: Option<PathBuf>,
    },
    /// CRC-check every entry.
    Verify {
        /// A VPK path, a bare map name (e.g. `de_mirage`), or `pak01`.
        vpk: String,
        #[arg(long)]
        game: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct ResCliArgs {
    /// A path on disk, or an entry path inside `--vpk`.
    file: String,
    /// A VPK path, a bare map name (e.g. `de_mirage`), or `pak01`.
    #[arg(long)]
    vpk: Option<String>,
    /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
    #[arg(long)]
    game: Option<PathBuf>,
    /// Restrict output to one block, by its 4CC (e.g. `DATA`).
    #[arg(long)]
    block: Option<String>,
    /// Print KV3 block(s) as text.
    #[arg(long)]
    kv3: bool,
    /// Don't truncate large blobs in KV3 text output.
    #[arg(long)]
    full_blobs: bool,
}

fn main() -> std::process::ExitCode {
    // KV3 parsing recurses per nesting level; run on a thread with a stack large enough for
    // the deepest documents we accept (see `RECURSION_LIMIT` in `s2fmt::kv3::binary`).
    let handle = std::thread::Builder::new()
        .name("cs2mod-main".into())
        .stack_size(16 << 20)
        .spawn(run)
        .expect("spawn main thread");
    match handle
        .join()
        .unwrap_or_else(|p| std::panic::resume_unwind(p))
    {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) if is_broken_pipe(&e) => {
            // The reader end of a pipe (e.g. `| head`) closed early; that's
            // not a real failure, just quiet success.
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Error: {e:?}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// True if any error in the chain is an `io::Error` with
/// `ErrorKind::BrokenPipe` (from a `writeln!` to a closed stdout pipe).
fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Extract {
            maps,
            game,
            cache,
            force,
        } => cmd_extract::extract(&maps, game.as_deref(), cache.as_deref(), force),
        Command::Info { map, game, cache } => {
            cmd_extract::info(&map, game.as_deref(), cache.as_deref())
        }
        Command::ExportObj {
            map,
            out,
            filter,
            region,
            no_y_up,
            game,
            cache,
        } => cmd_extract::export_obj(
            &map,
            &out,
            &filter,
            region.as_deref(),
            !no_y_up,
            game.as_deref(),
            cache.as_deref(),
        ),
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
        Command::Vpk { command } => match command {
            VpkCommand::Ls {
                vpk,
                filter,
                ext,
                long,
                game,
            } => cmd_vpk::ls(
                &vpk,
                game.as_deref(),
                filter.as_deref(),
                ext.as_deref(),
                long,
            ),
            VpkCommand::Cat {
                vpk,
                path,
                out,
                game,
            } => cmd_vpk::cat(&vpk, game.as_deref(), &path, &out),
            VpkCommand::Verify { vpk, game } => cmd_vpk::verify(&vpk, game.as_deref()),
        },
        Command::Res(args) => cmd_res::run(&cmd_res::ResArgs {
            file: &args.file,
            vpk: args.vpk.as_deref(),
            game: args.game.as_deref(),
            block: args.block.as_deref(),
            kv3: args.kv3,
            full_blobs: args.full_blobs,
        }),
    }
}
