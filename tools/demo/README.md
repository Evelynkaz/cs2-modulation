# Demo replay (`s6v_demo_replay_and_rim_rest.md`, Part C `s6v_c_pro_demos.md`)

Validates the sim against real GOTV demos: your own (`tv_enable 1; tv_delay 0` before `map`,
`tv_record <name>`, `tv_stoprecord`) or downloaded pro/tournament recordings (see "Pro demos"
below).

## 1. Extract launch states from a `.dem`

`extract_launches.py` (Python 3.11, [demoparser2](https://github.com/LaihoE/demoparser) +
pandas/numpy) reads every `CSmokeGrenadeProjectile`'s per-tick positions
(`DemoParser.parse_grenades()`) and its `smokegrenade_detonate` event, fits the first free-flight
ticks as a ballistic model (x/y linear, z quadratic, `g` a free parameter), and recovers the
launch state one tick before the first recorded sample.

Setup (isolated venv, never installed globally):

```
py -3.11 -m venv D:\porject\modulator-work\scratch\demo_venv
D:\porject\modulator-work\scratch\demo_venv\Scripts\pip install demoparser2 pandas numpy
```

Run (single demo):

```
D:\porject\modulator-work\scratch\demo_venv\Scripts\python.exe tools\demo\extract_launches.py \
    path\to\demo.dem -o launches.json
```

Run (several demos and/or a directory of demos - `-o` becomes an output DIRECTORY, one
`<demo stem>.launches.json` per demo, progress on stderr):

```
...\python.exe tools\demo\extract_launches.py path\to\dir_of_dems -o out_dir
```

Prints a one-line summary per projectile to stderr (fit length/residual/`g`, confidence, rest
source) and writes the full JSON to `-o` (or stdout).

Robustness notes (real findings from `okno2.dem`/`big1.dem`, the demos this was built against,
plus pro/tournament demos at scale - see "Pro demos" below):

- **No projectiles at all** (a non-GOTV/local demo, or a demo with no smoke throws): a clear
  message on stderr, an empty `projectiles`/`skipped` list in the JSON, no crash.
- **No matching `smokegrenade_detonate`** (the demo/round ended mid-flight): the entity's own last
  tracked position is used as `game_rest` instead (`rest_source: "last_position"`), flagged in
  `notes` - useful for comparing the tracked flight, not a real landing-spot grade (`replay-demo`
  excludes these from its summary counts as "no detonation").
- **A low-confidence launch fit** (`launch_confident: false`): a short or noisy free-flight window
  (real example: a throw aimed at a narrow rim/ledge close by, bouncing after only ~10-15 ticks)
  can pass the residual gate yet still fit a visibly wrong `g` (known physics: 320.0 u/s²,
  `sim::throw::BASE_GRAVITY * ThrowConstants::default().gravity_scale`) - checked directly, not
  just via residual (`replay-demo` excludes these from its summary counts as "low confidence").
- **A duplicate/recycled entity slot** (real example: `okno2.dem` entity 329, 45 ticks
  byte-for-byte identical to entity 374's own first 45 ticks, 1056 ticks later - not a second
  throw): detected by comparing raw position prefixes against every earlier-thrown entity, UNLESS
  the later entity has its own `smokegrenade_detonate` event in its own tick range (a real throw
  that happens to start with the same first few ticks as an earlier one is not a duplicate -
  `okno2.dem` entity 319/throw `E` is exactly this case). A true duplicate is reported in
  `skipped`, not graded as an independent projectile.

## 2. Replay against the exact sim

`cs2mod replay-demo [MAP] --input launches.json... [--json out.json] [--top N] [--solid spec]
[--dump-dir dir]` re-simulates each launch (`simulate_exact_raw`, the same collider/attributes as
`throw`) and aligns the sim trace with the game's own per-tick positions by tick number (not a
search - the launch instant is fit to land exactly one tick before the game's own first sample,
and the exact integrator's own tick length is the demo's own tick length, so they line up
directly; ticks after the sim's own trace ends, once the throw has settled, are compared against
the sim's rest point instead). Reports, per throw: the rest error, the first tick the deviation
exceeds 1u/5u, the sim's own contact at/just before that tick (point, normal, collision group, and
- for `EntitySolid`/`EntityBreakable` - the merged entity's classname/targetname/model, via
`CollisionMesh::tri_object`), and whether the game's own recorded positions show a velocity kink
there too (±2 ticks of the sim's own contact tick, tick-gap aware so it also works on a
2-tick-per-sample GOTV recording). Summary: counts within 1u/3u/8u (graded throws only) and a
ranked list of divergence surfaces (group + map + a 64u location cluster) by how many throws'
first significant divergence traces back to it.

`--input` accepts several `.launches.json` files and/or directories of them; each carries its own
map, so throws are grouped and reported per map (mesh/collider loaded once per map) and in total.

`--solid <spec>` overrides the grenade collision mask for A/B experiments, same syntax as
`export-obj --filter` (`grenade` default, `attrs:Name1,Name2`) plus `grenade-minus:Name1,...` (the
grenade mask with the named attribute groups made non-solid) - e.g.
`--solid grenade-minus:EntityPhysicsClip` to test "does `func_clip_vphysics` actually block
grenades?" without touching any code.

`--dump-dir <dir>` writes `<demo>_<entity>_<first_tick>.json` for every throw with a rest error
over 3u or any >1u tick (game samples, the full sim trace tick/pos/vel, every sim contact, and
every tick the game's own recorded positions show a kink) - for digging into one outlier.

`crates/calib/src/demo_replay.rs` has the alignment/kink-detection/exclusion logic, unit-tested on
synthetic data (no simulator or real demo involved).

## Known-good runs

- `okno2.dem`: 6 recorded entities, 1 duplicate of an earlier one (see above) - 5 real throws,
  matching ground truth `A`/`B`/`C`/`D`/`E` (`crates/solver/tests/fixtures/
  ground_truth_de_mirage.json`). `replay-demo` grades all 5 within 0.2u, zero ticks over 1u
  deviation across the whole flight.
- `okno1.dem`: a non-GOTV local demo, no `CSmokeGrenadeProjectile` at all - a clear message, no
  crash.
- `big1.dem`: 26 recorded entities, 22 with a detonation event, 21 confident graded throws (1
  excluded as low-confidence) - 18/21 within 1u, 21/21 within 3u except one real divergence on an
  `EntityPhysicsClip` surface (22.5u; the game did NOT bounce there either - a genuine collision-set
  gap, separately addressed on branch S6w by making that group non-solid for grenades; reproduce it
  here with `--solid grenade-minus:EntityPhysicsClip`, no code changes needed).

## Pro demos

HLTV/FACEIT pro and tournament demos give hundreds of real smokes per map, but differ from a
practice demo in ways this tool must handle:

1. Download the demo from HLTV manually in a browser (the site blocks automated downloads) and
   unpack the match archive with 7-Zip - one `.dem` per map.
2. Recycled entity ids: the same `grenade_entity_id` gets reused by throws several rounds apart
   (real FACEIT example: 102 raw groups for 108 detonations on one map). `extract_launches.py`
   splits each raw entity id's own samples into separate throws at any tick gap or a >64u position
   jump before fitting/grading/duplicate-checking anything - this is automatic, nothing to pass.
3. Run extraction on the unpacked folder: `...\python.exe tools\demo\extract_launches.py
   path\to\unpacked_folder -o out_dir` (recurses for `*.dem`, one `<map>.launches.json` per demo).
4. Run `cs2mod replay-demo --input out_dir --json report.json` - no map name needed, each file
   carries its own.
5. Read the report: per-map and total counts (graded, within 1u/3u/8u) plus counts EXCLUDED from
   grading and listed separately - "no detonation" (round/demo ended mid-flight), "low confidence"
   (a bad launch fit), "player contact" (the throw came within 6u of an alive player - CS2 grenades
   collide with players, so this is not a physics/collision-set divergence), "early detonation
   (fire)" (the smoke flew into a burning molotov/incendiary and detonated while still moving - see
   `early_detonation_fire`/`nearest_fire_distance` per throw), and "glass state unknown" (the
   divergence's sim contact is breakable glass, which may already have been broken earlier in the
   round - dynamic state this tool cannot know from the demo alone). The ranked divergence surfaces
   list is the best lead for a real collision-set gap; `--dump-dir` gives the full trace for any one
   outlier.
6. The header's `patch_version` is checked against the installed game's own `steam.inf`
   `PatchVersion` - a mismatch (old demo, updated map) is only a warning, since this tool cannot fix
   stale geometry, but it explains a cluster of divergences on one surface.
7. `--solid grenade-minus:EntityPhysicsClip` A/B switch: re-run the same report with
   `func_clip_vphysics` made non-solid for grenades to see how many divergences that alone
   explains, without any code changes (branch S6w is making this change permanently elsewhere).
