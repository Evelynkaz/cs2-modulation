//! Calibration of throw constants from measured throws (`throws.json`,
//! compatible with cs2-smoke-solver) and offline replay of recorded throw
//! corpora.

pub mod calibrate;
pub mod corpus;
pub mod launch;
pub mod replay;
pub mod throws_json;

pub use calibrate::{CalibrateError, CalibrationResult, SampleResidual, calibrate};
pub use corpus::{CorpusError, CorpusFile, CorpusResult, CorpusThrow, load_corpus};
pub use launch::{ErrorStats, LaunchReport, launch_check, parse_corpus_type};
pub use replay::{Metrics, ReplayReport, WorstEntry, replay};
pub use throws_json::{MeasuredThrow, ThrowsJsonError, load_throws_json, parse_getpos};
