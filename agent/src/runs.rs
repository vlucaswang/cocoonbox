//! Run results as the box's tools give them: `script_run`, plugin tools and `gui_plan`, run by the toolhost's
//! Node or by shuttle; the toolhost and serve both shape shuttle's runs here.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::toolhost::files;
use crate::toolhost::rpc::{self, Call, Client, Output};

pub const MAX_RESULT_BYTES: u64 = 256 << 10;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_TIMEOUT: Duration = Duration::from_secs(110);
const RUN_MARGIN: Duration = Duration::from_secs(15);

/// Where a run result's screenshot comes from.
pub trait Shots {
    /// One screenshot as base64, its long edge at most `max_edge`; with `keep`, also the path of the file that holds it.
    fn shot(
        &self,
        max_edge: Option<u32>,
        keep: bool,
    ) -> impl Future<Output = Result<Option<Shot>, String>> + Send;
}

/// A screenshot of a run result.
pub struct Shot {
    pub image_b64: String,
    pub path: Option<PathBuf>,
}

/// The arguments of `script_run` and plugin tools.
#[derive(Deserialize)]
pub struct RunArgs {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub params: Option<Map<String, Value>>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default = "default_screenshot")]
    pub screenshot: bool,
    #[serde(default)]
    pub max_edge: Option<u32>,
}

/// The arguments of `gui_plan`: the plan executor's parameters and the result's own.
#[derive(Deserialize)]
pub struct PlanArgs {
    #[serde(flatten)]
    pub params: Map<String, Value>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub screenshot: Option<bool>,
    #[serde(default)]
    pub max_edge: Option<u32>,
}

/// A script to run: module source, or a file.
pub enum Script {
    Code(String),
    File(PathBuf),
}

impl Script {
    /// The script of `code` or `script`, a path resolved against `workspace`.
    pub fn of(
        code: Option<String>,
        script: Option<String>,
        workspace: &Path,
    ) -> Result<Self, String> {
        match (code, script) {
            (Some(code), None) => Ok(Self::Code(code)),
            (None, Some(path)) => Ok(Self::File(files::resolve(workspace, &path))),
            _ => Err("pass exactly one of code or script".to_owned()),
        }
    }
}

/// `script_run`, or plugin tool `tool`, on shuttle.
pub async fn script(
    shuttle: &Client,
    shots: &impl Shots,
    workspace: &Path,
    tool: &str,
    args: Value,
) -> Result<Output, String> {
    let a: RunArgs = rpc::parse(args)?;
    let script = Script::of(a.code, a.script, workspace)?;
    let params = Value::Object(a.params.unwrap_or_default());
    let report = run_report(shuttle, tool, script, params, timeout_of(a.timeout_s)).await?;
    finish_run(report, a.screenshot, a.max_edge, shots).await
}

/// `gui_plan` on shuttle; the saved-flows listing is the caller's.
pub async fn plan(shuttle: &Client, shots: &impl Shots, args: Value) -> Result<Output, String> {
    let a: PlanArgs = rpc::parse(args)?;
    let report = plan_report(shuttle, Value::Object(a.params), timeout_of(a.timeout_s)).await?;
    finish_plan(report, a.screenshot, a.max_edge, shots).await
}

/// The report of shuttle's plan executor with `params`: the plan's result only.
pub async fn plan_report(
    shuttle: &Client,
    mut params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    params["timeout_s"] = timeout.as_secs().into();
    let out = relay(shuttle, "gui_plan", params, timeout).await?;
    let len = out.text.len() as u64;
    let mut report = if out.is_error {
        failed(out.text)?
    } else {
        let mut report = json!({});
        report["result"] =
            serde_json::from_str(&out.text).map_err(|e| format!("decode the plan result: {e}"))?;
        report
    };
    if len > MAX_RESULT_BYTES {
        cap(&mut report["result"]);
    }
    Ok(report)
}

/// A run's report as JSON text, with a kept screenshot unless turned off; an error in the report is an error result.
pub async fn finish_run(
    mut report: Value,
    screenshot: bool,
    max_edge: Option<u32>,
    shots: &impl Shots,
) -> Result<Output, String> {
    let mut out = Output::default();
    if screenshot {
        match shots.shot(max_edge, true).await {
            Ok(Some(shot)) => {
                if let Some(path) = shot.path {
                    report["screenshot_path"] = path.display().to_string().into();
                }
                out.image_b64 = Some(shot.image_b64);
            }
            Ok(None) => {}
            Err(e) => report["screenshot_error"] = e.into(),
        }
    }
    out.is_error = !report["error"].is_null();
    out.text = serde_json::to_string_pretty(&report).map_err(|e| format!("encode report: {e}"))?;
    Ok(out)
}

/// A plan's result as compact JSON, with a screenshot only after an escalation or on request.
pub async fn finish_plan(
    mut report: Value,
    screenshot: Option<bool>,
    max_edge: Option<u32>,
    shots: &impl Shots,
) -> Result<Output, String> {
    let mut out = Output::default();
    let mut body = match report.get_mut("error").filter(|e| !e.is_null()) {
        Some(error) => {
            out.is_error = true;
            json!({"error": error.take()})
        }
        None => report["result"].take(),
    };
    if screenshot.unwrap_or(body["ok"] != true) {
        match shots.shot(max_edge, false).await {
            Ok(shot) => out.image_b64 = shot.map(|s| s.image_b64),
            Err(e) if body.is_object() => body["screenshot_error"] = e.into(),
            Err(e) => return Err(e),
        }
    }
    out.text = body.to_string();
    Ok(out)
}

/// The run timeout of `timeout_s`, 90 s by default, from 1 to 110 s.
pub fn timeout_of(timeout_s: Option<u64>) -> Duration {
    timeout_s
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs)
        .clamp(Duration::from_secs(1), MAX_TIMEOUT)
}

/// The report of shuttle's run of `script` as tool `tool`.
async fn run_report(
    shuttle: &Client,
    tool: &str,
    script: Script,
    params: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let mut args = json!({"timeout_s": timeout.as_secs()});
    match script {
        Script::Code(code) => args["code"] = code.into(),
        Script::File(path) => {
            if let Ok(false) = tokio::fs::try_exists(&path).await {
                return Err(format!("script {} does not exist", path.display()));
            }
            args["script"] = path.display().to_string().into();
        }
    }
    args["params"] = params;
    let out = relay(shuttle, tool, args, timeout).await?;
    let len = out.text.len() as u64;
    let mut report: Value = if out.is_error {
        failed(out.text)?
    } else {
        serde_json::from_str(&out.text).map_err(|e| format!("decode the run report: {e}"))?
    };
    if len > MAX_RESULT_BYTES {
        cap(&mut report["result"]);
    }
    Ok(report)
}

/// shuttle's answer to a run of `tool` with `args` and `timeout`.
async fn relay(
    shuttle: &Client,
    tool: &str,
    args: Value,
    timeout: Duration,
) -> Result<Output, String> {
    let call = Call {
        agent_id: String::new(),
        tool: tool.to_owned(),
        args,
    };
    shuttle.desk(&call, timeout + RUN_MARGIN).await
}

/// The report of a failed run that shuttle answers as an error; an error answer that is no such report is an error.
fn failed(text: String) -> Result<Value, String> {
    match serde_json::from_str::<Value>(&text) {
        Ok(report) if report.is_object() && !report["error"].is_null() => Ok(report),
        _ => Err(text),
    }
}

/// Replaces a `result` past the size cap with the notice a local run gives.
fn cap(result: &mut Value) {
    if serde_json::to_vec(result).is_ok_and(|raw| raw.len() as u64 > MAX_RESULT_BYTES) {
        *result =
            format!("more than {MAX_RESULT_BYTES} bytes; write it to a file and return the path")
                .into();
    }
}

fn default_screenshot() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relayed_result_past_the_cap_becomes_the_local_notice() {
        let mut small = json!({"n": 1});
        cap(&mut small);
        assert_eq!(small, json!({"n": 1}));
        let mut big = Value::String("x".repeat(MAX_RESULT_BYTES as usize));
        cap(&mut big);
        assert_eq!(
            big,
            "more than 262144 bytes; write it to a file and return the path"
        );
    }

    #[test]
    fn a_timeout_is_clamped_to_one_and_110_seconds() {
        assert_eq!(timeout_of(None), Duration::from_secs(90));
        assert_eq!(timeout_of(Some(0)), Duration::from_secs(1));
        assert_eq!(timeout_of(Some(500)), Duration::from_secs(110));
    }
}
