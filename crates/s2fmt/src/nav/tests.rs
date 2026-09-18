//! Synthetic `.nav` fixtures, hand-assembled to mirror `parse_nav`'s expected byte layout
//! (`NavMeshFile.cs`). Unlike `kv3`'s round-trip tests (which have both a reader and a writer
//! that could share the same misunderstanding), there's no `.nav` writer in this crate to check
//! against, so these fixtures are built field-by-field against the VRF reference instead.

use super::*;
use crate::kv3::FORMAT_GENERIC;
use crate::kv3::test_writer::{Compression, TestNode, build};

/// A tiny little-endian byte-vector builder mirroring the write order `parse_nav`'s helpers read
/// in.
struct W(Vec<u8>);

impl W {
    fn new() -> Self {
        W(Vec::new())
    }

    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }

    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn vec3(&mut self, v: [f32; 3]) -> &mut Self {
        for c in v {
            self.f32(c);
        }
        self
    }

    fn cstr(&mut self, s: &str) -> &mut Self {
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
        self
    }

    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }

    fn align8(&mut self) -> &mut Self {
        while !self.0.len().is_multiple_of(8) {
            self.0.push(0);
        }
        self
    }

    fn kv3_doc(&mut self, doc: &[u8]) -> &mut Self {
        self.align8();
        self.bytes(doc)
    }
}

fn minimal_kv3() -> Vec<u8> {
    build(
        &TestNode::Object(vec![]),
        5,
        Compression::None,
        FORMAT_GENERIC,
    )
}

/// A fully-populated set of per-hull generation values, used to write (and independently compute
/// the expected [`GenerationHullParams`] for) a hull entry. Every field gets a value derived from
/// `seed` at a distinct "digit", so a reader bug that swaps or drops a field shows up as a wrong
/// value rather than an accidental match.
#[derive(Clone, Copy)]
struct HullFixture {
    seed: i32,
}

impl HullFixture {
    fn new(seed: i32) -> Self {
        HullFixture { seed }
    }

    fn enabled(self) -> bool {
        self.seed % 2 == 0
    }
    fn radius(self) -> f32 {
        10.0 + self.seed as f32
    }
    fn height(self) -> f32 {
        20.0 + self.seed as f32
    }
    fn short_height_enabled(self) -> bool {
        self.seed % 2 == 1
    }
    fn short_height(self) -> f32 {
        30.0 + self.seed as f32
    }
    fn agent_crawl_enabled(self) -> bool {
        self.seed % 3 == 0
    }
    fn agent_crawl_height(self) -> f32 {
        40.0 + self.seed as f32
    }
    fn max_climb(self) -> f32 {
        50.0 + self.seed as f32
    }
    fn max_slope(self) -> i32 {
        60 + self.seed
    }
    fn max_jump_down_dist(self) -> f32 {
        70.0 + self.seed as f32
    }
    fn max_jump_horiz_dist_base(self) -> f32 {
        80.0 + self.seed as f32
    }
    fn max_jump_up_dist(self) -> f32 {
        90.0 + self.seed as f32
    }
    fn border_erosion(self) -> i32 {
        100 + self.seed
    }
}

/// Writes one hull entry per `read_hull_params`'s field gating.
fn write_hull(w: &mut W, nav_gen_version: i32, h: HullFixture) {
    if nav_gen_version >= 9 {
        w.u8(h.enabled() as u8);
    }
    w.f32(h.radius()).f32(h.height());
    if nav_gen_version >= 9 {
        w.u8(h.short_height_enabled() as u8).f32(h.short_height());
    }
    if nav_gen_version >= 13 {
        w.u8(h.agent_crawl_enabled() as u8)
            .f32(h.agent_crawl_height());
    }
    w.f32(h.max_climb())
        .i32(h.max_slope())
        .f32(h.max_jump_down_dist())
        .f32(h.max_jump_horiz_dist_base())
        .f32(h.max_jump_up_dist());
    if nav_gen_version >= 11 {
        w.i32(h.border_erosion());
    }
}

/// The [`GenerationHullParams`] `write_hull` should produce for the same `nav_gen_version`,
/// applying the same field-gating defaults as `read_hull_params`.
fn expected_hull(nav_gen_version: i32, h: HullFixture) -> GenerationHullParams {
    GenerationHullParams {
        enabled: if nav_gen_version >= 9 {
            h.enabled()
        } else {
            true
        },
        radius: h.radius(),
        height: h.height(),
        short_height_enabled: nav_gen_version >= 9 && h.short_height_enabled(),
        short_height: if nav_gen_version >= 9 {
            h.short_height()
        } else {
            0.0
        },
        agent_crawl_enabled: nav_gen_version >= 13 && h.agent_crawl_enabled(),
        agent_crawl_height: if nav_gen_version >= 13 {
            h.agent_crawl_height()
        } else {
            0.0
        },
        max_climb: h.max_climb(),
        max_slope: h.max_slope(),
        max_jump_down_dist: h.max_jump_down_dist(),
        max_jump_horiz_dist_base: h.max_jump_horiz_dist_base(),
        max_jump_up_dist: h.max_jump_up_dist(),
        border_erosion: if nav_gen_version >= 11 {
            h.border_erosion()
        } else {
            0
        },
    }
}

#[test]
fn bad_magic_is_rejected() {
    let bytes = [0u8; 16];
    match parse_nav(&bytes) {
        Err(NavError::BadMagic { magic }) => assert_eq!(magic, 0),
        other => panic!("expected BadMagic, got {other:?}"),
    }
}

#[test]
fn version_29_is_unsupported() {
    let mut w = W::new();
    w.u32(MAGIC).u32(29);
    match parse_nav(&w.0) {
        Err(NavError::UnsupportedVersion { version }) => assert_eq!(version, 29),
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn version_37_is_unsupported() {
    let mut w = W::new();
    w.u32(MAGIC).u32(37);
    match parse_nav(&w.0) {
        Err(NavError::UnsupportedVersion { version }) => assert_eq!(version, 37),
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn truncated_areas_errors() {
    let mut w = W::new();
    w.u32(MAGIC)
        .u32(31)
        .u32(0) // sub_version
        .u32(0) // analyzed flag
        .u32(0) // polygon corner count
        .u32(0) // polygon count
        .u32(1); // area count, then nothing else
    match parse_nav(&w.0) {
        Err(NavError::Truncated { .. }) => {}
        other => panic!("expected Truncated, got {other:?}"),
    }
}

#[test]
fn bad_polygon_index_is_rejected() {
    let mut w = W::new();
    w.u32(MAGIC)
        .u32(31)
        .u32(0) // sub_version
        .u32(0) // analyzed flag
        .u32(0) // polygon corner count
        .u32(0) // polygon count (empty table)
        .u32(1) // area count
        .u32(1) // area id
        .u64(0) // attribute flags
        .u8(0) // hull index
        .u32(5); // polygon index -- out of range against an empty table
    match parse_nav(&w.0) {
        Err(NavError::BadPolygonIndex { index, count }) => {
            assert_eq!(index, 5);
            assert_eq!(count, 0);
        }
        other => panic!("expected BadPolygonIndex, got {other:?}"),
    }
}

#[test]
fn negative_hull_count_is_rejected() {
    let mut w = W::new();
    w.u32(MAGIC).u32(31).u32(0).u32(0); // sub_version 0, not analyzed
    w.u32(0).u32(0); // empty corner/polygon table
    w.u32(0); // area count
    w.u32(0); // ladder count
    w.u32(0); // transformed bounds count
    w.i32(5) // nav_gen_version
        .u32(0)
        .f32(1.0)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .i32(1)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .f32(1.0)
        .i32(1)
        .i32(-1); // hull count -- negative
    match parse_nav(&w.0) {
        Err(NavError::BadHullCount { count }) => assert_eq!(count, -1),
        other => panic!("expected BadHullCount, got {other:?}"),
    }
}

#[test]
fn v31_minimal_file_parses() {
    let mut w = W::new();
    w.u32(MAGIC).u32(31).u32(0).u32(0); // sub_version 0, not analyzed
    w.u32(0).u32(0); // empty corner/polygon table
    // v31 < 32, so no reserved u32; v31 < 35, so no movable mesh ids.
    w.u32(0); // area count
    w.u32(0); // ladder count
    w.u32(0); // transformed bounds count

    // Generation params, nav_gen_version 5: no small_area_on_edge_removal (<7), no hull
    // preset/definitions file (<12), no border_erosion per hull (<11), no
    // gravity_follows_rotation (<12), and (<=11) 2 extra hulls padded after the 1 real one.
    w.i32(5) // nav_gen_version
        .u32(0) // use_project_defaults
        .f32(1.0) // tile_size
        .f32(2.0) // cell_size
        .f32(3.0) // cell_height
        .i32(4) // min_region_size
        .i32(5) // merged_region_size
        .f32(6.0) // mesh_sample_distance
        .f32(7.0) // max_sample_error
        .i32(8) // max_edge_length
        .f32(9.0) // max_edge_error
        .i32(10) // verts_per_poly
        .i32(1); // hull_count
    write_hull(&mut w, 5, HullFixture::new(1));
    write_hull(&mut w, 5, HullFixture::new(2)); // padding hull 2 of 3 (discarded)
    write_hull(&mut w, 5, HullFixture::new(3)); // padding hull 3 of 3 (discarded)
    // sub_version == 0, so no custom data.

    let nav = parse_nav(&w.0).expect("v31 minimal file must parse");
    assert_eq!(nav.version, 31);
    assert_eq!(nav.sub_version, 0);
    assert!(!nav.is_analyzed);
    assert!(nav.areas.is_empty());
    assert!(nav.ladders.is_empty());
    assert!(nav.transformed_bounds.is_empty());
    assert!(nav.custom_data.is_none());
    assert!(nav.unknown_kv3.is_empty());

    let gp = nav.generation_params.expect("generation params");
    assert_eq!(gp.nav_gen_version, 5);
    assert!(!gp.use_project_defaults);
    assert_eq!(gp.tile_size, 1.0);
    assert_eq!(gp.cell_size, 2.0);
    assert_eq!(gp.cell_height, 3.0);
    assert_eq!(gp.min_region_size, 4);
    assert_eq!(gp.merged_region_size, 5);
    assert_eq!(gp.mesh_sample_distance, 6.0);
    assert_eq!(gp.max_sample_error, 7.0);
    assert_eq!(gp.max_edge_length, 8);
    assert_eq!(gp.max_edge_error, 9.0);
    assert_eq!(gp.verts_per_poly, 10);
    assert_eq!(gp.small_area_on_edge_removal, 0.0); // < 7: default
    assert_eq!(gp.hull_preset_name, None);
    assert_eq!(gp.hull_definitions_file, None);
    assert!(!gp.gravity_follows_rotation);
    assert_eq!(gp.hulls, vec![expected_hull(5, HullFixture::new(1))]);
}

/// `nav_gen_version` 9 and 10 (inclusive lower bound of the `enabled`/`short_height*` fields,
/// exclusive of `border_erosion` (>= 11) and the crawl fields (>= 13)) with 3 real hulls, so no
/// padding hulls come into play (see `generation_params_hull_padding_at_version_11` for that).
#[test]
fn generation_params_hull_fields_at_version_9_and_10() {
    for nav_gen_version in [9, 10] {
        let mut w = W::new();
        w.u32(MAGIC).u32(31).u32(0).u32(0);
        w.u32(0).u32(0);
        w.u32(0); // area count
        w.u32(0); // ladder count
        w.u32(0); // transformed bounds count
        w.i32(nav_gen_version)
            .u32(0)
            .f32(1.0)
            .f32(1.0)
            .f32(1.0)
            .i32(1)
            .i32(1)
            .f32(1.0)
            .f32(1.0)
            .i32(1)
            .f32(1.0)
            .i32(3) // verts_per_poly
            .f32(2.0) // small_area_on_edge_removal (>= 7)
            .i32(3); // hull_count -- already 3, so no padding kicks in

        let fixtures = [
            HullFixture::new(1),
            HullFixture::new(2),
            HullFixture::new(3),
        ];
        for &h in &fixtures {
            write_hull(&mut w, nav_gen_version, h);
        }

        let nav = parse_nav(&w.0)
            .unwrap_or_else(|e| panic!("nav_gen_version {nav_gen_version} must parse: {e}"));
        let gp = nav.generation_params.expect("generation params");
        let expected: Vec<_> = fixtures
            .iter()
            .map(|&h| expected_hull(nav_gen_version, h))
            .collect();
        assert_eq!(
            gp.hulls, expected,
            "nav_gen_version {nav_gen_version} hull fields"
        );
    }
}

/// `nav_gen_version` 11: `border_erosion` is present (>= 11) but the crawl fields aren't
/// (< 13), and (`nav_gen_version` <= 11) a `hull_count` of 1 still causes 2 extra hulls to be
/// read and discarded to pad out to 3.
#[test]
fn generation_params_hull_padding_at_version_11() {
    let mut w = W::new();
    w.u32(MAGIC).u32(31).u32(0).u32(0);
    w.u32(0).u32(0);
    w.u32(0); // area count
    w.u32(0); // ladder count
    w.u32(0); // transformed bounds count
    w.i32(11)
        .u32(0)
        .f32(1.0)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .i32(1)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .f32(1.0)
        .i32(3)
        .f32(2.0) // small_area_on_edge_removal (>= 7)
        .i32(1); // hull_count -- 1, so 2 padding hulls follow

    write_hull(&mut w, 11, HullFixture::new(1));
    write_hull(&mut w, 11, HullFixture::new(2)); // padding (discarded)
    write_hull(&mut w, 11, HullFixture::new(3)); // padding (discarded)

    let nav = parse_nav(&w.0).expect("v11 hull padding file must parse");
    let gp = nav.generation_params.expect("generation params");
    assert_eq!(gp.hulls, vec![expected_hull(11, HullFixture::new(1))]);
}

#[test]
fn v35_file_round_trips_areas_ladders_and_custom_data() {
    let mut w = W::new();
    w.u32(MAGIC).u32(35).u32(1).u32(1); // sub_version 1, analyzed

    // Shared corner table + 1 polygon (v35: polygons carry a movable mesh id).
    w.u32(3)
        .vec3([0.0, 0.0, 0.0])
        .vec3([1.0, 0.0, 0.0])
        .vec3([1.0, 1.0, 0.0])
        .u32(1) // polygon count
        .u8(3) // polygon corner count
        .u32(0)
        .u32(1)
        .u32(2) // corner indices
        .u32(0); // movable mesh id (0, i.e. attached to a movable mesh)

    w.u32(0); // v35 >= 32: reserved u32

    w.u32(1).cstr("mesh0").bytes(&[0u8; 48]); // 1 movable mesh id + reserved transform

    // 1 area referencing polygon 0.
    w.u32(1)
        .u32(42) // area id
        .u64(flags::JUMP | flags::STAIRS)
        .u8(2) // hull index
        .u32(0) // polygon index
        .f32(3.25); // unknown float (deliberately nonzero)
    // Connections: one distinct entry per corner (3 corners), to catch a swapped-order bug.
    w.u32(1).u32(200).u32(10); // corner 0
    w.u32(1).u32(201).u32(11); // corner 1
    w.u32(1).u32(202).u32(12); // corner 2
    w.u8(0).u32(0); // legacy hiding spot / spot encounter counts
    w.u32(1).u32(7); // ladders above: [7]
    w.u32(1).u32(8); // ladders below: [8]

    // 1 ladder (v35: has bottom_left/right). Every "area id" field below is a distinct raw value
    // unrelated to the area above (they're not resolved at parse time; see `NavLadder`'s doc
    // comment), to catch a field ordering bug.
    w.u32(1)
        .u32(55) // id
        .f32(12.5) // width
        .vec3([1.5, 2.5, 3.5]) // top
        .vec3([4.5, 5.5, 6.5]) // bottom
        .f32(44.5) // length
        .u32(1) // direction: East
        .u32(201) // top_forward_area
        .u32(202) // top_left_area
        .u32(203) // top_right_area
        .u32(204) // top_behind_area
        .u32(205) // bottom_area
        .u32(206) // bottom_left_area
        .u32(207); // bottom_right_area

    // 1 transformed bounds entry.
    w.u32(1).vec3([-1.0, -2.0, -3.0]).vec3([4.0, 5.0, 6.0]);
    for v in 1..=12 {
        w.f32(v as f32);
    }

    // Generation params, nav_gen_version 13: every optional field present.
    w.i32(13)
        .u32(1) // use_project_defaults
        .f32(10.0)
        .f32(0.3)
        .f32(0.2) // tile/cell size/height
        .i32(8)
        .i32(20) // min/merged region size
        .f32(6.0)
        .f32(1.0) // mesh sample distance/max sample error
        .i32(12)
        .f32(1.3)
        .i32(6) // max edge length/error, verts per poly
        .f32(4.0) // small area on edge removal (>=7)
        .cstr("Default") // hull preset name (>=12)
        .cstr("hulls.txt") // hull definitions file (>=12)
        .i32(1); // hull count
    write_hull(&mut w, 13, HullFixture::new(1));
    w.u8(1); // gravity_follows_rotation (>=12)

    let custom_data = minimal_kv3();
    w.kv3_doc(&custom_data); // sub_version > 0

    let nav = parse_nav(&w.0).expect("v35 file must parse");
    assert_eq!(nav.version, 35);
    assert_eq!(nav.sub_version, 1);
    assert!(nav.is_analyzed);
    assert!(nav.unknown_kv3.is_empty());
    assert_eq!(nav.movable_mesh_ids, vec!["mesh0".to_string()]);

    assert_eq!(nav.areas.len(), 1);
    let area = &nav.areas[0];
    assert_eq!(area.id, 42);
    assert_eq!(area.attribute_flags, flags::JUMP | flags::STAIRS);
    assert_eq!(area.hull_index, 2);
    assert_eq!(area.movable_mesh_id, Some(0));
    assert_eq!(area.corners.len(), 3);
    assert_eq!(area.unknown_f32, 3.25);
    assert_eq!(area.connections.len(), 3);
    assert_eq!(
        area.connections[0],
        vec![NavConnection {
            area_id: 200,
            edge_id: 10
        }]
    );
    assert_eq!(
        area.connections[1],
        vec![NavConnection {
            area_id: 201,
            edge_id: 11
        }]
    );
    assert_eq!(
        area.connections[2],
        vec![NavConnection {
            area_id: 202,
            edge_id: 12
        }]
    );
    assert_eq!(area.ladders_above, vec![7]);
    assert_eq!(area.ladders_below, vec![8]);

    assert_eq!(nav.ladders.len(), 1);
    let ladder = &nav.ladders[0];
    assert_eq!(ladder.id, 55);
    assert_eq!(ladder.width, 12.5);
    assert_eq!(ladder.length, 44.5);
    assert_eq!(ladder.top, [1.5, 2.5, 3.5]);
    assert_eq!(ladder.bottom, [4.5, 5.5, 6.5]);
    assert_eq!(ladder.direction, NavDirection::East);
    assert_eq!(ladder.top_forward_area, 201);
    assert_eq!(ladder.top_left_area, 202);
    assert_eq!(ladder.top_right_area, 203);
    assert_eq!(ladder.top_behind_area, 204);
    assert_eq!(ladder.bottom_area, 205);
    assert_eq!(ladder.bottom_left_area, Some(206));
    assert_eq!(ladder.bottom_right_area, Some(207));

    assert_eq!(nav.transformed_bounds.len(), 1);
    let bounds = &nav.transformed_bounds[0];
    assert_eq!(bounds.min, [-1.0, -2.0, -3.0]);
    assert_eq!(bounds.max, [4.0, 5.0, 6.0]);
    assert_eq!(bounds.transform[0], [1.0, 2.0, 3.0, 4.0]);
    assert_eq!(bounds.transform[1], [5.0, 6.0, 7.0, 8.0]);
    assert_eq!(bounds.transform[2], [9.0, 10.0, 11.0, 12.0]);

    assert!(nav.custom_data.is_some());
    assert_eq!(nav.area(42).map(|a| a.id), Some(42));
    assert_eq!(nav.hull_areas(2).count(), 1);
    assert_eq!(nav.hull_areas(9).count(), 0);

    let gp = nav.generation_params.expect("generation params");
    assert!(gp.use_project_defaults);
    assert_eq!(gp.tile_size, 10.0);
    assert_eq!(gp.cell_size, 0.3);
    assert_eq!(gp.cell_height, 0.2);
    assert_eq!(gp.min_region_size, 8);
    assert_eq!(gp.merged_region_size, 20);
    assert_eq!(gp.mesh_sample_distance, 6.0);
    assert_eq!(gp.max_sample_error, 1.0);
    assert_eq!(gp.max_edge_length, 12);
    assert_eq!(gp.max_edge_error, 1.3);
    assert_eq!(gp.verts_per_poly, 6);
    assert_eq!(gp.small_area_on_edge_removal, 4.0);
    assert_eq!(gp.hull_preset_name.as_deref(), Some("Default"));
    assert_eq!(gp.hull_definitions_file.as_deref(), Some("hulls.txt"));
    assert!(gp.gravity_follows_rotation);
    assert_eq!(gp.hulls, vec![expected_hull(13, HullFixture::new(1))]);
}

#[test]
fn v36_file_reads_three_unknown_kv3_blocks() {
    let mut w = W::new();
    w.u32(MAGIC).u32(36).u32(0).u32(0); // sub_version 0, not analyzed

    let kv3_a = minimal_kv3();
    w.kv3_doc(&kv3_a); // KV3Unknown1 (v36+)

    w.u32(0).u32(0); // empty corner/polygon table (v31+)
    w.u32(0); // reserved u32 (v32+)
    w.u32(0); // movable mesh count (v35+)

    let kv3_b = minimal_kv3();
    w.kv3_doc(&kv3_b); // KV3Unknown2 (v36+)

    w.u32(0); // area count
    w.u32(0); // ladder count
    w.u32(0); // transformed bounds count

    // Generation params, nav_gen_version 0: nothing optional is present.
    w.i32(0)
        .u32(0)
        .f32(1.0)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .i32(1)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .f32(1.0)
        .i32(3)
        .i32(3); // hull_count 3 -- nav_gen_version <= 11 always stores 3 hulls regardless
    let fixtures = [
        HullFixture::new(1),
        HullFixture::new(2),
        HullFixture::new(3),
    ];
    for &h in &fixtures {
        write_hull(&mut w, 0, h);
    }

    let kv3_c = minimal_kv3();
    w.kv3_doc(&kv3_c); // KV3Unknown3 (v36+)
    // sub_version == 0, so no custom data.

    let nav = parse_nav(&w.0).expect("v36 file must parse");
    assert_eq!(nav.version, 36);
    assert_eq!(nav.unknown_kv3.len(), 3);
    assert!(nav.custom_data.is_none());
    let gp = nav.generation_params.expect("generation params");
    assert_eq!(gp.nav_gen_version, 0);
    let expected: Vec<_> = fixtures.iter().map(|&h| expected_hull(0, h)).collect();
    assert_eq!(gp.hulls, expected);
    assert_eq!(gp.small_area_on_edge_removal, 0.0);
    assert_eq!(gp.hull_preset_name, None);
}

#[test]
fn trailing_data_after_last_section_is_rejected() {
    let mut w = W::new();
    w.u32(MAGIC).u32(31).u32(0).u32(0); // sub_version 0, not analyzed
    w.u32(0).u32(0); // empty corner/polygon table
    w.u32(0); // area count
    w.u32(0); // ladder count
    w.u32(0); // transformed bounds count
    w.i32(0) // nav_gen_version
        .u32(0)
        .f32(1.0)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .i32(1)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .f32(1.0)
        .i32(3)
        .i32(3); // hull_count 3 -- nav_gen_version <= 11 always stores 3 hulls regardless
    write_hull(&mut w, 0, HullFixture::new(1));
    write_hull(&mut w, 0, HullFixture::new(2));
    write_hull(&mut w, 0, HullFixture::new(3));
    w.bytes(&[1, 2, 3]); // trailing garbage

    match parse_nav(&w.0) {
        Err(NavError::TrailingData { count }) => assert_eq!(count, 3),
        other => panic!("expected TrailingData, got {other:?}"),
    }
}

/// A duplicate area id: `NavMesh::area` should resolve to the later one in file order (matching
/// VRF's `Dictionary<uint, NavMeshArea>`, where a later `AddArea` overwrites the earlier entry).
#[test]
fn area_lookup_resolves_duplicate_ids_to_the_last_one() {
    let mut w = W::new();
    w.u32(MAGIC).u32(30).u32(0).u32(0); // version 30: areas store corners inline, no polygon table

    // 2 areas sharing id 1, with different hull indices so they're distinguishable.
    w.u32(2);
    for hull_index in [3u8, 4u8] {
        w.u32(1) // area id (duplicated)
            .u64(0) // attribute flags
            .u8(hull_index)
            .u32(0) // corner count (v31: inline corners, none here)
            .f32(0.0) // unknown float
            .u8(0) // legacy hiding spot count
            .u32(0) // legacy spot encounter count
            .u32(0) // ladders above count
            .u32(0); // ladders below count
    }

    w.u32(0); // ladder count
    w.u32(0); // transformed bounds count
    w.i32(0)
        .u32(0)
        .f32(1.0)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .i32(1)
        .f32(1.0)
        .f32(1.0)
        .i32(1)
        .f32(1.0)
        .i32(3)
        .i32(3);
    write_hull(&mut w, 0, HullFixture::new(1));
    write_hull(&mut w, 0, HullFixture::new(2));
    write_hull(&mut w, 0, HullFixture::new(3));

    let nav = parse_nav(&w.0).expect("file with duplicate area ids must parse");
    assert_eq!(nav.areas.len(), 2);
    let resolved = nav.area(1).expect("area id 1 must resolve");
    assert_eq!(resolved.hull_index, 4, "the later duplicate must win");
}
