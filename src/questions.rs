//! The System One question set: the questions sent per state, how their
//! answers fuse into one score, and the threshold that flags a failure.
//!
//! The set is data (`questions/builtin.toml`, embedded as `builtin`), so a
//! config can swap it for a file of the same shape without a rebuild.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::Path;

/// The embedded default set (the measured winners of g-fleet#244).
pub const BUILTIN_TOML: &str = include_str!("../questions/builtin.toml");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    #[serde(rename = "type")]
    pub kind: String,
    pub instructions: String,
    pub criteria: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Score {
    #[serde(default)]
    pub positive: Vec<String>,
    #[serde(default)]
    pub negative: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionSet {
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "default_tail_bytes")]
    pub tail_bytes: usize,
    pub score: Score,
    pub questions: Map<String, Value>,
}

fn default_threshold() -> f64 {
    0.8
}

fn default_tail_bytes() -> usize {
    4096
}

impl QuestionSet {
    pub fn builtin() -> Self {
        Self::from_toml(BUILTIN_TOML).expect("embedded questions/builtin.toml is valid")
    }

    pub fn from_toml(text: &str) -> Result<Self, String> {
        let set: QuestionSet = toml::from_str(text).map_err(|e| e.to_string())?;
        set.validate()?;
        Ok(set)
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let set: QuestionSet = serde_json::from_str(text).map_err(|e| e.to_string())?;
        set.validate()?;
        Ok(set)
    }

    /// `builtin`, or a path to a `.toml` / `.json` file of the same shape.
    pub fn load(spec: &str) -> Result<Self, String> {
        if spec == "builtin" {
            return Ok(Self::builtin());
        }
        let text = std::fs::read_to_string(spec).map_err(|e| format!("questions {spec}: {e}"))?;
        let res = if Path::new(spec).extension().is_some_and(|e| e == "json") {
            Self::from_json(&text)
        } else {
            Self::from_toml(&text)
        };
        res.map_err(|e| format!("questions {spec}: {e}"))
    }

    fn validate(&self) -> Result<(), String> {
        if self.questions.is_empty() {
            return Err("no questions defined".into());
        }
        for (name, q) in &self.questions {
            let q: Question = serde_json::from_value(q.clone()).map_err(|e| format!("question {name}: {e}"))?;
            if q.kind != "noul" && q.kind != "choice" {
                return Err(format!("question {name}: unsupported type {:?}", q.kind));
            }
        }
        let scored: Vec<&String> = self.score.positive.iter().chain(&self.score.negative).collect();
        if scored.is_empty() {
            return Err("score: no positive or negative questions".into());
        }
        for name in scored {
            match self.questions.get(name).and_then(|q| q.get("type")) {
                Some(Value::String(t)) if t == "noul" => {}
                Some(_) => return Err(format!("score: {name} is not a noul question")),
                None => return Err(format!("score: unknown question {name}")),
            }
        }
        if !(0.0..=1.0).contains(&self.threshold) {
            return Err(format!("threshold {} outside [0, 1]", self.threshold));
        }
        Ok(())
    }

    /// Fuse `answers` (the response's `answers` object) into one score.
    /// Returns `None` when any scored question is missing or malformed, so a
    /// partial answer never masquerades as a confident one.
    pub fn fuse(&self, answers: &Map<String, Value>) -> Option<f64> {
        let mut parts = Vec::new();
        for name in &self.score.positive {
            parts.push(noul(answers, name)?);
        }
        for name in &self.score.negative {
            parts.push(1.0 - noul(answers, name)?);
        }
        Some(parts.iter().sum::<f64>() / parts.len() as f64)
    }
}

/// P(true) of a `noul` answer, clamped to [0, 1].
pub fn noul(answers: &Map<String, Value>, name: &str) -> Option<f64> {
    let p = answers.get(name)?.get("noul")?.as_f64()?;
    p.is_finite().then(|| p.clamp(0.0, 1.0))
}

/// The state string the questions were measured on.
pub fn build_state(cmd: &str, tail: &str) -> String {
    format!("Command: {cmd}\n(The exit status is not shown.)\nLast output:\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_matches_the_measured_questions() {
        let s = QuestionSet::builtin();
        assert_eq!(s.threshold, 0.8);
        assert_eq!(s.tail_bytes, 4096);
        let names: Vec<&String> = s.questions.keys().collect();
        assert_eq!(names, ["failing", "clean_done"]);
        assert_eq!(
            s.questions["failing"]["instructions"],
            "Does the output show an error that the process did not recover from?"
        );
        assert_eq!(
            s.questions["clean_done"]["criteria"]["false"],
            "the output ends mid-work, or with something broken or unfinished"
        );
    }

    #[test]
    fn fuse_is_mean_of_failing_and_not_clean_done() {
        let s = QuestionSet::builtin();
        let a = json!({"failing": {"noul": 0.9}, "clean_done": {"noul": 0.2}});
        let f = s.fuse(a.as_object().unwrap()).unwrap();
        assert!((f - 0.85).abs() < 1e-9);
    }

    #[test]
    fn fuse_refuses_partial_answers() {
        let s = QuestionSet::builtin();
        let a = json!({"failing": {"noul": 0.9}});
        assert_eq!(s.fuse(a.as_object().unwrap()), None);
        let a = json!({"failing": {"noul": "x"}, "clean_done": {"noul": 0.1}});
        assert_eq!(s.fuse(a.as_object().unwrap()), None);
    }

    #[test]
    fn rejects_scoring_unknown_or_choice_questions() {
        let bad = r#"
            [score]
            positive = ["nope"]
            [questions.a]
            type = "noul"
            instructions = "x"
            criteria = { true = "a", false = "b" }
        "#;
        assert!(QuestionSet::from_toml(bad).unwrap_err().contains("unknown question"));
        let bad = r#"
            [score]
            positive = ["a"]
            [questions.a]
            type = "choice"
            instructions = "x"
            criteria = { x = "a", y = "b" }
        "#;
        assert!(QuestionSet::from_toml(bad).unwrap_err().contains("not a noul"));
    }

    #[test]
    fn state_has_the_measured_shape() {
        assert_eq!(
            build_state("cargo test", "ok\n"),
            "Command: cargo test\n(The exit status is not shown.)\nLast output:\nok\n"
        );
    }
}
