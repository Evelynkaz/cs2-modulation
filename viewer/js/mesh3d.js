// Parses `GET /api/mesh`'s binary `SM3D` v2 payload (`crates/server/src/mesh_payload.rs`) into
// typed arrays a three.js `BufferGeometry` can use directly - the same collision geometry the
// solver runs on, used here for target-picking (raycasting) and the "show collisions" overlay
// (`s6f3b_viewer3d.md` F3b-1b).

const MESH_MAGIC = 0x4433_4d53; // "SM3D", little-endian.
const MESH_FORMAT_VERSION = 2;

/**
 * `buffer`: an `ArrayBuffer` from `GET /api/mesh`. Throws on a bad magic/version (a stale cache
 * or a server mismatch) - callers should treat that as a load error, not render garbage.
 * Returns `{ positions: Float32Array (x,y,z per vertex), groups: { world, phantom, door,
 * breakable }: Uint32Array }` - each group is a flat triangle-index list into `positions`.
 */
export function parseSm3d(buffer) {
  const view = new DataView(buffer);
  const magic = view.getUint32(0, true);
  const version = view.getUint32(4, true);
  if (magic !== MESH_MAGIC) {
    throw new Error(`SM3D: bad magic 0x${magic.toString(16)}`);
  }
  if (version !== MESH_FORMAT_VERSION) {
    throw new Error(`SM3D: unsupported version ${version}`);
  }
  const vertexCount = view.getInt32(8, true);
  const worldCount = view.getInt32(12, true);
  const phantomCount = view.getInt32(16, true);
  const doorCount = view.getInt32(20, true);
  const breakableCount = view.getInt32(24, true);

  let offset = 28;
  const positions = new Float32Array(buffer, offset, vertexCount * 3);
  offset += vertexCount * 12;

  function readGroup(count) {
    const arr = new Uint32Array(buffer, offset, count);
    offset += count * 4;
    return arr;
  }
  const world = readGroup(worldCount);
  const phantom = readGroup(phantomCount);
  const door = readGroup(doorCount);
  const breakable = readGroup(breakableCount);

  return { positions, groups: { world, phantom, door, breakable } };
}

/**
 * Concatenates every group's indices into one flat `Uint32Array`, with three.js geometry
 * `groups` (material index per contiguous range) matching the four SM3D categories, in the same
 * order `mesh_payload.rs` writes them - group 0 world, 1 phantom, 2 door, 3 breakable.
 */
export function flattenGroups(groups) {
  const { world, phantom, door, breakable } = groups;
  const total = world.length + phantom.length + door.length + breakable.length;
  const indices = new Uint32Array(total);
  const ranges = [];
  let at = 0;
  for (const [materialIndex, g] of [world, phantom, door, breakable].entries()) {
    indices.set(g, at);
    ranges.push({ start: at, count: g.length, materialIndex });
    at += g.length;
  }
  return { indices, ranges };
}
