//! Real de_mirage radar render; needs `CS2_GAME_DIR` (to resolve the current build/cache) and a
//! prior `cs2mod extract de_mirage`. `specs/s6b_radar.md` §tests (real, ignored).

use std::path::PathBuf;
use std::time::Instant;

use extract::cache;
use extract::game::GameInstall;
use radar::{RadarOptions, render};

fn cache_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("cache")
}

#[test]
#[ignore = "needs CS2_GAME_DIR"]
fn de_mirage_radar_renders_and_round_trips() {
    let game_dir = std::env::var_os("CS2_GAME_DIR").expect("CS2_GAME_DIR must be set");
    let install = GameInstall::new(PathBuf::from(game_dir)).expect("game install");
    let cache_root = cache_root();
    let dir = cache::find_cached(&cache_root, &install, "de_mirage")
        .expect("find_cached")
        .unwrap_or_else(|| panic!("no cache for de_mirage; run `cs2mod extract de_mirage` first"));
    let mesh = cache::load_mesh(&dir).expect("load cached mesh");
    let nav_areas = extract::load_nav_areas(&dir).expect("load nav areas");

    let start = Instant::now();
    let img = render(&mesh, &nav_areas, &RadarOptions::default()).expect("render");
    let elapsed = start.elapsed();

    let covered = img
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] != 0)
        .count();
    let total = (img.width as usize) * (img.height as usize);
    let coverage = covered as f64 / total as f64;
    println!(
        "de_mirage radar: {}x{} in {:.1}s, {:.1}% nav-covered",
        img.width,
        img.height,
        elapsed.as_secs_f64(),
        coverage * 100.0
    );
    assert!(
        coverage > 0.20,
        "expected >20% nav coverage, got {:.1}%",
        coverage * 100.0
    );

    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    let tmp = TempFile(std::env::temp_dir().join("cs2mod-radar-real-test.png"));
    radar::write_png(&img, &tmp.0).expect("write_png");
    let file = std::fs::File::open(&tmp.0).expect("open written png");
    let decoder = png::Decoder::new(file);
    let mut reader = decoder.read_info().expect("read_info");
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("next_frame");
    let decoded = &buf[..info.buffer_size()];
    assert_eq!(
        decoded,
        img.rgba.as_slice(),
        "PNG must decode back to the same bytes"
    );
}
