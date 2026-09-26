mod cmd_extract;
mod cmd_render;
mod cmd_res;
mod cmd_serve;
mod cmd_sim;
mod cmd_solver;
mod cmd_viewerdata;
mod cmd_vpk;
mod constants;
mod game_path;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "cs2mod",
    version,
    about = "CS2 grenade lineup calculator; run with no command to start the viewer server \
             and open it in the browser (same as `cs2mod serve --open`)"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
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
        /// Re-extract even if a complete cache entry for the map's current .vpk already exists.
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
    Throw {
        /// Map name (e.g. `de_mirage`).
        map: String,
        /// Feet position `x,y,z`; eye is derived from `--type`'s eye height.
        #[arg(long, allow_hyphen_values = true)]
        pos: Option<String>,
        /// Eye position `x,y,z`, used as-is (overrides `--pos`).
        #[arg(long, allow_hyphen_values = true)]
        eye: Option<String>,
        /// `pitch,yaw` degrees; required with `--pos`/`--eye`.
        #[arg(long, allow_hyphen_values = true)]
        ang: Option<String>,
        #[arg(long, default_value = "stand")]
        r#type: String,
        #[arg(long, default_value = "left")]
        click: String,
        #[arg(long, default_value_t = 0.0)]
        run_deg: f32,
        /// `setpos x y z;setang p y r`; overrides `--pos`/`--eye`/`--ang`.
        #[arg(long)]
        getpos: Option<String>,
        /// `throw-constants.json` path; defaults to sim's own defaults.
        #[arg(long)]
        constants: Option<PathBuf>,
        /// Writes per-tick positions and bounces to this JSON file.
        #[arg(long)]
        trace: Option<PathBuf>,
        #[arg(long)]
        game: Option<PathBuf>,
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Simulate a smoke grenade and its resulting volume.
    Smoke {
        map: String,
        #[arg(long, allow_hyphen_values = true)]
        rest: String,
        #[arg(long, default_value = "uncalibrated")]
        params: String,
        /// Vision-mask group names, comma-separated (reference `--attrs "Default,default"`).
        #[arg(long, default_value = "Default,default")]
        attrs: String,
        #[arg(long)]
        obj: Option<PathBuf>,
        #[arg(long)]
        game: Option<PathBuf>,
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Check sightline occlusion between two points.
    Sightline {
        map: String,
        #[arg(long, allow_hyphen_values = true)]
        from: String,
        #[arg(long, allow_hyphen_values = true)]
        to: String,
        #[arg(long, allow_hyphen_values = true)]
        rest: Option<String>,
        #[arg(long, default_value = "uncalibrated")]
        params: String,
        /// Vision-mask group names, comma-separated (reference `--attrs "Default,default"`),
        /// used for both the voxel grid and the exact geometry check.
        #[arg(long, default_value = "Default,default")]
        attrs: String,
        #[arg(long)]
        game: Option<PathBuf>,
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Precompute every stand spot on a map, using the real player hull.
    Standspots {
        /// Map name (e.g. `de_mirage`).
        map: String,
        /// Lattice spacing, in units.
        #[arg(long, default_value_t = 16.0)]
        step: f32,
        /// Recompute even if a cached `standspots.json` already matches.
        #[arg(long)]
        force: bool,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; defaults to `<repo-or-cwd>/cache`.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Solve for a lineup that hits a target.
    #[allow(clippy::too_many_arguments)]
    Solve {
        /// Map name (e.g. `de_mirage`).
        map: String,
        /// `x,y` or `x,y,z`; without a `z`, the height is derived from nav data.
        #[arg(long, allow_hyphen_values = true)]
        target: String,
        #[arg(long, default_value_t = 80.0)]
        tolerance: f32,
        /// Origin click `x,y[,z]`: only lineups near this spot.
        #[arg(long, allow_hyphen_values = true, conflicts_with = "getpos")]
        from: Option<String>,
        /// `setpos x y z;setang p y r`, in place of `--from`: with a `setang` line,
        /// feet = eye - (0,0,64.06); without one the position is already feet.
        #[arg(long, conflicts_with = "from")]
        getpos: Option<String>,
        /// Defaults to 300u with `--from`/`--getpos`, else 3100u (map-wide).
        #[arg(long)]
        reach: Option<f32>,
        /// With `--from`: only that exact origin (and its wall/corner pins), no lattice neighbour.
        #[arg(long)]
        exact: bool,
        /// Halve the angle lattice step for a more thorough (slower) search.
        #[arg(long)]
        fine: bool,
        /// Comma-separated: stand,crouch,jump,crouchjump,runjump.
        #[arg(long)]
        types: Option<String>,
        /// Comma-separated: left,both,right.
        #[arg(long)]
        clicks: Option<String>,
        /// Comma-separated: glass,doors (collision groups to treat as gone).
        #[arg(long)]
        broken: Option<String>,
        /// `t`, `ct`, or `all`: search only from spawn positions.
        #[arg(long)]
        spawns: Option<String>,
        /// `corner`: only lineups wedged into a corner. `wall`: also lineups pressed against a
        /// single wall.
        #[arg(long)]
        pin: Option<String>,
        /// Narrow the aim re-verify search/probe step from 0.6°/±2 steps to 0.2°/±4 steps (±0.8°)
        /// and also report `stability_wide`, the same 5-probe stability at the coarser 0.6° window.
        #[arg(long)]
        precise: bool,
        /// Also run the exhaustive exact-spot referee (exact-origin solves only).
        #[arg(long)]
        referee: bool,
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Writes the full ranked lineup list as JSON.
        #[arg(long)]
        json: Option<PathBuf>,
        /// `throw-constants.json` path; defaults to `data/throw-constants.json` if present,
        /// else sim's own defaults.
        #[arg(long)]
        constants: Option<PathBuf>,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; defaults to `<repo-or-cwd>/cache`.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Calibrate throw constants from measured throws.
    Calibrate {
        map: String,
        #[arg(long)]
        throws: PathBuf,
        #[arg(long, default_value = "data/throw-constants.json")]
        out: PathBuf,
        /// Comma-separated constants to sweep (e.g. `speed,jumpv`); everything is frozen
        /// (reported only, never fit) unless named here.
        #[arg(long, value_delimiter = ',')]
        unfreeze: Vec<String>,
        #[arg(long)]
        game: Option<PathBuf>,
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Replay a recorded throw corpus offline.
    Replay {
        maps: Vec<String>,
        /// Run every map that has corpus files under `--corpus`.
        #[arg(long)]
        all: bool,
        /// Corpus directory; defaults to `CS2MOD_CORPUS`.
        #[arg(long)]
        corpus: Option<PathBuf>,
        /// Only replay throws recorded on this game build.
        #[arg(long)]
        build: Option<String>,
        #[arg(long, default_value_t = 0)]
        worst: usize,
        #[arg(long)]
        json: Option<PathBuf>,
        /// `throw-constants.json` path; defaults to `data/throw-constants.json` if present,
        /// else sim's own defaults.
        #[arg(long)]
        constants: Option<PathBuf>,
        #[arg(long)]
        game: Option<PathBuf>,
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Render a 2D radar PNG and `viewer-map.json` header for a map.
    Viewerdata {
        /// Map name (e.g. `de_mirage`).
        map: String,
        /// World units per pixel.
        #[arg(long, default_value_t = 2.0)]
        pixel_size: f32,
        /// `x0,y0,x1,y1`; defaults to the nav mesh's AABB.
        #[arg(long)]
        region: Option<String>,
        /// Overwrite an existing `viewer-map.png`/`.json`.
        #[arg(long)]
        force: bool,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; defaults to `<repo-or-cwd>/cache`.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Run the HTTP API and web viewer server.
    Serve {
        /// Port to listen on (loopback only); defaults to the saved config's port (8137 if
        /// unconfigured).
        #[arg(long)]
        port: Option<u16>,
        /// Open the viewer in the default browser once the server is up.
        #[arg(long)]
        open: bool,
        /// Game directory (`...\game\csgo`); overrides the saved config for this run.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; overrides the saved config for this run.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
    /// Export a map's visible render geometry (world + entities), materials and textures to
    /// `render.glb`/`render.json` in its extraction cache directory.
    ExportGlb {
        /// Map name (e.g. `de_mirage`).
        map: String,
        /// Texture mip budget: no side longer than this many pixels.
        #[arg(long, default_value_t = 1024)]
        max_texture: u32,
        /// `high` writes irradiance at full resolution (mip 0, 8192²) instead of the mip-1
        /// (4096²) default.
        #[arg(long, default_value = "default")]
        lightmap_quality: String,
        /// Overwrite an existing `render.glb`/`render.json`.
        #[arg(long)]
        force: bool,
        /// Game directory (`...\game\csgo`); defaults to `CS2_GAME_DIR`.
        #[arg(long)]
        game: Option<PathBuf>,
        /// Cache directory; defaults to `<repo-or-cwd>/cache`.
        #[arg(long)]
        cache: Option<PathBuf>,
    },
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
        Ok(code) => std::process::ExitCode::from(code),
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

fn run() -> anyhow::Result<u8> {
    let cli = Cli::parse();

    let Some(command) = cli.command else {
        return match cmd_serve::serve(None, true, None, None) {
            Ok(code) => Ok(code),
            Err(e) => {
                eprintln!("Error: {e:?}");
                eprintln!("Press Enter to close this window.");
                let mut input = String::new();
                let _ = std::io::stdin().read_line(&mut input);
                Ok(1)
            }
        };
    };

    match command {
        Command::Extract {
            maps,
            game,
            cache,
            force,
        } => cmd_extract::extract(&maps, game.as_deref(), cache.as_deref(), force).map(|()| 0),
        Command::Info { map, game, cache } => {
            cmd_extract::info(&map, game.as_deref(), cache.as_deref()).map(|()| 0)
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
        )
        .map(|()| 0),
        Command::Throw {
            map,
            pos,
            eye,
            ang,
            r#type,
            click,
            run_deg,
            getpos,
            constants,
            trace,
            game,
            cache,
        } => cmd_sim::throw(
            &map,
            pos.as_deref(),
            eye.as_deref(),
            ang.as_deref(),
            &r#type,
            &click,
            run_deg,
            getpos.as_deref(),
            constants.as_deref(),
            trace.as_deref(),
            game.as_deref(),
            cache.as_deref(),
        ),
        Command::Smoke {
            map,
            rest,
            params,
            attrs,
            obj,
            game,
            cache,
        } => cmd_sim::smoke(
            &map,
            &rest,
            &params,
            &attrs,
            obj.as_deref(),
            game.as_deref(),
            cache.as_deref(),
        ),
        Command::Sightline {
            map,
            from,
            to,
            rest,
            params,
            attrs,
            game,
            cache,
        } => cmd_sim::sightline(
            &map,
            &from,
            &to,
            rest.as_deref(),
            &params,
            &attrs,
            game.as_deref(),
            cache.as_deref(),
        ),
        Command::Standspots {
            map,
            step,
            force,
            game,
            cache,
        } => {
            cmd_solver::standspots(&map, game.as_deref(), cache.as_deref(), force, step).map(|()| 0)
        }
        Command::Solve {
            map,
            target,
            tolerance,
            from,
            getpos,
            reach,
            exact,
            fine,
            types,
            clicks,
            broken,
            spawns,
            pin,
            precise,
            referee,
            top,
            json,
            constants,
            game,
            cache,
        } => cmd_solver::solve(
            &map,
            &target,
            tolerance,
            from.as_deref(),
            getpos.as_deref(),
            reach,
            exact,
            fine,
            types.as_deref(),
            clicks.as_deref(),
            broken.as_deref(),
            spawns.as_deref(),
            pin.as_deref(),
            precise,
            referee,
            top,
            json.as_deref(),
            constants.as_deref(),
            game.as_deref(),
            cache.as_deref(),
        )
        .map(|()| 0),
        Command::Calibrate {
            map,
            throws,
            out,
            unfreeze,
            game,
            cache,
        } => cmd_sim::calibrate(
            &map,
            &throws,
            &out,
            &unfreeze,
            game.as_deref(),
            cache.as_deref(),
        ),
        Command::Replay {
            maps,
            all,
            corpus,
            build,
            worst,
            json,
            constants,
            game,
            cache,
        } => cmd_sim::replay(
            &maps,
            all,
            corpus.as_deref(),
            build.as_deref(),
            worst,
            json.as_deref(),
            constants.as_deref(),
            game.as_deref(),
            cache.as_deref(),
        ),
        Command::Viewerdata {
            map,
            pixel_size,
            region,
            force,
            game,
            cache,
        } => cmd_viewerdata::viewerdata(
            &map,
            pixel_size,
            region.as_deref(),
            force,
            game.as_deref(),
            cache.as_deref(),
        )
        .map(|()| 0),
        Command::Serve {
            port,
            open,
            game,
            cache,
        } => cmd_serve::serve(port, open, game.as_deref(), cache.as_deref()),
        Command::ExportGlb {
            map,
            max_texture,
            lightmap_quality,
            force,
            game,
            cache,
        } => cmd_render::export_glb(
            &map,
            max_texture,
            lightmap_quality.eq_ignore_ascii_case("high"),
            force,
            game.as_deref(),
            cache.as_deref(),
        )
        .map(|()| 0),
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
            )
            .map(|()| 0),
            VpkCommand::Cat {
                vpk,
                path,
                out,
                game,
            } => cmd_vpk::cat(&vpk, game.as_deref(), &path, &out).map(|()| 0),
            VpkCommand::Verify { vpk, game } => cmd_vpk::verify(&vpk, game.as_deref()).map(|()| 0),
        },
        Command::Res(args) => cmd_res::run(&cmd_res::ResArgs {
            file: &args.file,
            vpk: args.vpk.as_deref(),
            game: args.game.as_deref(),
            block: args.block.as_deref(),
            kv3: args.kv3,
            full_blobs: args.full_blobs,
        })
        .map(|()| 0),
    }
}
