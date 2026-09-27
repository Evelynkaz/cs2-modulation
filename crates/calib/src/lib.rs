//! Calibration of throw constants from measured throws (`throws.json`,
//! compatible with cs2-smoke-solver) and offline replay of recorded throw
//! corpora.

pub mod calibrate;
pub mod corpus;
pub mod demo_replay;
pub mod launch;
pub mod replay;
pub mod throws_json;

pub use calibrate::{CalibrateError, CalibrationResult, SampleResidual, calibrate};
pub use corpus::{CorpusError, CorpusFile, CorpusResult, CorpusThrow, load_corpus};
pub use demo_replay::{
    DemoHeader, DemoReplaySummary, DivergenceContact, EntityInfo, Exclusion, GradedThrow,
    KINK_ACCEL_TOLERANCE, LaunchesFile, PlayerNear, ProjectileLaunch, SkippedProjectile,
    ThrowReplay, TickSample, align_deviations, describe_triangle, exclusion_reason, first_over,
    game_kink_ticks, game_velocity_kink, player_contact_excludes, replay_throw, summarize,
};
pub use launch::{ErrorStats, LaunchReport, launch_check, parse_corpus_type};
pub use replay::{Metrics, ReplayReport, WorstEntry, replay};
pub use throws_json::{MeasuredThrow, ThrowsJsonError, load_throws_json, parse_getpos};
