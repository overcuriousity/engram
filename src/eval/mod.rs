//! Retrieval evaluation: the on-disk format and the metrics.
//!
//! Ranking has several knobs — fusion, the per-source cap, recency weight,
//! reranking — and hand-testing cannot judge them, because the queries anyone
//! thinks to type reuse words they remember from the passage they are looking
//! for. Written-down pairs are how a knob change becomes a number that moved.
//!
//! The corpus this measures is not in the repository and must not be: it is
//! whatever documents the operator actually wants to search. What lives here is
//! the shape of the files and the arithmetic over ranks.

pub mod claims;
pub mod export;
pub mod metrics;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Where the corpus, the frozen artifacts and the pairs live. Outside the
/// repository by default; the in-repo fallback exists so an error message can
/// name a concrete path, and it is gitignored.
pub fn eval_dir() -> PathBuf {
    std::env::var("ENGRAM_EVAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("eval-data"))
}

/// One artifact as the segmenter produced it, frozen so a benchmark run costs no
/// completions and two runs rank exactly the same text.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrozenArtifact {
    pub id: String,
    /// The corpus this came from, by id. What the per-source cap groups by, so
    /// it has to survive the freeze and it has to be unique — two captures of
    /// one document share a title and are still two corpora.
    pub source: String,
    pub text: String,
    pub title: Option<String>,
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A query and the artifact that should answer it.
///
/// The query is meant to be phrased as a situation, in the words a reader
/// happens to have — not in the vocabulary of the artifact. A pair that shares the
/// artifact's terminology measures nothing: every retrieval system passes it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EvalPair {
    pub query: String,
    /// `FrozenArtifact::id` of the expected answer.
    pub expect: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// A question, its verdict, and the artifacts the operator said carried the
/// answer. `expect` is empty for `wrong` and `nothing_here`, and for a `right`
/// answer that was a synthesis with no single carrier — those still measure
/// abstention, not citation recall.
///
/// The first half of that is an invariant `export` enforces rather than one the
/// store upholds: marking a carrier does not overrule a verdict already given,
/// so a `wrong` answer can carry marks, and a carrier behind `wrong` is not a
/// statement that the artifact should have been cited.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EvalQuestion {
    pub question: String,
    /// `right` | `wrong` | `nothing_here`.
    pub verdict: String,
    #[serde(default)]
    pub expect: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
}

pub fn questions_path(dir: &Path) -> PathBuf {
    dir.join("questions.json")
}

pub fn save_questions(dir: &Path, questions: &[EvalQuestion]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = questions_path(dir);
    let json = serde_json::to_string_pretty(questions)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

pub fn load_questions(dir: &Path) -> Result<Vec<EvalQuestion>> {
    let path = questions_path(dir);
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn artifacts_path(dir: &Path) -> PathBuf {
    dir.join("artifacts.json")
}

pub fn pairs_path(dir: &Path) -> PathBuf {
    dir.join("pairs.json")
}

pub fn load_artifacts(dir: &Path) -> Result<Vec<FrozenArtifact>> {
    let path = artifacts_path(dir);
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

pub fn save_artifacts(dir: &Path, artifacts: &[FrozenArtifact]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = artifacts_path(dir);
    let json = serde_json::to_string_pretty(artifacts)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

pub fn save_pairs(dir: &Path, pairs: &[EvalPair]) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = pairs_path(dir);
    let json = serde_json::to_string_pretty(pairs)?;
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))
}

pub fn load_pairs(dir: &Path) -> Result<Vec<EvalPair>> {
    let path = pairs_path(dir);
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_survive_a_round_trip_through_the_frozen_file() {
        let dir = tempfile::tempdir().unwrap();
        let artifacts = vec![FrozenArtifact {
            id: "01J8".into(),
            source: "dateisysteme-fat.txt".into(),
            text: "Ein Cluster ist die kleinste adressierbare Einheit.".into(),
            title: Some("Cluster".into()),
            category: Some("concept".into()),
            tags: vec!["fat".into()],
        }];

        save_artifacts(dir.path(), &artifacts).unwrap();
        assert_eq!(load_artifacts(dir.path()).unwrap(), artifacts);
    }

    #[test]
    fn a_missing_pairs_file_says_which_path_it_wanted() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_pairs(dir.path()).unwrap_err().to_string();
        assert!(err.contains("pairs.json"), "unhelpful error: {err}");
    }

    #[test]
    fn the_eval_directory_comes_from_the_environment() {
        temp_env::with_var("ENGRAM_EVAL_DIR", Some("/somewhere/else"), || {
            assert_eq!(eval_dir(), std::path::PathBuf::from("/somewhere/else"));
        });
        temp_env::with_var_unset("ENGRAM_EVAL_DIR", || {
            assert_eq!(eval_dir(), std::path::PathBuf::from("eval-data"));
        });
    }
}
