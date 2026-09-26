# viewer/lib

Vendored third-party ES modules, checked in so the viewer works fully offline (no npm, no CDN,
no build step at runtime). See `NOTICE.md` at the repo root for licenses.

## three/

`three.js`, version **0.186.0**, MIT license. Downloaded once as the npm registry tarball:

```
https://registry.npmjs.org/three/-/three-0.186.0.tgz
sha1  08f70ce80dffa9247a567b421165bec630e86f8d
```

Files copied unmodified from the tarball's `package/`:

- `build/three.module.js` and `build/three.core.js` - the ES module build, split across two
  files in this version of the package (`three.module.js` re-exports from `three.core.js`, its
  only import). No `three.module.min.js` is published for this version, so the unminified build
  is used as-is.
- `examples/jsm/loaders/GLTFLoader.js` - loads `render.glb`.
- `examples/jsm/utils/BufferGeometryUtils.js`, `examples/jsm/utils/SkeletonUtils.js` -
  `GLTFLoader`'s own two same-package dependencies.
- `examples/jsm/controls/OrbitControls.js` - the orbit-around-a-point mode used by
  `viewer/js/scene3d.js`. Free flight and the first-person view use their own pointer-lock and
  yaw/pitch code in `scene3d.js` instead of the package's `PointerLockControls.js`: that helper
  rotates the camera around world +Y and only fits a Y-up scene, and this viewer's scene is Z-up
  (`s6f3a3_map.md` §6) - so it is not vendored here.
- `examples/jsm/libs/meshopt_decoder.module.js` - registered with `GLTFLoader.setMeshoptDecoder()`
  in `scene3d.js`. `render.glb` doesn't use `EXT_meshopt_compression`/`KHR_meshopt_compression`
  today, so this is a no-op until `crates/s2render` starts writing compressed geometry, but
  `GLTFLoader` throws on a meshopt-compressed file with no decoder registered - vendoring it now
  means a future `render.glb` doesn't need a matching viewer change.

Every one of these files imports the bare specifier `"three"`, resolved by `viewer/index.html`'s
import map to `./lib/three/build/three.module.js` - nothing here was edited to add relative
paths.

To update: download a newer `three-<version>.tgz` from the npm registry, verify its `dist.shasum`
from `https://registry.npmjs.org/three`, and re-copy the same file list above.

## three-mesh-bvh/

`three-mesh-bvh`, version **0.9.15**, MIT license (Garrett Johnson). Downloaded once as the npm
registry tarball:

```
https://registry.npmjs.org/three-mesh-bvh/-/three-mesh-bvh-0.9.15.tgz
sha1    0e070be254527155b4cc9285277cddaf531d8ef1
sha512  f717440e40b31684fc54f147f43ce7c0549d6da2048ccbccd74e88aaaed326eaeb22179428371d5b8a6b7c94c2c6bca28785d2fd4c56b341f72b830b31117239
```

`0.9.15` is the newest published version whose `peerDependencies.three` (`>= 0.159.0`) still
accepts the vendored three 0.186.0 - unlike three.js itself, the package no longer publishes a
version pinned to a specific three release.

Files copied unmodified from the tarball's `package/`:

- `build/index.module.js` - the package's own bundled ES module build (imports the bare specifier
  `"three"`, resolved by the same import map entry as every other vendored file here; nothing
  edited). The package also publishes `build/index.umd.cjs` (CommonJS, unused) and an unbundled
  `src/` tree (its own `"module"` entry point, not used here since it pulls in dozens of
  same-package relative imports the import map doesn't cover) - only the bundle is vendored.
  `build/index.module.js.map` (source map) is not vendored, same as three.js's own build above.
- `LICENSE`.

Used by `viewer/js/scene3d.js`'s `loadCollisionMesh` (`geometry.boundsTree = new MeshBVH(geometry)`,
`pickProxy.raycast = acceleratedRaycast`) to accelerate raycasts against the collision mesh - a
brute-force `Raycaster.intersectObject` over the largest maps' collision meshes took tens of
milliseconds per cast, stalling the 3D hover preview (`s6k_draw_in_3d.md` review fix item 5).

To update: download a newer `three-mesh-bvh-<version>.tgz` whose `peerDependencies.three` still
accepts the vendored three version, verify its `dist.shasum` from
`https://registry.npmjs.org/three-mesh-bvh`, and re-copy `build/index.module.js` + `LICENSE`.
