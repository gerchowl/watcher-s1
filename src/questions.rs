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

/// Which surface a threshold applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The wrapper (exit, silence threshold).
    Wrap,
    /// The PostToolUse judge (every piped exit-0 Bash call).
    Judge,
}

/// `threshold = 0.8` (both surfaces) or `threshold = { wrap = 0.5, judge = 0.8 }`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Threshold {
    Both(f64),
    PerSurface { wrap: f64, judge: f64 },
}

impl Threshold {
    pub fn get(&self, s: Surface) -> f64 {
        match (*self, s) {
            (Threshold::Both(t), _) => t,
            (Threshold::PerSurface { wrap, .. }, Surface::Wrap) => wrap,
            (Threshold::PerSurface { judge, .. }, Surface::Judge) => judge,
        }
    }
}

/// How the answers become one score in [0, 1].
///
/// - `kind = "mean"` (default): mean of p(q) for `positive` and 1 - p(q) for
///   `negative` (`noul` questions only).
/// - `kind = "logistic"`: sigmoid(bias + sum(weight * feature)). A feature
///   is a `noul` question name, or `"<choice question>:<label>+<label>"`, the
///   summed probability of those labels.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Score {
    #[serde(default)]
    pub kind: ScoreKind,
    #[serde(default)]
    pub positive: Vec<String>,
    #[serde(default)]
    pub negative: Vec<String>,
    #[serde(default)]
    pub bias: f64,
    #[serde(default)]
    pub weights: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScoreKind {
    #[default]
    Mean,
    Logistic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionSet {
    #[serde(default = "default_threshold")]
    pub threshold: Threshold,
    #[serde(default = "default_tail_bytes")]
    pub tail_bytes: usize,
    pub score: Score,
    pub questions: Map<String, Value>,
}

fn default_threshold() -> Threshold {
    Threshold::Both(0.8)
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

    pub fn threshold(&self, s: Surface) -> f64 {
        self.threshold.get(s)
    }

    fn question(&self, name: &str) -> Result<Question, String> {
        let q = self
            .questions
            .get(name)
            .ok_or_else(|| format!("unknown question {name}"))?;
        serde_json::from_value(q.clone()).map_err(|e| format!("question {name}: {e}"))
    }

    fn validate(&self) -> Result<(), String> {
        if self.questions.is_empty() {
            return Err("no questions defined".into());
        }
        for name in self.questions.keys() {
            let q = self.question(name)?;
            if q.kind != "noul" && q.kind != "choice" {
                return Err(format!("question {name}: unsupported type {:?}", q.kind));
            }
        }
        let noul_only = |name: &String| -> Result<(), String> {
            match self.question(name) {
                Ok(q) if q.kind == "noul" => Ok(()),
                Ok(_) => Err(format!("score: {name} is not a noul question")),
                Err(e) => Err(format!("score: {e}")),
            }
        };
        match self.score.kind {
            ScoreKind::Mean => {
                let scored: Vec<&String> = self.score.positive.iter().chain(&self.score.negative).collect();
                if scored.is_empty() {
                    return Err("score: no positive or negative questions".into());
                }
                for name in scored {
                    noul_only(name)?;
                }
            }
            ScoreKind::Logistic => {
                if self.score.weights.is_empty() {
                    return Err("score: logistic needs weights".into());
                }
                if !self.score.bias.is_finite() {
                    return Err("score: bias must be a number".into());
                }
                for (feature, w) in &self.score.weights {
                    if !w.as_f64().is_some_and(f64::is_finite) {
                        return Err(format!("score: weight for {feature} must be a number"));
                    }
                    match feature.split_once(':') {
                        None => noul_only(feature)?,
                        Some((qname, labels)) => {
                            let q = self.question(qname).map_err(|e| format!("score: {e}"))?;
                            if q.kind != "choice" {
                                return Err(format!("score: {qname} is not a choice question"));
                            }
                            for l in labels.split('+') {
                                if !q.criteria.contains_key(l) {
                                    return Err(format!("score: {qname} has no label {l:?}"));
                                }
                            }
                        }
                    }
                }
            }
        }
        for s in [Surface::Wrap, Surface::Judge] {
            let t = self.threshold(s);
            if !(0.0..=1.0).contains(&t) {
                return Err(format!("threshold {t} outside [0, 1]"));
            }
        }
        Ok(())
    }

    /// Fuse `answers` (the response's `answers` object) into one score.
    /// Returns `None` when any scored question is missing or malformed, so a
    /// partial answer never masquerades as a confident one.
    pub fn fuse(&self, answers: &Map<String, Value>) -> Option<f64> {
        match self.score.kind {
            ScoreKind::Mean => {
                let mut parts = Vec::new();
                for name in &self.score.positive {
                    parts.push(noul(answers, name)?);
                }
                for name in &self.score.negative {
                    parts.push(1.0 - noul(answers, name)?);
                }
                Some(parts.iter().sum::<f64>() / parts.len() as f64)
            }
            ScoreKind::Logistic => {
                let mut z = self.score.bias;
                for (feature, w) in &self.score.weights {
                    let x = match feature.split_once(':') {
                        None => noul(answers, feature)?,
                        Some((q, labels)) => choice_sum(answers, q, labels)?,
                    };
                    z += w.as_f64()? * x;
                }
                Some(1.0 / (1.0 + (-z).exp()))
            }
        }
    }
}

/// Summed probability of `labels` (`a+b`) in a `choice` answer, clamped.
pub fn choice_sum(answers: &Map<String, Value>, question: &str, labels: &str) -> Option<f64> {
    let probs = answers.get(question)?.get("probabilities")?.as_object()?;
    let mut sum = 0.0;
    for l in labels.split('+') {
        let p = probs.get(l)?.as_f64()?;
        if !p.is_finite() {
            return None;
        }
        sum += p;
    }
    Some(sum.clamp(0.0, 1.0))
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

    const MEAN_SET: &str = r#"
        threshold = 0.8
        [score]
        positive = ["failing"]
        negative = ["clean_done"]
        [questions.failing]
        type = "noul"
        instructions = "x"
        criteria = { true = "a", false = "b" }
        [questions.clean_done]
        type = "noul"
        instructions = "y"
        criteria = { true = "a", false = "b" }
    "#;

    fn all_answers(p: f64) -> Value {
        let mut m = Map::new();
        for q in [
            "failing",
            "clean_done",
            "red",
            "any_failure",
            "exit_status",
            "tests_failed",
            "ends_with_error",
            "succeeded",
            "error_present",
        ] {
            let v = if matches!(q, "clean_done" | "succeeded") {
                1.0 - p
            } else {
                p
            };
            m.insert(q.into(), json!({ "noul": v }));
        }
        m.insert(
            "outcome".into(),
            json!({"probabilities": {"success": 1.0 - p, "failure": p * 0.7, "partial": p * 0.3, "info": 0.0}}),
        );
        Value::Object(m)
    }

    #[test]
    fn builtin_is_the_measured_logistic_set() {
        let s = QuestionSet::builtin();
        assert_eq!(s.score.kind, ScoreKind::Logistic);
        assert_eq!(s.threshold(Surface::Wrap), 0.5);
        assert_eq!(s.threshold(Surface::Judge), 0.8);
        assert_eq!(s.tail_bytes, 4096);
        assert_eq!(s.questions.len(), 10);
        assert_eq!(s.questions["outcome"]["type"], "choice");
        assert_eq!(
            s.questions["failing"]["instructions"],
            "Does the output show an error that the process did not recover from?"
        );
    }

    #[test]
    fn builtin_separates_clear_failure_from_clear_success() {
        let s = QuestionSet::builtin();
        let fail = s.fuse(all_answers(0.95).as_object().unwrap()).unwrap();
        let ok = s.fuse(all_answers(0.05).as_object().unwrap()).unwrap();
        assert!(fail >= s.threshold(Surface::Judge), "{fail}");
        assert!(ok < s.threshold(Surface::Wrap), "{ok}");
    }

    #[test]
    fn logistic_refuses_partial_answers() {
        let s = QuestionSet::builtin();
        let mut a = all_answers(0.9);
        a.as_object_mut().unwrap().remove("outcome");
        assert_eq!(s.fuse(a.as_object().unwrap()), None);
    }

    #[test]
    fn choice_sum_adds_labels() {
        let a = json!({"o": {"probabilities": {"x": 0.25, "y": 0.5, "z": 0.25}}});
        assert_eq!(choice_sum(a.as_object().unwrap(), "o", "x+y"), Some(0.75));
        assert_eq!(choice_sum(a.as_object().unwrap(), "o", "x+nope"), None);
    }

    #[test]
    fn mean_kind_still_works() {
        let s = QuestionSet::from_toml(MEAN_SET).unwrap();
        assert_eq!(s.score.kind, ScoreKind::Mean);
        assert_eq!(s.threshold(Surface::Judge), 0.8);
        let a = json!({"failing": {"noul": 0.9}, "clean_done": {"noul": 0.2}});
        assert!((s.fuse(a.as_object().unwrap()).unwrap() - 0.85).abs() < 1e-9);
        let a = json!({"failing": {"noul": 0.9}});
        assert_eq!(s.fuse(a.as_object().unwrap()), None);
    }

    #[test]
    fn logistic_validation() {
        let base = r#"
            [questions.a]
            type = "noul"
            instructions = "x"
            criteria = { true = "a", false = "b" }
            [questions.c]
            type = "choice"
            instructions = "y"
            criteria = { good = "g", bad = "b" }
        "#;
        let ok = format!("[score]\nkind = \"logistic\"\nbias = -1\nweights = {{ a = 1.0, \"c:bad\" = 2.0 }}\n{base}");
        assert!(QuestionSet::from_toml(&ok).is_ok());
        let bad_label = ok.replace("c:bad", "c:worse");
        assert!(QuestionSet::from_toml(&bad_label).unwrap_err().contains("no label"));
        let noul_as_choice = ok.replace("c:bad", "a:true");
        assert!(
            QuestionSet::from_toml(&noul_as_choice)
                .unwrap_err()
                .contains("not a choice")
        );
        let none = format!("[score]\nkind = \"logistic\"\n{base}");
        assert!(QuestionSet::from_toml(&none).unwrap_err().contains("needs weights"));
        let threshold_cfg = format!("threshold = {{ wrap = 1.5, judge = 0.8 }}\n{ok}");
        assert!(QuestionSet::from_toml(&threshold_cfg).unwrap_err().contains("outside"));
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
