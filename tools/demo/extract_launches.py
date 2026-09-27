#!/usr/bin/env python3
"""Extracts smoke-projectile launch states + ground-truth rests from CS2 GOTV demos, including
pro/tournament HLTV recordings (`s6v_c_pro_demos.md`, Part C of `s6v_demo_replay_and_rim_rest.md`).

Setup (Python 3.11, isolated venv - never installed globally):
    py -3.11 -m venv D:\\porject\\modulator-work\\scratch\\demo_venv
    D:\\porject\\modulator-work\\scratch\\demo_venv\\Scripts\\pip install demoparser2 pandas numpy

Usage (single demo, same as before):
    D:\\porject\\modulator-work\\scratch\\demo_venv\\Scripts\\python.exe extract_launches.py \
        <demo.dem> [-o launches.json]

Usage (several demos and/or a directory of demos - HLTV match archives unpack to one .dem per
map): `-o` becomes an output DIRECTORY, one `<demo stem>.launches.json` per demo, progress on
stderr:
    ...\\python.exe extract_launches.py <dir-of-dems-or-.dem-files>... -o out_dir

For every `CSmokeGrenadeProjectile` entity (`DemoParser.parse_grenades()`), fits the first
free-flight ticks (before any bounce/rest) as a simple ballistic model - x/y linear, z
quadratic with gravity `g` a free parameter - by growing a least-squares window one tick at a
time until the fit residual exceeds a tolerance (a bounce or the projectile coming to rest both
show up as a sudden residual jump, so this needs no explicit collision detection). The launch
instant is taken to be exactly one tick before the first recorded sample
(`s6v_demo_replay_and_rim_rest.md`'s own evidence: fitted g came out 320.00, velocity matched
`derive_initial` to 0.01 u/s, position to 0.2u, on that assumption) - `launch_confident` records
whether this fit is trustworthy enough to extrapolate that one tick backward at all (a short or
noisy fit window is instead reported alongside `sample0_pos`/`sample0_vel`: the fit's own value AT
the first sample, no backward extrapolation).

The rest is read from the `smokegrenade_detonate` event matched by entity id AND tick range (a
recycled entity id, see below, can otherwise match the wrong throw's detonation) when one exists
(the authoritative game position); projectiles that never detonate in the demo (round/demo ended
mid-flight, or interrupted) fall back to the entity's own last tracked position instead of being
dropped, flagged via `rest_source: "last_position"` plus a note - still useful for comparing the
simulated flight up to that point, just not for a rest-error grade.

Pro/tournament demos (`s6v_c_pro_demos.md`) add, on top of the above:
- **Recycled entity ids** (real, measured on a FACEIT pro demo: the same `grenade_entity_id`
  reused by throws several rounds apart, mirage 102 raw groups for 108 detonations): each raw
  entity id's own recorded samples are split into separate throws at any tick gap or a position
  jump (`split_segments`, ported from the scratch prototype `scratch/pro/split_pro.py`) before
  anything else - fitting, duplicate-checking and detonation-matching all then run per SEGMENT,
  not per raw entity id.
- **Thrower**: `thrower_steamid`/`thrower_name` per throw, from `parse_grenades()`'s own
  `steamid`/`name` columns.
- **Player proximity**: `player_near` (ticks where the grenade centre came within
  `PLAYER_NEAR_DIST` of an alive player's hull AABB, ignoring the thrower during the first
  `PLAYER_THROWER_GRACE_TICKS` after launch) and `player_contact_possible` - a throw that touched
  a player must not be graded as a physics/collision-set divergence.
- **Early detonation in fire** (measured, the main pro-demo divergence source): a smoke that
  flies into a burning molotov/incendiary detonates at once while still moving. `early_detonation_
  fire`/`nearest_fire_distance` per throw, from `inferno_startburn`/`inferno_expire`.
- **Map version**: the header's `patch_version` (e.g. "14185") is the game's steam.inf
  `PatchVersion` without dots; recorded (with `network_protocol`/`demo_version_name`/
  `server_name`/`demo_file_stamp`/`tick_interval`, whichever the parser provides) so a cluster of
  divergences on an old demo is explainable even though this tool cannot fix stale geometry.
"""

import argparse
import json
import sys
from dataclasses import dataclass
from pathlib import Path

import numpy as np
import pandas as pd
from demoparser2 import DemoParser

TICK_DT = 1.0 / 64.0  # `sim::throw::TIME_STEP`.
MIN_FIT_TICKS = 5
# Ballistic flight can run ~260-280 ticks for a lob throw (`PROGRESS.md`); cap generously above
# that so the growing fit is never truncated before a real bounce/rest ends it on its own.
MAX_FIT_TICKS = 512
# Growing-window stop rule: keep extending while both hold; the first tick that would break
# either (a real bounce, or the projectile coming to rest) stops the grow instead of entering
# the window. `s6v_fix1.md`: clean flight noise is rms 0.016u, max <=0.026u (1/32u quantization) -
# these limits used to be ~50x that, wide enough to swallow post-bounce samples into the fit.
RESID_RMS_LIMIT = 0.05
RESID_MAX_LIMIT = 0.1
# `launch_confident` gate: below any of these, backward-extrapolating one tick is not trusted.
CONFIDENT_MIN_TICKS = 8
CONFIDENT_MAX_RESID = 0.5
# `sim::throw::BASE_GRAVITY * ThrowConstants::default().gravity_scale` (800 * 0.4): a short fit
# window (real data: a throw aimed at a narrow rim/ledge close by, only ~10-20 free-flight ticks
# before the first bounce) can pass the residual gate above yet still fit a visibly wrong `g` -
# a glancing first bounce biases the whole window smoothly rather than as one sharp per-tick
# spike, so `RESID_MAX_LIMIT` alone does not always catch it. Known physics, so checked directly.
EXPECTED_G = 320.0
CONFIDENT_G_TOLERANCE = 10.0
# A recycled-entity-slot demo/parser artifact seen on real data (`okno2.dem` entity 329: 45
# ticks byte-for-byte identical to entity 374's own first 45 ticks, 1056 ticks later - not a
# second throw, the same recorded positions replayed under a reused entity id): a later entity
# whose raw position sequence matches an earlier entity's own prefix this closely is reported as
# a duplicate and skipped, UNLESS it has a `smokegrenade_detonate` event of its own in its own
# tick range (`s6v_fix1.md`: entity 319 on `okno2.dem` has its own detonation at tick 7361 - it is
# a real throw E that merely happens to fly the same first few ticks as an earlier one, not the
# demo/parser artifact).
DUPLICATE_POS_TOLERANCE = 0.02

# s6v Part C (`s6v_c_pro_demos.md`): a pro/tournament demo reuses the same `grenade_entity_id`
# across rounds (real FACEIT example: mirage, 102 raw groups for 108 detonations, gaps of
# 11618-239124 ticks inside one raw "projectile"). Split each raw entity id's own samples into
# separate throws at any tick gap greater than the demo's own MODAL tick delta (`modal_tick_delta`,
# `s6v_fix2.md`: 1 for a normal 64-tick GOTV recording, 2 for a tournament GOTV recorded at
# `tv_snapshotrate 32` - a fixed threshold of 1 would split every single throw of such a demo into
# one throw per sample) or a position jump > `SEGMENT_JUMP_DIST` - each piece is then
# fit/graded/duplicate-checked exactly like a single-throw entity used to be.
SEGMENT_GAP_TICKS = 1  # fallback when a demo has no projectile samples to compute a mode from
SEGMENT_JUMP_DIST = 64.0

# Player proximity (`s6v_c_pro_demos.md`): a throw that touches a player must not be graded as a
# physics/collision-set divergence.
PLAYER_NEAR_DIST = 6.0
PLAYER_THROWER_GRACE_TICKS = 12
# Grenade/player hull footprint used for the proximity check: x/y +-16, z feet..feet+72 (crouch
# height is unknown from `parse_ticks` alone, so the taller stand height is used - conservative:
# it can only make a throw MORE likely to be excluded as "touched a player", never less).
PLAYER_HULL_XY = 16.0
PLAYER_HULL_Z = 72.0

# EARLY DETONATION IN FIRE (`s6v_c_pro_demos.md` addendum, measured - the main pro-demo
# divergence source): a smoke that flies into a burning molotov/incendiary detonates at once
# while still moving (speed 400-990 u/s in the last real samples).
FIRE_SEARCH_WINDOW_TICKS = int(8 * 64)  # 8s before the detonate tick
FIRE_BURN_TICKS = int(7 * 64)  # a fire lasts ~7s after `inferno_startburn` if no matching `inferno_expire`
FIRE_NEAR_DIST = 250.0
EARLY_DETONATION_SPEED = 30.0  # u/s, max over the last up-to-4 sample pairs at/before detonate_tick - 2 (see early_detonation_fire)


@dataclass
class FitResult:
    fit_ticks: int
    resid_rms: float
    resid_max: float
    g_fit: float
    sample0_pos: list
    sample0_vel: list
    launch_pos: list
    launch_vel: list
    confident: bool


def fit_free_flight(ticks_rel: np.ndarray, pos: np.ndarray) -> FitResult:
    """Grows a least-squares ballistic fit (x,y linear; z quadratic, free `g`) over `pos[:L]`
    one tick at a time, keeping the largest `L` whose fit still satisfies the residual limits
    (or, if even the smallest window fails them, that smallest window anyway - a bad first
    window is still reported, just flagged `confident: False`, rather than reporting nothing).
    `ticks_rel` is 0-based tick offsets from the first sample (so `ticks_rel[i] * TICK_DT` is
    the fit's own time axis, sample 0 at t=0)."""
    n = len(pos)
    max_l = min(n, MAX_FIT_TICKS)
    best = None
    for length in range(MIN_FIT_TICKS, max_l + 1):
        t = ticks_rel[:length] * TICK_DT
        window = pos[:length]
        ax = np.vstack([np.ones(length), t]).T
        cx, *_ = np.linalg.lstsq(ax, window[:, 0], rcond=None)
        cy, *_ = np.linalg.lstsq(ax, window[:, 1], rcond=None)
        az = np.vstack([np.ones(length), t, t * t]).T
        cz, *_ = np.linalg.lstsq(az, window[:, 2], rcond=None)
        fit_x = ax @ cx
        fit_y = ax @ cy
        fit_z = az @ cz
        d = np.sqrt(
            (fit_x - window[:, 0]) ** 2
            + (fit_y - window[:, 1]) ** 2
            + (fit_z - window[:, 2]) ** 2
        )
        rms = float(np.sqrt(np.mean(d**2)))
        mx = float(d.max())
        ok = rms <= RESID_RMS_LIMIT and mx <= RESID_MAX_LIMIT
        if best is None:
            best = (length, cx, cy, cz, rms, mx)
        if ok:
            best = (length, cx, cy, cz, rms, mx)
        else:
            break

    length, cx, cy, cz, rms, mx = best
    g_fit = -2.0 * cz[2]
    sample0_pos = [float(cx[0]), float(cy[0]), float(cz[0])]
    sample0_vel = [float(cx[1]), float(cy[1]), float(cz[1])]
    dt = TICK_DT
    launch_pos = [
        float(cx[0] - cx[1] * dt),
        float(cy[0] - cy[1] * dt),
        float(cz[0] - cz[1] * dt + cz[2] * dt * dt),
    ]
    launch_vel = [
        float(cx[1]),
        float(cy[1]),
        float(cz[1] - 2.0 * cz[2] * dt),
    ]
    confident = bool(
        length >= CONFIDENT_MIN_TICKS
        and rms <= CONFIDENT_MAX_RESID
        and abs(g_fit - EXPECTED_G) <= CONFIDENT_G_TOLERANCE
    )
    return FitResult(
        fit_ticks=length,
        resid_rms=rms,
        resid_max=mx,
        g_fit=float(g_fit),
        sample0_pos=sample0_pos,
        sample0_vel=sample0_vel,
        launch_pos=launch_pos,
        launch_vel=launch_vel,
        confident=confident,
    )


def modal_tick_delta(all_ticks: list) -> int:
    """The demo's own modal tick delta between consecutive samples of the same raw entity id, over
    every projectile's own samples (`s6v_fix2.md`, DECISION: support 2-tick tournament GOTV
    recordings): 1 for a normal 64-tick recording, 2 for `tv_snapshotrate 32` - used as
    `split_segments`' own gap threshold so an ordinary 2-tick-spaced recording is not spuriously
    split into one throw per sample. Falls back to `SEGMENT_GAP_TICKS` when there is nothing to
    compute a mode from (fewer than 2 samples for every entity id)."""
    deltas = [d for ticks in all_ticks for d in np.diff(np.sort(ticks)).tolist() if d > 0]
    if not deltas:
        return SEGMENT_GAP_TICKS
    values, counts = np.unique(deltas, return_counts=True)
    return int(values[np.argmax(counts)])


def split_segments(
    ticks: np.ndarray, pos: np.ndarray, *extra: np.ndarray, gap=SEGMENT_GAP_TICKS, jump=SEGMENT_JUMP_DIST
):
    """Splits one raw entity's own recorded `(ticks, pos, *extra)` into separate throws at a tick
    gap greater than `gap` or a position jump greater than `jump` units (`s6v_c_pro_demos.md`'s
    recycled-entity-id finding; ported from the scratch prototype `scratch/pro/split_pro.py`).
    `extra` is any number of additional per-sample arrays (e.g. per-sample thrower/name) split the
    same way, so the first entry of each is the segment's own first sample (`s6v_fix2.md`: thrower/
    name must come from the segment, not from the whole raw entity id's first sample).
    Returns a list of `(ticks_slice, pos_slice, *extra_slices)`, in order."""
    n = len(ticks)
    segments = []
    start = 0
    for i in range(1, n + 1):
        cut = i == n
        if not cut:
            cut = (ticks[i] - ticks[i - 1] > gap) or (
                np.linalg.norm(pos[i] - pos[i - 1]) > jump
            )
        if cut:
            segments.append((ticks[start:i], pos[start:i], *(e[start:i] for e in extra)))
            start = i
    return segments


def hull_distances(point: np.ndarray, feet_positions: np.ndarray) -> np.ndarray:
    """Vectorised distance from `point` to every player's AABB hull (`feet_positions`: Nx3, one
    row per alive player): x/y +-`PLAYER_HULL_XY`, z `feet .. feet + PLAYER_HULL_Z`."""
    lo = feet_positions + np.array([-PLAYER_HULL_XY, -PLAYER_HULL_XY, 0.0])
    hi = feet_positions + np.array([PLAYER_HULL_XY, PLAYER_HULL_XY, PLAYER_HULL_Z])
    d = np.maximum(np.maximum(lo - point, 0.0), point - hi)
    return np.sqrt((d * d).sum(axis=1))


def header_info(header: dict) -> dict:
    """The fixed subset of `DemoParser.parse_header()` `s6v_c_pro_demos.md` wants recorded and
    shown in the report - a map-version mismatch (old demo, updated map) explains a cluster of
    divergences the replay tool itself cannot fix. Any field the parser did not provide is
    `None`, not a `KeyError`."""
    return {
        "network_protocol": header.get("network_protocol"),
        "demo_version_name": header.get("demo_version_name"),
        "server_name": header.get("server_name"),
        "demo_file_stamp": header.get("demo_file_stamp"),
        "patch_version": header.get("patch_version"),
        "tick_interval": header.get("tick_interval"),
    }


def build_fire_windows(startburn: pd.DataFrame, expire: pd.DataFrame):
    """One `(start_tick, expire_tick, x, y, z)` per `inferno_startburn` row, matched to the
    `inferno_expire` sharing its `entityid` (the nearest one at/after the start) when there is
    one, else `start_tick + FIRE_BURN_TICKS` (a fire cut short by the round/demo ending)."""
    if len(startburn) == 0:
        return []
    expire_by_entity = {}
    if len(expire):
        for eid, grp in expire.groupby("entityid"):
            expire_by_entity[int(eid)] = sorted(int(t) for t in grp["tick"].to_numpy())
    windows = []
    for _, row in startburn.iterrows():
        eid = int(row["entityid"])
        start_tick = int(row["tick"])
        candidates = [t for t in expire_by_entity.get(eid, []) if t >= start_tick]
        expire_tick = min(candidates) if candidates else start_tick + FIRE_BURN_TICKS
        windows.append(
            (start_tick, expire_tick, float(row["x"]), float(row["y"]), float(row["z"]))
        )
    return windows


def early_detonation_fire(ticks, pos, detonate_tick, game_rest, fire_windows):
    """`s6v_c_pro_demos.md` addendum, measured: a smoke that enters a burning molotov/incendiary
    detonates at once while still moving. The entity keeps getting recorded motionless for ~18s
    AFTER it detonates, and its position snaps back on the detonation tick itself, so `speed` is
    never taken from the last recorded samples - it is the MAX speed over the last up-to-4
    consecutive sample pairs at/before `detonate_tick - 2` (skipping the snap-back tick), each
    using the real tick delta (not an assumed 1-tick step - the same tick-gap-aware idiom the
    replay tool's own kink detector uses for tournament GOTV's 2-tick snapshot rate). `speed` is
    `0` when fewer than 2 such pre-detonation samples exist.
    Returns `(early_detonation_fire, nearest_fire_distance)` - `nearest_fire_distance` is `None`
    when no fire window was in the search range at all."""
    if detonate_tick is None or len(pos) < 2:
        return False, None
    valid_idx = np.nonzero(ticks <= detonate_tick - 2)[0]
    if len(valid_idx) < 2:
        speed = 0.0
    else:
        pair_idx = valid_idx[-5:]
        speeds = []
        for k in range(1, len(pair_idx)):
            i, j = pair_idx[k - 1], pair_idx[k]
            dt = (ticks[j] - ticks[i]) * TICK_DT
            if dt > 0:
                speeds.append(float(np.linalg.norm(pos[j] - pos[i]) / dt))
        speed = max(speeds) if speeds else 0.0
    nearest_dist = None
    burning_nearby = False
    for start_tick, expire_tick, fx, fy, fz in fire_windows:
        if start_tick > detonate_tick or start_tick < detonate_tick - FIRE_SEARCH_WINDOW_TICKS:
            continue
        d = float(np.linalg.norm(np.array(game_rest, dtype=np.float64) - np.array([fx, fy, fz])))
        if nearest_dist is None or d < nearest_dist:
            nearest_dist = d
        if d <= FIRE_NEAR_DIST and expire_tick >= detonate_tick:
            burning_nearby = True
    return bool(speed > EARLY_DETONATION_SPEED and burning_nearby), nearest_dist


def claim_detonation(claimed: dict, entity_id: int, detonate_tick: int, first_tick: int):
    """Tracks which segment first claims each `(entity_id, detonate_tick)` `smokegrenade_detonate`
    event. When a smoke detonates in fire (`early_detonation_fire`), its recorded position snaps
    back and it keeps getting recorded motionless afterwards - `split_segments`' position-jump cut
    then turns that motionless tail into a SECOND segment matching the SAME detonation (real cases:
    inferno entity 481's tail from tick 56243, entity 133's from tick ~110182 - both fit g~=0 and
    were counted as low-confidence throws). Returns a `skipped` reason string when `(entity_id,
    detonate_tick)` was already claimed by an earlier segment, else records this segment as the
    claim and returns `None`."""
    key = (entity_id, detonate_tick)
    claimed_at = claimed.get(key)
    if claimed_at is not None:
        return f"post-detonation tail of the segment at tick {claimed_at} (shares its smokegrenade_detonate)"
    claimed[key] = first_tick
    return None


def extract(demo_path: str) -> dict:
    parser = DemoParser(demo_path)
    header = parser.parse_header()
    map_name = header.get("map_name") or ""
    empty = {"map": map_name, "header": header_info(header), "projectiles": [], "skipped": []}

    grenades = parser.parse_grenades()
    if len(grenades) == 0:
        return empty

    proj = grenades[grenades["grenade_type"] == "CSmokeGrenadeProjectile"]
    # The held (not yet thrown) weapon is also a `CSmokeGrenadeProjectile` row, with NaN position.
    proj = proj.dropna(subset=["x", "y", "z"])
    if len(proj) == 0:
        return empty

    thrower_col = "steamid" if "steamid" in proj.columns else None
    name_col = "name" if "name" in proj.columns else None

    def parse_event_safe(name):
        try:
            return parser.parse_event(name)
        except Exception:
            return pd.DataFrame(columns=["entityid", "tick", "x", "y", "z"])

    detonate = parse_event_safe("smokegrenade_detonate")
    fire_windows = build_fire_windows(
        parse_event_safe("inferno_startburn"), parse_event_safe("inferno_expire")
    )

    # s6v Part C: split each raw entity id's own samples into separate throws before anything
    # else - a recycled id otherwise concatenates several rounds' throws into one "projectile".
    groups = [(eid, g.sort_values("tick")) for eid, g in proj.groupby("grenade_entity_id")]
    gap = modal_tick_delta([g["tick"].to_numpy() for _, g in groups])
    segments = []  # (entity_id, ticks, pos, thrower_steamid, thrower_name)
    for entity_id, g in groups:
        ticks = g["tick"].to_numpy()
        pos = g[["x", "y", "z"]].to_numpy(dtype=np.float64)
        throwers = g[thrower_col].to_numpy() if thrower_col else np.full(len(g), None, dtype=object)
        names = g[name_col].to_numpy() if name_col else np.full(len(g), None, dtype=object)
        for seg_ticks, seg_pos, seg_thrower, seg_name in split_segments(
            ticks, pos, throwers, names, gap=gap
        ):
            thrower = str(seg_thrower[0]) if thrower_col else None
            name = str(seg_name[0]) if name_col else None
            segments.append((int(entity_id), seg_ticks, seg_pos, thrower, name))
    segments.sort(key=lambda s: s[1][0])  # chronological, by first tick

    # Player proximity needs live positions only at the ticks a smoke projectile actually exists
    # at, to keep `parse_ticks` memory bounded on a full match demo.
    all_proj_ticks = sorted({int(t) for _, ticks, _, _, _ in segments for t in ticks})
    if all_proj_ticks:
        player_df = parser.parse_ticks(
            ["X", "Y", "Z", "steamid", "is_alive"], ticks=all_proj_ticks
        )
        player_df = player_df[player_df["is_alive"].astype(bool)]
        players_by_tick = {int(t): g for t, g in player_df.groupby("tick")}
    else:
        players_by_tick = {}

    projectiles = []
    skipped = []
    accepted_raw = []  # [(entity_id, ticks, pos)] of every non-duplicate segment seen so far
    claimed_detonations = {}  # (entity_id, detonate_tick) -> first_tick of the claiming segment
    for entity_id, ticks, pos, thrower, thrower_name in segments:
        n = len(ticks)

        duplicate_of = None
        for prev_id, _prev_ticks, prev_pos in accepted_raw:
            m = min(n, len(prev_pos))
            if m >= MIN_FIT_TICKS and np.allclose(
                pos[:m], prev_pos[:m], atol=DUPLICATE_POS_TOLERANCE
            ):
                duplicate_of = prev_id
                break
        if duplicate_of is not None:
            own_det = (
                detonate[
                    (detonate["entityid"] == entity_id)
                    & (detonate["tick"] >= ticks[0])
                    & (detonate["tick"] <= ticks[-1] + 2)
                ]
                if len(detonate)
                else detonate
            )
            if len(own_det) == 0:
                skipped.append(
                    {
                        "entity_id": entity_id,
                        "reason": (
                            f"duplicate of entity {duplicate_of}: {n} recorded position(s) "
                            "byte-for-byte identical to that entity's own trajectory, and no "
                            "smokegrenade_detonate event of its own (a recycled-entity-slot "
                            "demo/parser artifact, not a real second throw)"
                        ),
                    }
                )
                continue
            # else: has its own detonation - a real throw that merely starts the same as an
            # earlier one (`s6v_fix1.md`), not the demo/parser artifact - fall through and keep it.
        accepted_raw.append((entity_id, ticks, pos))

        if n < MIN_FIT_TICKS:
            skipped.append(
                {
                    "entity_id": entity_id,
                    "reason": f"only {n} sample(s), need at least {MIN_FIT_TICKS}",
                }
            )
            continue

        gaps = np.diff(ticks)
        missing_ticks = int(np.sum(gaps[gaps > 1] - 1)) if len(gaps) else 0

        ticks_rel = (ticks - ticks[0]).astype(np.float64)
        fit = fit_free_flight(ticks_rel, pos)

        det_rows = (
            detonate[
                (detonate["entityid"] == entity_id)
                & (detonate["tick"] >= ticks[0])
                & (detonate["tick"] <= ticks[-1] + 2)
            ]
            if len(detonate)
            else detonate
        )
        notes = []
        if len(det_rows) > 0:
            det_row = det_rows.iloc[0]
            detonate_tick = int(det_row["tick"])
            tail_reason = claim_detonation(claimed_detonations, entity_id, detonate_tick, int(ticks[0]))
            if tail_reason is not None:
                skipped.append({"entity_id": entity_id, "reason": tail_reason})
                continue
            game_rest = [float(det_row["x"]), float(det_row["y"]), float(det_row["z"])]
            rest_source = "detonate_event"
        else:
            game_rest = [float(pos[-1, 0]), float(pos[-1, 1]), float(pos[-1, 2])]
            rest_source = "last_position"
            detonate_tick = None
            notes.append("no smokegrenade_detonate event matched this entity id/tick range")

        if not fit.confident:
            notes.append(
                f"fit not confident (fit_ticks={fit.fit_ticks}, resid_rms={fit.resid_rms:.3f}u)"
            )
        if missing_ticks:
            notes.append(f"{missing_ticks} missing tick(s) inside the recorded range")

        near = []
        for t, p in zip(ticks, pos):
            df = players_by_tick.get(int(t))
            if df is None or len(df) == 0:
                continue
            feet = df[["X", "Y", "Z"]].to_numpy(dtype=np.float64)
            dist = hull_distances(p, feet)
            sids = df["steamid"].to_numpy()
            for d, sid in zip(dist, sids):
                is_thr = thrower is not None and str(sid) == thrower
                if d < PLAYER_NEAR_DIST and not (
                    is_thr and t - ticks[0] < PLAYER_THROWER_GRACE_TICKS
                ):
                    near.append(
                        {
                            "tick": int(t),
                            "distance": float(d),
                            "steamid": str(sid),
                            "is_thrower": bool(is_thr),
                        }
                    )

        fire_flag, fire_dist = early_detonation_fire(
            ticks, pos, detonate_tick, game_rest, fire_windows
        )

        projectiles.append(
            {
                "entity_id": int(entity_id),
                "first_tick": int(ticks[0]),
                "last_tick": int(ticks[-1]),
                "num_samples": n,
                "missing_ticks": missing_ticks,
                "fit_ticks": fit.fit_ticks,
                "fit_residual_rms": fit.resid_rms,
                "fit_residual_max": fit.resid_max,
                "fit_g": fit.g_fit,
                "launch_confident": fit.confident,
                "launch_tick": int(ticks[0]) - 1,
                "launch_pos": fit.launch_pos,
                "launch_vel": fit.launch_vel,
                "sample0_pos": fit.sample0_pos,
                "sample0_vel": fit.sample0_vel,
                "ticks": [
                    {"tick": int(t), "pos": [float(p[0]), float(p[1]), float(p[2])]}
                    for t, p in zip(ticks, pos)
                ],
                "game_rest": game_rest,
                "rest_source": rest_source,
                "detonate_tick": detonate_tick,
                "thrower_steamid": thrower,
                "thrower_name": thrower_name,
                "player_near": near,
                "player_contact_possible": bool(near),
                "early_detonation_fire": fire_flag,
                "nearest_fire_distance": fire_dist,
                "notes": notes,
            }
        )

    projectiles.sort(key=lambda p: p["first_tick"])
    return {"map": map_name, "header": header_info(header), "projectiles": projectiles, "skipped": skipped}


def print_summary(demo_path, result: dict) -> None:
    """Progress/diagnostics on stderr - shared between the single- and multi-demo paths."""
    if not result["projectiles"]:
        print(
            f"no smoke grenade projectiles found in {demo_path} "
            f"(map={result['map'] or 'unknown'!r}) - not a GOTV recording of a smoke throw?",
            file=sys.stderr,
        )
        return
    print(
        f"{demo_path}: map={result['map'] or 'unknown'!r}, "
        f"{len(result['projectiles'])} projectile(s), {len(result['skipped'])} skipped",
        file=sys.stderr,
    )
    for p in result["projectiles"]:
        conf = "ok" if p["launch_confident"] else "LOW-CONFIDENCE"
        near = f"  near player(s): {len(p['player_near'])}" if p["player_near"] else ""
        fire = "  EARLY-DETONATION-IN-FIRE" if p.get("early_detonation_fire") else ""
        print(
            f"  entity {p['entity_id']} @ tick {p['first_tick']}: {p['num_samples']} samples, "
            f"fit {p['fit_ticks']} ticks (resid {p['fit_residual_rms']:.3f}u/"
            f"{p['fit_residual_max']:.3f}u, g={p['fit_g']:.2f}) [{conf}], "
            f"rest_source={p['rest_source']}{near}{fire}",
            file=sys.stderr,
        )
    for s in result["skipped"]:
        print(f"  skipped entity {s['entity_id']}: {s['reason']}", file=sys.stderr)


def discover_demos(inputs: list) -> list:
    """Each input is a `.dem` file or a directory (searched recursively for `*.dem` - an HLTV
    match archive unpacks to one `.dem` per map, possibly under subfolders)."""
    demos = []
    for raw in inputs:
        p = Path(raw)
        if p.is_dir():
            demos.extend(sorted(p.rglob("*.dem")))
        else:
            demos.append(p)
    return demos


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument(
        "demo",
        nargs="+",
        help="path(s) to a .dem file and/or a directory (searched recursively for *.dem)",
    )
    ap.add_argument(
        "-o",
        "--output",
        help="write JSON here (single demo); an output DIRECTORY, one <demo stem>.launches.json "
        "per demo, when more than one demo is being processed (default: stdout)",
    )
    args = ap.parse_args()

    demos = discover_demos(args.demo)
    if not demos:
        print("no .dem files found in the given input(s)", file=sys.stderr)
        return 1

    multi = len(args.demo) > 1 or any(Path(a).is_dir() for a in args.demo)
    if multi and args.output:
        Path(args.output).mkdir(parents=True, exist_ok=True)

    for i, demo_path in enumerate(demos):
        if multi:
            print(f"[{i + 1}/{len(demos)}] {demo_path}", file=sys.stderr)
        result = extract(str(demo_path))
        print_summary(demo_path, result)
        text = json.dumps(result, indent=2)
        if args.output:
            out_path = (
                Path(args.output) / f"{demo_path.stem}.launches.json"
                if multi
                else Path(args.output)
            )
            out_path.write_text(text)
            print(f"wrote {out_path}", file=sys.stderr)
        elif multi:
            print(f"--- {demo_path} ---")
            print(text)
        else:
            print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
