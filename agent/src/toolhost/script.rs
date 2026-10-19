//! Runs on shuttle as the toolhost takes them: `script_run`, plugin tools and `gui_plan`, with their screenshots
//! from the toolhost's screen.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};

use super::cua::Cua;
use super::files;
use super::rpc::{self, Call, Client, Output};
use crate::llm::image_mime;
use crate::runs::{self, PlanArgs, Shot, Shots, timeout_of};

const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
const LOCATE_TIMEOUT: Duration = Duration::from_secs(20);
const KEPT_SHOTS: usize = 20;

/// Runs scripts and GUI plans on shuttle, when the box runs it, keeping their screenshots under `dir`.
pub struct Scripts {
    dir: PathBuf,
    shuttle: Option<Client>,
}

impl Scripts {
    /// Creates the runner whose runs go to `shuttle` and whose kept screenshots go under `dir`.
    pub fn new(dir: PathBuf, shuttle: Option<Client>) -> Self {
        Self { dir, shuttle }
    }

    /// Runs one script and reports it as JSON text, with a screenshot taken after the run unless turned off.
    pub async fn run(
        &self,
        screen: &impl Screen,
        workspace: &Path,
        args: Value,
    ) -> Result<Output, String> {
        self.run_with("script_run", screen, workspace, args).await
    }

    /// Runs one script as [`Scripts::run`] does, as tool `tool`, such as a plugin tool.
    pub async fn run_with(
        &self,
        tool: &str,
        screen: &impl Screen,
        workspace: &Path,
        args: Value,
    ) -> Result<Output, String> {
        runs::script(self.shuttle()?, &self.kept(screen), workspace, tool, args).await
    }

    /// Runs the plan executor and reports its result as compact JSON, with a screenshot only after an escalation or on request.
    pub async fn plan(
        &self,
        screen: &impl Screen,
        workspace: &Path,
        args: Value,
    ) -> Result<Output, String> {
        let a: PlanArgs = rpc::parse(args)?;
        if a.params.get("flows") == Some(&Value::Bool(true)) {
            let index = Self::flows(workspace).await;
            return Ok(Output::text(if index.trim().is_empty() {
                "no saved flows yet".to_owned()
            } else {
                index
            }));
        }
        let report = self
            .plan_report(Value::Object(a.params), timeout_of(a.timeout_s))
            .await?;
        runs::finish_plan(report, a.screenshot, a.max_edge, &self.kept(screen)).await
    }

    /// The plan executor's resolve of `target` in the focused window, pressed by action when `press`; its result.
    pub async fn locate(&self, target: &str, press: bool) -> Result<Value, String> {
        let params = json!({"locate": {"target": target, "press": press}});
        let mut report = self.plan_report(params, LOCATE_TIMEOUT).await?;
        if let Some(error) = report.get("error") {
            return Err(format!("locate: {error}"));
        }
        Ok(report["result"].take())
    }

    /// Rebuilds the flow index with the plan executor after the library sync changed flows.
    pub async fn reindex(&self) -> Result<Output, String> {
        let _index = super::flows::INDEX_LOCK.lock().await;
        let report = self
            .plan_report(json!({"reindex": true}), RELEASE_TIMEOUT)
            .await?;
        if let Some(error) = report.get("error") {
            return Err(format!("reindex flows: {error}"));
        }
        Ok(Output::text(report["result"].to_string()))
    }

    /// The saved flows' catalog, one `name(params)` line each, newest first; empty before the first save.
    pub async fn flows(workspace: &Path) -> String {
        tokio::fs::read_to_string(super::flows::index(workspace))
            .await
            .unwrap_or_default()
    }

    /// The screenshots of runs on `screen`, kept in the scripts' folder.
    pub fn kept<'a, S>(&'a self, screen: &'a S) -> Kept<'a, S> {
        Kept {
            scripts: self,
            screen,
        }
    }

    /// The report of one run of shuttle's plan executor with `params`: the plan's result only.
    async fn plan_report(&self, params: Value, timeout: Duration) -> Result<Value, String> {
        runs::plan_report(self.shuttle()?, params, timeout).await
    }

    fn shuttle(&self) -> Result<&Client, String> {
        self.shuttle.as_ref().ok_or_else(|| {
            "scripts and GUI plans run on shuttle, which this box does not run".to_owned()
        })
    }

    async fn screenshot(
        &self,
        screen: &impl Screen,
        max_edge: Option<u32>,
        id: &str,
    ) -> Result<Option<(PathBuf, String)>, String> {
        let Some(image_b64) = screen.screenshot_once(max_edge).await? else {
            return Ok(None);
        };
        let data = B64
            .decode(&image_b64)
            .map_err(|e| format!("decode screenshot: {e}"))?;
        let ext = image_mime(&data).trim_start_matches("image/");
        let shots = self.dir.join("shots");
        let ms = crate::now_ms();
        let path = shots.join(format!("{ms:013}-{id}.{ext}"));
        files::write_bytes(&path, &data).await?;
        prune(&shots).await;
        Ok(Some((path, image_b64)))
    }
}

/// The screen a script's or plan's screenshot comes from.
pub trait Screen: Sync {
    /// One screenshot as base64, its long edge at most `max_edge` where the screen can scale it.
    fn screenshot_once(
        &self,
        max_edge: Option<u32>,
    ) -> impl Future<Output = Result<Option<String>, String>> + Send;
}

impl Screen for Cua {
    async fn screenshot_once(&self, max_edge: Option<u32>) -> Result<Option<String>, String> {
        Cua::screenshot_once(self, max_edge).await
    }
}

/// The screen of the desk shuttle serves, through its `desk_look`.
pub struct ShuttleScreen<'a>(pub &'a Client);

impl Screen for ShuttleScreen<'_> {
    async fn screenshot_once(&self, _max_edge: Option<u32>) -> Result<Option<String>, String> {
        let call = Call {
            agent_id: "toolhost".to_owned(),
            tool: "desk_look".to_owned(),
            args: json!({}),
        };
        let out = self.0.desk(&call, rpc::DESK_DEADLINE).await?;
        if out.is_error {
            return Err(out.text);
        }
        Ok(out.image_b64)
    }
}

/// The screenshots of the toolhost's runs: from `screen`, kept in the scripts' folder when asked.
pub struct Kept<'a, S> {
    scripts: &'a Scripts,
    screen: &'a S,
}

impl<S: Screen> Shots for Kept<'_, S> {
    async fn shot(&self, max_edge: Option<u32>, keep: bool) -> Result<Option<Shot>, String> {
        if !keep {
            let shot = self.screen.screenshot_once(max_edge).await?;
            return Ok(shot.map(|image_b64| Shot {
                image_b64,
                path: None,
            }));
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let shot = self.scripts.screenshot(self.screen, max_edge, &id).await?;
        Ok(shot.map(|(path, image_b64)| Shot {
            image_b64,
            path: Some(path),
        }))
    }
}

async fn prune(shots: &Path) {
    let Ok(mut dir) = tokio::fs::read_dir(shots).await else {
        return;
    };
    let mut names = Vec::new();
    while let Ok(Some(e)) = dir.next_entry().await {
        names.push(e.path());
    }
    names.sort_unstable();
    for old in names.iter().rev().skip(KEPT_SHOTS) {
        let _ = tokio::fs::remove_file(old).await;
    }
}
