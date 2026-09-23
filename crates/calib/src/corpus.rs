//! Typed loader for the reference's validation corpus JSON
//! (`cs2-smoke-solver/data/validation/<map>-<timestamp>.json`), tolerant of
//! fields older report files lack (`serverBuild`, `name`, `batch`, `RunDeg`,
//! `PerturbU`, `Scatter`, `GlassState`), strict on the fields every report
//! has always carried (`map`, `build`, `results[].Index/Type/Feet/Yaw/Pitch/
//! Strength`).

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CorpusError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Json {
        path: std::path::PathBuf,
        source: serde_json::Error,
    },
}

/// One `results[]` entry, field names matching the reference JSON exactly
/// (`ReplayCommand.cs:99-109`, `ValidateCommand.cs` result serialization).
/// One report in the corpus (`de_dust2-20260709-200339.json`, build 2000839)
/// predates the PascalCase convention and uses camelCase field names
/// instead; every field accepts both spellings via `alias`.
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusResult {
    #[serde(rename = "Index", alias = "index")]
    pub index: i32,
    #[serde(rename = "Type", alias = "type")]
    pub throw_type: String,
    #[serde(rename = "Strength", alias = "strength")]
    pub strength: f32,
    #[serde(rename = "Stability", alias = "stability", default)]
    pub stability: f32,
    #[serde(rename = "Feet", alias = "feet")]
    pub feet: [f32; 3],
    #[serde(rename = "Yaw", alias = "yaw")]
    pub yaw: f32,
    #[serde(rename = "Pitch", alias = "pitch")]
    pub pitch: f32,
    /// Older reports (pre-run-jump-throw calibration) lack this; 0 is a
    /// no-op offset for every non-`RunJumpThrow` type.
    #[serde(rename = "RunDeg", alias = "runDeg", default)]
    pub run_deg: f32,
    #[serde(rename = "PerturbU", alias = "perturbU", default)]
    pub perturb_u: f32,
    #[serde(rename = "Scatter", alias = "scatter", default)]
    pub scatter: f32,
    #[serde(rename = "PredictedBounces", alias = "predictedBounces", default)]
    pub predicted_bounces: i32,
    #[serde(rename = "RealBounces", alias = "realBounces", default)]
    pub real_bounces: i32,
    #[serde(rename = "Pos", alias = "pos", default)]
    pub pos: Option<[f32; 3]>,
    #[serde(rename = "Vel", alias = "vel", default)]
    pub vel: Option<[f32; 3]>,
    #[serde(rename = "PredictedRest", alias = "predictedRest", default)]
    pub predicted_rest: Option<[f32; 3]>,
    #[serde(rename = "RealRest", alias = "realRest", default)]
    pub real_rest: Option<[f32; 3]>,
    #[serde(rename = "Detonated", alias = "detonated", default)]
    pub detonated: bool,
    #[serde(rename = "ErrPredicted", alias = "errPredicted", default)]
    pub err_predicted: f32,
    #[serde(rename = "ErrTarget", alias = "errTarget", default)]
    pub err_target: f32,
    #[serde(rename = "DivergenceTick", alias = "divergenceTick", default)]
    pub divergence_tick: i32,
    #[serde(rename = "DivergenceClass", alias = "divergenceClass", default)]
    pub divergence_class: Option<String>,
    #[serde(rename = "GlassState", alias = "glassState", default)]
    pub glass_state: Option<String>,
}

/// A whole `<map>-<timestamp>.json` report file.
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusFile {
    pub map: String,
    pub build: String,
    #[serde(default, rename = "serverBuild")]
    pub server_build: Option<String>,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub batch: Option<String>,
    #[serde(default)]
    pub target: Option<[f32; 3]>,
    #[serde(default)]
    pub tolerance: f32,
    pub results: Vec<CorpusResult>,
}

/// A `results[]` row flattened with the report file it came from, matching
/// the `(Report, Index, ...)` rows `ReplayCommand.cs:84-110` builds.
#[derive(Debug, Clone)]
pub struct CorpusThrow {
    /// The report file's stem (`<map>-<timestamp>`), used to attribute a
    /// worst-N entry to a run.
    pub report: String,
    pub build: String,
    pub result: CorpusResult,
}

/// Loads every `<map>-*.json` report under `dir` (skipping the `.md`
/// siblings the reference also writes there), sorted by filename to match
/// `Directory.EnumerateFiles(...).OrderBy(f => f)` (`ReplayCommand.cs:87`).
pub fn load_corpus(dir: &Path, map: &str) -> Result<Vec<CorpusThrow>, CorpusError> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|source| CorpusError::Io {
            path: dir.to_path_buf(),
            source,
        })?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&format!("{map}-")))
        })
        .collect();
    paths.sort();

    let mut rows = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path).map_err(|source| CorpusError::Io {
            path: path.clone(),
            source,
        })?;
        let file: CorpusFile = serde_json::from_str(&text).map_err(|source| CorpusError::Json {
            path: path.clone(),
            source,
        })?;
        let report = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&file.map)
            .to_string();
        for result in file.results {
            rows.push(CorpusThrow {
                report: report.clone(),
                build: file.build.clone(),
                result,
            });
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &str) {
        std::fs::write(dir.join(name), contents).unwrap();
    }

    #[test]
    fn loads_and_flattens_across_files_tolerating_missing_fields() {
        struct TempDir(std::path::PathBuf);
        impl std::ops::Deref for TempDir {
            type Target = Path;
            fn deref(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let dir =
            TempDir(std::env::temp_dir().join(format!("calib-corpus-test-{}", std::process::id())));
        std::fs::create_dir_all(&*dir).unwrap();
        write(
            &dir,
            "de_mirage-1.json",
            r#"{"map":"de_mirage","build":"2000872","results":[
                {"Index":0,"Type":"Stand","Strength":1.0,"Feet":[1,2,3],"Yaw":0,"Pitch":0,
                 "Pos":[1,2,3],"Vel":[4,5,6],"RealRest":[7,8,9],"Detonated":true,"ErrPredicted":1.0}
            ]}"#,
        );
        write(
            &dir,
            "de_mirage-2.json",
            r#"{"map":"de_mirage","build":"2000899","serverBuild":"2000899","name":"auto-1","batch":"b1","results":[
                {"Index":0,"Type":"JumpThrow","Strength":0.5,"Feet":[0,0,0],"Yaw":1,"Pitch":2,"RunDeg":5,"GlassState":"gone"}
            ]}"#,
        );
        write(&dir, "de_mirage-1.md", "not json, must be skipped");
        write(
            &dir,
            "de_dust2-1.json",
            r#"{"map":"de_dust2","build":"1","results":[]}"#,
        );

        let rows = load_corpus(&dir, "de_mirage").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].build, "2000872");
        assert_eq!(rows[0].result.pos, Some([1.0, 2.0, 3.0]));
        assert_eq!(rows[1].build, "2000899");
        assert_eq!(rows[1].result.run_deg, 5.0);
        assert_eq!(rows[1].result.glass_state.as_deref(), Some("gone"));
        assert_eq!(rows[1].result.pos, None);
    }
}
