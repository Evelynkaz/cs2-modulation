//! Parses the reference's `throws.json` calibration format
//! (`cs2-smoke-solver/data/throws.json`): `{"throw": "setpos x y z;setang p
//! y r", "landing": "...", "type": "stand"|"crouch"|"jump"|"crouchjump"|
//! "runjump", "strength": 1|0.5|0, "measure": "impact"}`.
//! `CalibrateCommand.cs:19-36`.

use geom::math::V3;
use serde::Deserialize;
use sim::{ThrowSpec, ThrowType};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ThrowsJsonError {
    #[error("failed to read {0}: {1}")]
    Io(std::path::PathBuf, std::io::Error),
    #[error("failed to parse {0}: {1}")]
    Json(std::path::PathBuf, serde_json::Error),
    #[error("cannot parse getpos line: {0:?}")]
    GetPos(String),
    #[error("unknown throw type {0:?}")]
    ThrowType(String),
}

#[derive(Debug, Clone, Deserialize)]
struct RawEntry {
    #[serde(rename = "throw")]
    throw: String,
    landing: String,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    strength: Option<f32>,
    #[serde(default)]
    measure: Option<String>,
}

/// A single measured sample: the launch spec and the measured landing
/// point/kind (rest vs. first-impact horizontal position).
#[derive(Debug, Clone, PartialEq)]
pub struct MeasuredThrow {
    pub spec: ThrowSpec,
    pub landing: V3,
    pub impact: bool,
}

/// Parses a `setpos x y z;setang p y r` line into `(pos, pitch, yaw)`,
/// matching `CliParsing.cs:ParseGetPos`'s regex
/// (`setpos\s+(-?[\d.]+)\s+(-?[\d.]+)\s+(-?[\d.]+)\s*;?\s*(?:setang\s+
/// (-?[\d.]+)\s+(-?[\d.]+))?`): `setpos` is found case-insensitively anywhere
/// in the line (so a console-log prefix like `] setpos ...` still parses),
/// its next 3 tokens are the position, and if a `setang` token appears
/// anywhere after that its next 2 tokens are pitch/yaw (roll, if given, is
/// ignored, like the reference); a missing `;`, or no `setang` at all, is
/// tolerated and yields `(0.0, 0.0)`.
pub fn parse_getpos(line: &str) -> Result<(V3, f32, f32), ThrowsJsonError> {
    let err = || ThrowsJsonError::GetPos(line.to_string());
    // `;` isn't whitespace, so a `setpos ...;setang ...` with no space
    // around the `;` would otherwise glue `"z;setang"` into one token.
    let normalized = line.replace(';', " ");
    let tokens: Vec<&str> = normalized.split_whitespace().collect();

    let take_floats = |start: usize, n: usize| -> Option<Vec<f32>> {
        tokens
            .get(start..start + n)?
            .iter()
            .map(|t| t.parse::<f32>().ok())
            .collect()
    };

    let pos_idx = tokens
        .iter()
        .position(|t| t.eq_ignore_ascii_case("setpos"))
        .ok_or_else(err)?;
    let pos_vals = take_floats(pos_idx + 1, 3).ok_or_else(err)?;
    let pos = V3::new(pos_vals[0], pos_vals[1], pos_vals[2]);

    let ang_idx = tokens[pos_idx + 4..]
        .iter()
        .position(|t| t.eq_ignore_ascii_case("setang"))
        .map(|i| pos_idx + 4 + i);
    let (pitch, yaw) = match ang_idx.and_then(|i| take_floats(i + 1, 2)) {
        Some(v) => (v[0], v[1]),
        None => (0.0, 0.0),
    };
    Ok((pos, pitch, yaw))
}

/// `CliParsing.cs:ParseThrowType`.
pub fn parse_throw_type(s: &str) -> Result<ThrowType, ThrowsJsonError> {
    match s.to_lowercase().as_str() {
        "stand" => Ok(ThrowType::Stand),
        "crouch" => Ok(ThrowType::Crouch),
        "jump" => Ok(ThrowType::JumpThrow),
        "crouchjump" => Ok(ThrowType::CrouchJumpThrow),
        "runjump" => Ok(ThrowType::RunJumpThrow),
        other => Err(ThrowsJsonError::ThrowType(other.to_string())),
    }
}

/// Loads a `throws.json` file. `landing_eye_height` is the height the
/// reference subtracts from the landing `getpos`'s eye Z to reach the feet
/// (`CalibrateCommand.cs:34`: `landingEye - Vector3(0,0,64)`, hard-coded to
/// `64`, not the exact `StandEyeHeight` of `64.06` -- an open question
/// (`docs/ARCHITECTURE.md` §8 item 1: is the recorded landing measured
/// standing, or is `64` just a round-number stand-in?); left configurable
/// here rather than baked in, defaulting to the reference's literal `64.0`.
pub fn load_throws_json(
    path: &std::path::Path,
    landing_eye_height: f32,
) -> Result<Vec<MeasuredThrow>, ThrowsJsonError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| ThrowsJsonError::Io(path.to_path_buf(), e))?;
    let entries: Vec<RawEntry> =
        serde_json::from_str(&text).map_err(|e| ThrowsJsonError::Json(path.to_path_buf(), e))?;

    entries
        .into_iter()
        .map(|entry| {
            let (eye, pitch, yaw) = parse_getpos(&entry.throw)?;
            let (landing_eye, _, _) = parse_getpos(&entry.landing)?;
            let throw_type = parse_throw_type(entry.r#type.as_deref().unwrap_or("stand"))?;
            let strength = entry.strength.unwrap_or(1.0);
            let impact = entry.measure.as_deref() == Some("impact");
            Ok(MeasuredThrow {
                spec: ThrowSpec {
                    eye,
                    yaw_deg: yaw,
                    pitch_deg: pitch,
                    throw_type,
                    strength,
                    run_yaw_offset_deg: 0.0,
                },
                landing: landing_eye - V3::new(0.0, 0.0, landing_eye_height),
                impact,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_getpos_with_and_without_setang() {
        let (pos, pitch, yaw) = parse_getpos("setpos 100 -200.5 30;setang -10 90 0").unwrap();
        assert_eq!(pos, V3::new(100.0, -200.5, 30.0));
        assert_eq!(pitch, -10.0);
        assert_eq!(yaw, 90.0);

        let (pos, pitch, yaw) = parse_getpos("setpos 1 2 3").unwrap();
        assert_eq!(pos, V3::new(1.0, 2.0, 3.0));
        assert_eq!(pitch, 0.0);
        assert_eq!(yaw, 0.0);
    }

    #[test]
    fn tolerates_console_prefix_missing_semicolon_and_far_setang() {
        // A pasted console line, no `;` at all.
        let (pos, pitch, yaw) = parse_getpos("] setpos 10 20 30 setang 5 15 0").unwrap();
        assert_eq!(pos, V3::new(10.0, 20.0, 30.0));
        assert_eq!(pitch, 5.0);
        assert_eq!(yaw, 15.0);

        // `setang` glued to the prior token by a bare `;`.
        let (pos, pitch, yaw) = parse_getpos("setpos 1 2 3;setang 4 5 0").unwrap();
        assert_eq!(pos, V3::new(1.0, 2.0, 3.0));
        assert_eq!(pitch, 4.0);
        assert_eq!(yaw, 5.0);

        // No `setang` anywhere: defaults to (0, 0), not an error.
        let (_, pitch, yaw) = parse_getpos("] setpos 1 2 3").unwrap();
        assert_eq!((pitch, yaw), (0.0, 0.0));
    }

    #[test]
    fn missing_setpos_is_an_error() {
        assert!(parse_getpos("setang 1 2 0").is_err());
    }

    #[test]
    fn loads_throws_json_fixture() {
        let dir = std::env::temp_dir().join(format!("calib-throws-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("throws.json");
        std::fs::write(
            &path,
            r#"[
                {"throw": "setpos 0 0 64.06;setang 0 0 0", "landing": "setpos 500 0 64;setang 0 0 0", "type": "stand", "strength": 1},
                {"throw": "setpos 0 0 64.06;setang -20 0 0", "landing": "setpos 300 0 64;setang 0 0 0", "type": "jump", "strength": 0.5, "measure": "impact"}
            ]"#,
        )
        .unwrap();

        let samples = load_throws_json(&path, 64.0).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].spec.throw_type, ThrowType::Stand);
        assert!(!samples[0].impact);
        assert_eq!(samples[1].spec.throw_type, ThrowType::JumpThrow);
        assert!(samples[1].impact);
        assert_eq!(samples[0].landing, V3::new(500.0, 0.0, 0.0));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
