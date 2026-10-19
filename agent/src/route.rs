//! Tier router of a box that runs GUI plans: asks gw's decisions model which tool tier fits a task.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::llm::Usage;

const DESK_ROUTE: &str = include_str!("../../image/desk/route.json");
const TASK: &str = "\"__TASK__\"";
const TIMEOUT: Duration = Duration::from_secs(2);
const BUILDS: &str = "typesafe/jev-1.14-";
pub(crate) const BUILD: &str = "typesafe/jev-1.14-20261014";
const ERROR_BODY_CHARS: usize = 300;

/// Lowest confidence at which a route is followed rather than left to the caller.
pub const CONFIDENT: f64 = 0.8;

/// The tiers with what each one means for the tools to use.
pub const TIERS: &[(&str, &str)] = &[
    ("command", "use shell and the file tools"),
    (
        "cua",
        "a single look or click: screenshot, computer_state, computer_act (in the box's own agent: delegate_computer)",
    ),
    (
        "browser",
        "a web page: browser_fetch to read it, the other browser_* tools to click, type or sign in, also for a form filled several times",
    ),
    (
        "plan",
        "run desktop-app GUI work as one gui_plan call with the whole plan, also a batch of files moved, copied or renamed in a file manager (select them by pattern, not one by one); write one script on the desk API for script_run only for logic a plan cannot hold",
    ),
    (
        "jev",
        "a pure judgment from the given text: no action on the computer is needed",
    ),
];

static WARNED: AtomicBool = AtomicBool::new(false);

/// Client of gw's `/v1/decisions` with the V4 tier prompt.
pub struct Router {
    http: reqwest::Client,
    url: String,
    api_key: String,
    route: &'static str,
    model: String,
}

impl Router {
    /// Creates the router for gw at `base_url` (ending in `/v1`).
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .map_err(|e| format!("build router client: {e}"))?;
        let route: Value =
            serde_json::from_str(DESK_ROUTE).map_err(|e| format!("decode route.json: {e}"))?;
        Ok(Self {
            http,
            url: format!("{}/decisions", base_url.trim_end_matches('/')),
            api_key: api_key.to_owned(),
            route: DESK_ROUTE,
            model: route["model"].as_str().unwrap_or_default().to_owned(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Routes `task`; a failure comes back as an uncertain route that carries the error.
    pub async fn route(&self, task: &str) -> Route {
        match self.decide(task).await {
            Ok(route) => route,
            Err(e) => Route {
                tier: String::new(),
                confidence: 0.0,
                error: Some(e),
                usage: None,
            },
        }
    }

    async fn decide(&self, task: &str) -> Result<Route, String> {
        let task = serde_json::to_string(task).map_err(|e| format!("encode task: {e}"))?;
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .body(self.route.replacen(TASK, &task, 1))
            .send()
            .await
            .map_err(|e| format!("post {}: {e}", self.url))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| format!("read decisions reply: {e}"))?;
        if !status.is_success() {
            let text: String = text.chars().take(ERROR_BODY_CHARS).collect();
            return Err(format!("gw decisions returned {status}: {text}"));
        }
        let reply: Value =
            serde_json::from_str(&text).map_err(|e| format!("decode decisions reply: {e}"))?;
        let served = reply["model"].as_str().unwrap_or_default();
        if !is_build(served) {
            return Err(format!(
                "gw decisions served {} for {}",
                reply["model"], self.model
            ));
        }
        if served != BUILD && !WARNED.swap(true, Ordering::Relaxed) {
            eprintln!("router: jev answers from build {served}, not the tested {BUILD}");
        }
        let usage = Some(Usage {
            input: reply["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output: reply["usage"]["output_tokens"].as_u64().unwrap_or(0),
            cached: 0,
        });
        let answer = &reply["answers"]["tier"];
        let Some(tier) = answer["choice"].as_str().filter(|t| meaning(t).is_some()) else {
            return Ok(Route {
                tier: String::new(),
                confidence: 0.0,
                error: Some(format!("decisions reply has no known tier: {answer}")),
                usage,
            });
        };
        Ok(Route {
            tier: tier.to_owned(),
            confidence: answer["confidence"].as_f64().unwrap_or(0.0),
            error: None,
            usage,
        })
    }
}

/// One routing decision.
#[derive(Debug)]
pub struct Route {
    pub tier: String,
    pub confidence: f64,
    pub error: Option<String>,
    pub usage: Option<Usage>,
}

impl Route {
    /// Whether the route is confident enough to follow.
    pub fn confident(&self) -> bool {
        self.error.is_none() && self.confidence >= CONFIDENT
    }
}

/// Whether tool `tool` leads the offered tools when a turn is routed to `tier`.
pub fn leads(tier: &str, tool: &str) -> bool {
    match tier {
        "command" => matches!(
            tool,
            "shell" | "read_file" | "write_file" | "edit_file" | "list_dir"
        ),
        "cua" => matches!(tool, "delegate_computer" | "screenshot"),
        "browser" => tool.starts_with("browser_"),
        "plan" => matches!(tool, "gui_plan" | "script_run"),
        _ => false,
    }
}

/// What tier `tier` means for the tools to use.
pub fn meaning(tier: &str) -> Option<&'static str> {
    TIERS.iter().find(|(t, _)| *t == tier).map(|(_, m)| *m)
}

/// Whether `model` names a dated build of the router's jev version.
pub(crate) fn is_build(model: &str) -> bool {
    model
        .strip_prefix(BUILDS)
        .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_offers_exactly_the_tiers_and_takes_one_task() {
        assert_eq!(DESK_ROUTE.matches(TASK).count(), 1);
        let body: Value = serde_json::from_str(&DESK_ROUTE.replacen(TASK, "\"x\"", 1)).unwrap();
        assert_eq!(body["model"], "typesafe/jev-1.14");
        assert!(BUILD.starts_with(body["model"].as_str().unwrap()));
        assert_eq!(body["state"]["task"], "x");
        let criteria = body["questions"]["tier"]["criteria"].as_object().unwrap();
        let mut offered: Vec<&str> = criteria.keys().map(String::as_str).collect();
        let mut known: Vec<&str> = TIERS.iter().map(|(t, _)| *t).collect();
        offered.sort_unstable();
        known.sort_unstable();
        assert_eq!(offered, known);
    }

    #[test]
    fn any_dated_build_of_the_router_version_answers_and_nothing_else() {
        assert!(is_build(BUILD) && is_build("typesafe/jev-1.14-20270101"));
        for other in [
            "typesafe/jev-1.13-20260917",
            "typesafe/jev-1.14",
            "~typesafe/jev-latest",
            "typesafe/jev-preview",
            "typesafe/jev-1.14-2026",
            "typesafe/jev-1.141-20261014",
        ] {
            assert!(!is_build(other), "{other}");
        }
    }

    #[test]
    fn a_failed_or_weak_route_is_not_confident() {
        let failed = Route {
            tier: "plan".into(),
            confidence: 0.99,
            error: Some("gw decisions returned 429".into()),
            usage: None,
        };
        let weak = Route {
            tier: "cua".into(),
            confidence: 0.55,
            error: None,
            usage: None,
        };
        let strong = Route {
            tier: "browser".into(),
            confidence: CONFIDENT,
            error: None,
            usage: None,
        };
        assert!(!failed.confident() && !weak.confident() && strong.confident());
    }
}
