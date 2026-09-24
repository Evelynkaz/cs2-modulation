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

Every one of these files imports the bare specifier `"three"`, resolved by `viewer/index.html`'s
import map to `./lib/three/build/three.module.js` - nothing here was edited to add relative
paths.

To update: download a newer `three-<version>.tgz` from the npm registry, verify its `dist.shasum`
from `https://registry.npmjs.org/three`, and re-copy the same file list above.
