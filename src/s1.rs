//! The System One client: one request per state carrying every question
//! (the server caches the state prefix), ordered endpoints with a breaker
//! each, a hard deadline, and fail-open errors.

use crate::breaker::Breaker;
use crate::config::S1Config;
use crate::http;
use crate::questions::{QuestionSet, build_state, noul};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};

/// The `s1` object of an event.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Verdict {
    pub endpoint: String,
    pub failing: Option<f64>,
    pub clean_done: Option<f64>,
    pub fused: f64,
    pub latency_ms: u64,
}

pub struct Client {
    pub cfg: S1Config,
    pub questions: QuestionSet,
    breaker: Breaker,
}

impl Client {
    pub fn new(cfg: S1Config, questions: QuestionSet, breaker: Breaker) -> Self {
        Self {
            cfg,
            questions,
            breaker,
        }
    }

    pub fn threshold(&self) -> f64 {
        self.questions.threshold
    }

    /// Judge `tail` (already cut to the question set's `tail_bytes`).
    /// `budget` bounds the whole call across every endpoint tried.
    pub fn judge(&self, cmd: &str, tail: &str, budget: Duration) -> Result<Verdict, String> {
        let state = build_state(cmd, tail);
        let body = serde_json::to_vec(&json!({"state": state, "questions": self.questions.questions}))
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + budget;
        let mut errors = Vec::new();
        for url in &self.cfg.urls {
            if Instant::now() >= deadline {
                errors.push("budget exhausted".to_string());
                break;
            }
            if !self.breaker.allow(url) {
                errors.push(format!("{url}: breaker open"));
                continue;
            }
            let attempt = deadline.min(Instant::now() + self.cfg.timeout);
            let t0 = Instant::now();
            match self.ask(url, &body, attempt) {
                Ok(answers) => match self.questions.fuse(&answers) {
                    Some(fused) => {
                        self.breaker.record(url, true);
                        return Ok(Verdict {
                            endpoint: url.clone(),
                            failing: noul(&answers, "failing"),
                            clean_done: noul(&answers, "clean_done"),
                            fused,
                            latency_ms: t0.elapsed().as_millis() as u64,
                        });
                    }
                    None => {
                        self.breaker.record(url, false);
                        errors.push(format!("{url}: answers missing scored questions"));
                    }
                },
                Err(e) => {
                    self.breaker.record(url, false);
                    errors.push(format!("{url}: {e}"));
                }
            }
        }
        Err(errors.join("; "))
    }

    fn ask(&self, url: &str, body: &[u8], deadline: Instant) -> Result<Map<String, Value>, String> {
        let r = http::post_json(url, body, deadline)?;
        if r.status != 200 {
            return Err(format!("HTTP {}", r.status));
        }
        let v: Value = serde_json::from_slice(&r.body).map_err(|e| format!("bad JSON: {e}"))?;
        match v.get("answers") {
            Some(Value::Object(m)) => Ok(m.clone()),
            _ => Err("response has no answers object".into()),
        }
    }
}
