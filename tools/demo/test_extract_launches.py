"""Unit tests for `extract_launches.py` (`s6v_fix2.md`): pure functions only, no `.dem`/parser
involved. Run with the venv python from the repo root:

    D:\\porject\\modulator-work\\scratch\\demo_venv\\Scripts\\python.exe -m unittest \
        tools/demo/test_extract_launches.py
"""

import sys
import unittest
from pathlib import Path

import numpy as np
import pandas as pd

sys.path.insert(0, str(Path(__file__).resolve().parent))
import extract_launches as el  # noqa: E402


class SplitSegmentsTests(unittest.TestCase):
    def test_splits_on_tick_gap_and_carries_per_segment_thrower_and_name(self):
        # Two throwers on one recycled entity id (`s6v_fix2.md` bug 1): the gap between tick 102
        # and tick 500 splits the raw group into two throws, each keeping its OWN thrower/name from
        # its own first sample, not the whole group's `iloc[0]`.
        ticks = np.array([100, 101, 102, 500, 501, 502])
        pos = np.zeros((6, 3))
        throwers = np.array(["1Sandman", "1Sandman", "1Sandman", "consti", "consti", "consti"])
        names = np.array(["Sandman", "Sandman", "Sandman", "consti", "consti", "consti"])

        segments = el.split_segments(ticks, pos, throwers, names)

        self.assertEqual(len(segments), 2)
        seg_ticks, seg_pos, seg_thrower, seg_name = segments[0]
        self.assertEqual(seg_thrower[0], "1Sandman")
        self.assertEqual(seg_name[0], "Sandman")
        seg_ticks, seg_pos, seg_thrower, seg_name = segments[1]
        self.assertEqual(seg_thrower[0], "consti")
        self.assertEqual(seg_name[0], "consti")

    def test_splits_on_position_jump_even_with_no_tick_gap(self):
        ticks = np.array([100, 101, 102])
        pos = np.array([[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1000.0, 0.0, 0.0]])
        segments = el.split_segments(ticks, pos)
        self.assertEqual(len(segments), 2)
        self.assertEqual(list(segments[0][0]), [100, 101])
        self.assertEqual(list(segments[1][0]), [102])

    def test_modal_gap_of_two_does_not_split_a_uniform_2_tick_recording(self):
        # tv_snapshotrate 32: every projectile sample is 2 ticks apart. The demo's own modal tick
        # delta is 2, so `gap=2` must NOT split this run (a fixed gap of 1 would split every pair).
        ticks = np.array([100, 102, 104, 106, 108])
        pos = np.zeros((5, 3))
        segments = el.split_segments(ticks, pos, gap=2)
        self.assertEqual(len(segments), 1)
        self.assertEqual(list(segments[0][0]), list(ticks))

    def test_modal_gap_of_two_still_splits_a_real_gap(self):
        ticks = np.array([100, 102, 104, 400, 402])
        pos = np.zeros((5, 3))
        segments = el.split_segments(ticks, pos, gap=2)
        self.assertEqual(len(segments), 2)
        self.assertEqual(list(segments[0][0]), [100, 102, 104])
        self.assertEqual(list(segments[1][0]), [400, 402])


class ModalTickDeltaTests(unittest.TestCase):
    def test_picks_the_most_common_delta_across_all_entities(self):
        all_ticks = [
            np.array([100, 102, 104, 106]),  # deltas: 2, 2, 2
            np.array([500, 502, 900]),  # deltas: 2, 398 - the 398 is a real split gap, not modal
        ]
        self.assertEqual(el.modal_tick_delta(all_ticks), 2)

    def test_falls_back_to_the_constant_when_there_is_nothing_to_measure(self):
        self.assertEqual(el.modal_tick_delta([np.array([100])]), el.SEGMENT_GAP_TICKS)
        self.assertEqual(el.modal_tick_delta([]), el.SEGMENT_GAP_TICKS)


class HullDistancesTests(unittest.TestCase):
    def test_point_inside_a_hull_is_zero_distance(self):
        feet = np.array([[0.0, 0.0, 0.0]])
        point = np.array([0.0, 0.0, 10.0])
        dist = el.hull_distances(point, feet)
        self.assertAlmostEqual(dist[0], 0.0)

    def test_point_outside_a_hull_matches_independent_aabb_clamp(self):
        feet = np.array([[0.0, 0.0, 0.0], [100.0, 100.0, 0.0]])
        point = np.array([0.0, 0.0, 10.0])
        dist = el.hull_distances(point, feet)

        def expected(feet_row):
            lo = feet_row + np.array([-el.PLAYER_HULL_XY, -el.PLAYER_HULL_XY, 0.0])
            hi = feet_row + np.array([el.PLAYER_HULL_XY, el.PLAYER_HULL_XY, el.PLAYER_HULL_Z])
            clamped = np.clip(point, lo, hi)
            return float(np.linalg.norm(point - clamped))

        self.assertAlmostEqual(dist[0], expected(feet[0]))
        self.assertAlmostEqual(dist[1], expected(feet[1]))


class BuildFireWindowsTests(unittest.TestCase):
    def test_empty_startburn_returns_no_windows(self):
        empty = pd.DataFrame(columns=["entityid", "tick", "x", "y", "z"])
        self.assertEqual(el.build_fire_windows(empty, empty), [])

    def test_matches_the_nearest_expire_at_or_after_start_and_ignores_earlier_ones(self):
        startburn = pd.DataFrame([{"entityid": 5, "tick": 1000, "x": 100.0, "y": 200.0, "z": 300.0}])
        expire = pd.DataFrame(
            [
                {"entityid": 5, "tick": 900},  # before start - must be ignored
                {"entityid": 5, "tick": 1500},
                {"entityid": 5, "tick": 1800},
            ]
        )
        windows = el.build_fire_windows(startburn, expire)
        self.assertEqual(windows, [(1000, 1500, 100.0, 200.0, 300.0)])

    def test_falls_back_to_fire_burn_ticks_when_no_expire_matches(self):
        startburn = pd.DataFrame([{"entityid": 6, "tick": 2000, "x": 0.0, "y": 0.0, "z": 0.0}])
        expire = pd.DataFrame(columns=["entityid", "tick"])
        windows = el.build_fire_windows(startburn, expire)
        self.assertEqual(windows, [(2000, 2000 + el.FIRE_BURN_TICKS, 0.0, 0.0, 0.0)])


class EarlyDetonationFireTests(unittest.TestCase):
    def test_fast_flight_into_a_nearby_burning_fire_flags_early_detonation(self):
        # Pre-detonation samples moving fast (640 u/s), then a post-detonation motionless tail
        # (the entity keeps getting recorded at the same rest position for a while) - the tail must
        # not be allowed to zero out the measured speed.
        pre_ticks = np.array([100, 101, 102, 103])
        pre_pos = np.array([[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [20.0, 0.0, 0.0], [30.0, 0.0, 0.0]])
        tail_ticks = np.array([106, 107, 108, 109, 110])
        tail_pos = np.tile(pre_pos[-1], (len(tail_ticks), 1))
        ticks = np.concatenate([pre_ticks, tail_ticks])
        pos = np.concatenate([pre_pos, tail_pos])
        detonate_tick = 105
        game_rest = [30.0, 0.0, 0.0]
        fire_windows = [(detonate_tick - 10, detonate_tick + 100, 30.0, 0.0, 50.0)]  # 50u away

        flag, dist = el.early_detonation_fire(ticks, pos, detonate_tick, game_rest, fire_windows)

        self.assertTrue(flag)
        self.assertAlmostEqual(dist, 50.0)

    def test_normal_slow_roll_does_not_fire_without_a_nearby_fire(self):
        # A grenade slowing to rest (well under the speed gate) with no fire window nearby at all -
        # must not be flagged, even though it is genuinely coming to rest right at `detonate_tick`.
        ticks = np.array([100, 101, 102, 103])
        pos = np.array([[0.0, 0.0, 0.0], [0.05, 0.0, 0.0], [0.09, 0.0, 0.0], [0.1, 0.0, 0.0]])
        detonate_tick = 105
        game_rest = [0.1, 0.0, 0.0]
        fire_windows = []

        flag, dist = el.early_detonation_fire(ticks, pos, detonate_tick, game_rest, fire_windows)

        self.assertFalse(flag)
        self.assertIsNone(dist)

    def test_fast_flight_far_from_any_fire_does_not_fire(self):
        pre_ticks = np.array([100, 101, 102, 103])
        pre_pos = np.array([[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [20.0, 0.0, 0.0], [30.0, 0.0, 0.0]])
        detonate_tick = 105
        game_rest = [30.0, 0.0, 0.0]
        fire_windows = [(detonate_tick - 10, detonate_tick + 100, 5000.0, 0.0, 0.0)]  # far away

        flag, dist = el.early_detonation_fire(pre_ticks, pre_pos, detonate_tick, game_rest, fire_windows)

        self.assertFalse(flag)
        self.assertIsNotNone(dist)  # still reports the nearest distance, just too far to flag


class ClaimDetonationTests(unittest.TestCase):
    def test_first_claim_returns_none_and_records_it(self):
        claimed = {}
        result = el.claim_detonation(claimed, entity_id=481, detonate_tick=56243, first_tick=56100)
        self.assertIsNone(result)
        self.assertEqual(claimed[(481, 56243)], 56100)

    def test_second_segment_sharing_the_same_detonation_is_flagged_as_a_tail(self):
        # Real case: a smoke that detonates in fire leaves a motionless recorded tail that
        # `split_segments` cuts into a second segment matching the SAME smokegrenade_detonate.
        claimed = {}
        el.claim_detonation(claimed, entity_id=481, detonate_tick=56243, first_tick=56100)
        result = el.claim_detonation(claimed, entity_id=481, detonate_tick=56243, first_tick=56260)
        self.assertEqual(
            result, "post-detonation tail of the segment at tick 56100 (shares its smokegrenade_detonate)"
        )

    def test_same_entity_different_detonation_is_not_a_tail(self):
        # A recycled entity id thrown again later, with its own separate detonation - must not be
        # mistaken for a tail of the earlier throw.
        claimed = {}
        el.claim_detonation(claimed, entity_id=481, detonate_tick=56243, first_tick=56100)
        result = el.claim_detonation(claimed, entity_id=481, detonate_tick=99999, first_tick=99900)
        self.assertIsNone(result)


if __name__ == "__main__":
    unittest.main()
