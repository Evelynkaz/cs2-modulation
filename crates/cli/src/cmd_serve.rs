//! `cs2mod serve`: runs the local HTTP API and web viewer server until Ctrl-C.

use std::path::Path;

pub fn serve(
    port: Option<u16>,
    open: bool,
    game: Option<&Path>,
    cache: Option<&Path>,
) -> anyhow::Result<u8> {
    let cfg = server::ServeConfig {
        port,
        open,
        game: game.map(Path::to_path_buf),
        cache: cache.map(Path::to_path_buf),
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(server::serve(cfg))?;
    Ok(0)
}
