//! Criterion benchmarks: build and query throughput for `UniformGrid`,
//! `Bvh` and `VoxelGrid`, on a synthetic random-triangle scene (always) and
//! on the real mirage mesh when `CS2MOD_CGEO` is set.

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use geom::bvh::Bvh;
use geom::cgeo;
use geom::collider::{Collider, ColliderTriangles};
use geom::filter::{all_mask, grenade_mask};
use geom::grid::UniformGrid;
use geom::math::{Aabb, V3};
use geom::mesh::{CollisionAttribute, CollisionMesh, MeshObject, ObjectKind, SurfaceProperty};
use geom::voxel::VoxelGrid;

struct Rng(u32);
impl Rng {
    fn new(seed: u32) -> Self {
        Rng(seed | 1)
    }
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn f32(&mut self, lo: f32, hi: f32) -> f32 {
        let t = self.next_u32() as f32 / u32::MAX as f32;
        lo + t * (hi - lo)
    }
    fn v3(&mut self, lo: f32, hi: f32) -> V3 {
        V3::new(self.f32(lo, hi), self.f32(lo, hi), self.f32(lo, hi))
    }
    fn unit_v3(&mut self) -> V3 {
        loop {
            let v = self.v3(-1.0, 1.0);
            let len2 = v.length_squared();
            if len2 > 1e-6 && len2 <= 1.0 {
                return v / len2.sqrt();
            }
        }
    }
}

/// A synthetic scene of 200k random triangles scattered over a 4096^3 box,
/// fixed seed.
fn synthetic_mesh() -> CollisionMesh {
    let mut rng = Rng::new(0x5CE7_E5EE);
    let mut mesh = CollisionMesh::new();
    let attr = mesh
        .add_attribute(CollisionAttribute {
            name: "Default".to_string(),
            interact_as: vec![],
            interact_with: vec![],
            interact_exclude: vec![],
            synthetic: false,
        })
        .unwrap();
    let obj = mesh.add_object(MeshObject {
        kind: ObjectKind::WorldMesh,
        classname: None,
        targetname: None,
        model: None,
        hammer_id: None,
        source_index: 0,
        hull_flags: None,
    });
    for _ in 0..200_000 {
        let base = rng.v3(-2048.0, 2048.0);
        let vert = |rng: &mut Rng| {
            let d = rng.v3(-24.0, 24.0);
            [base.x + d.x, base.y + d.y, base.z + d.z]
        };
        let a = vert(&mut rng);
        let b = vert(&mut rng);
        let c = vert(&mut rng);
        let _ = mesh.push_triangles(
            &[a, b, c],
            &[[0, 1, 2]],
            attr,
            |_| SurfaceProperty::NONE,
            obj,
        );
    }
    mesh
}

fn bounds_of(mesh: &CollisionMesh) -> Aabb {
    let (min, max) = mesh.bounds().unwrap();
    Aabb {
        min: V3::from_array(min),
        max: V3::from_array(max),
    }
}

fn bench_scene(c: &mut Criterion, group_name: &str, mesh: &CollisionMesh) {
    let mask = all_mask(mesh);
    let bounds = bounds_of(mesh);

    c.bench_function(&format!("{group_name}/build/grid"), |bch| {
        bch.iter(|| black_box(UniformGrid::build(mesh, &mask, None, 128.0).unwrap()));
    });
    c.bench_function(&format!("{group_name}/build/bvh"), |bch| {
        bch.iter(|| black_box(Bvh::build(mesh, &mask, None).unwrap()));
    });
    c.bench_function(&format!("{group_name}/build/voxel16"), |bch| {
        bch.iter(|| black_box(VoxelGrid::build(mesh, &mask, 16.0, bounds).unwrap()));
    });

    let triangles = ColliderTriangles::build(mesh, &mask, None).unwrap();
    let grid = UniformGrid::build(mesh, &mask, None, 128.0).unwrap();
    let bvh = Bvh::from_triangles(triangles);

    let mut rng = Rng::new(0xB00B_5EED);
    let n = 2000;
    let hull_segments: Vec<(V3, V3)> = (0..n)
        .map(|_| {
            let from = V3::new(
                rng.f32(bounds.min.x, bounds.max.x),
                rng.f32(bounds.min.y, bounds.max.y),
                rng.f32(bounds.min.z, bounds.max.z),
            );
            let to = from + rng.unit_v3() * 5.5;
            (from, to)
        })
        .collect();
    let short_rays: Vec<(V3, V3)> = (0..n)
        .map(|_| {
            let from = V3::new(
                rng.f32(bounds.min.x, bounds.max.x),
                rng.f32(bounds.min.y, bounds.max.y),
                rng.f32(bounds.min.z, bounds.max.z),
            );
            let to = from + rng.unit_v3() * 32.0;
            (from, to)
        })
        .collect();
    let long_rays: Vec<(V3, V3)> = (0..n)
        .map(|_| {
            let from = V3::new(
                rng.f32(bounds.min.x, bounds.max.x),
                rng.f32(bounds.min.y, bounds.max.y),
                rng.f32(bounds.min.z, bounds.max.z),
            );
            let to = from + rng.unit_v3() * 2000.0;
            (from, to)
        })
        .collect();
    let sightlines: Vec<(V3, V3)> = (0..n)
        .map(|_| {
            let from = V3::new(
                rng.f32(bounds.min.x, bounds.max.x),
                rng.f32(bounds.min.y, bounds.max.y),
                rng.f32(bounds.min.z, bounds.max.z),
            );
            let to = from + rng.unit_v3() * 1500.0;
            (from, to)
        })
        .collect();
    let box_centers: Vec<V3> = (0..n)
        .map(|_| {
            V3::new(
                rng.f32(bounds.min.x, bounds.max.x),
                rng.f32(bounds.min.y, bounds.max.y),
                rng.f32(bounds.min.z, bounds.max.z),
            )
        })
        .collect();

    let colliders: [(&str, &dyn Collider); 2] = [
        ("grid", &grid as &dyn Collider),
        ("bvh", &bvh as &dyn Collider),
    ];
    for (collider_name, collider) in colliders {
        let half = V3::new(2.0, 2.0, 2.0);
        c.bench_function(
            &format!("{group_name}/hull_sweep_2u_5.5u/{collider_name}"),
            |bch| {
                bch.iter(|| {
                    for (from, to) in &hull_segments {
                        black_box(collider.first_hit_hull(*from, *to, half, -2.0, None));
                    }
                });
            },
        );
        c.bench_function(&format!("{group_name}/short_ray/{collider_name}"), |bch| {
            bch.iter(|| {
                for (from, to) in &short_rays {
                    black_box(collider.first_hit_ray(*from, *to));
                }
            });
        });
        c.bench_function(
            &format!("{group_name}/long_ray_2000u/{collider_name}"),
            |bch| {
                bch.iter(|| {
                    for (from, to) in &long_rays {
                        black_box(collider.first_hit_ray(*from, *to));
                    }
                });
            },
        );
        c.bench_function(
            &format!("{group_name}/blocked_1500u/{collider_name}"),
            |bch| {
                bch.iter(|| {
                    for (from, to) in &sightlines {
                        black_box(collider.blocked(*from, *to));
                    }
                });
            },
        );
        let player_half = V3::new(16.0, 16.0, 36.0);
        c.bench_function(
            &format!("{group_name}/box_intersects_player_hull/{collider_name}"),
            |bch| {
                bch.iter(|| {
                    for c in &box_centers {
                        black_box(collider.box_intersects(*c, player_half));
                    }
                });
            },
        );
    }
}

fn synthetic_benches(c: &mut Criterion) {
    let mesh = synthetic_mesh();
    bench_scene(c, "synthetic_200k", &mesh);
}

fn real_mesh_benches(c: &mut Criterion) {
    let Ok(path) = std::env::var("CS2MOD_CGEO") else {
        return;
    };
    let (mesh, _meta) = cgeo::load_cgeo(&path).expect("failed to load world.cgeo from CS2MOD_CGEO");
    let _ = grenade_mask(&mesh); // sanity: attribute filters resolve on real data
    bench_scene(c, "mirage_real", &mesh);
}

criterion_group!(benches, synthetic_benches, real_mesh_benches);
criterion_main!(benches);
