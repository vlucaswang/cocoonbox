//! The script tool: one stateless script run as `user` by Node; `gui_plan` runs the image's plan executor that way.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::android::Android;
use super::cua::Cua;
use super::files;
use super::rpc::{self, Call, Client, Output};
use super::shell::Capture;
use super::timing;
use crate::llm::image_mime;
use crate::runs::{self, MAX_RESULT_BYTES, PlanArgs, RunArgs, Script, Shot, Shots, timeout_of};
use crate::sys;

const TERM_GRACE: Duration = Duration::from_secs(2);
const PIPE_GRACE: Duration = Duration::from_millis(500);
const RELEASE_TIMEOUT: Duration = Duration::from_secs(10);
const LOCATE_TIMEOUT: Duration = Duration::from_secs(20);
const KEPT_SHOTS: usize = 20;
const ERROR_LINES: usize = 12;
const DESK_PLAN: &str = "/opt/desk/lib/gui_plan.mts";
const GAMES_PATH: &str = ":/usr/local/games:/usr/games";
const ENTRY: &str = r#"import { writeFileSync } from 'node:fs'
import { pathToFileURL } from 'node:url'
const env = process.env
const mod = await import(pathToFileURL(env.CB_SCRIPT).href)
if (typeof mod.main === 'function') {
  const value = await mod.main(JSON.parse(env.CB_PARAMS))
  writeFileSync(env.CB_RESULT, JSON.stringify(value ?? null) ?? 'null')
}
if (globalThis.$snapshot !== undefined) writeFileSync(env.CB_SNAPSHOT, String(globalThis.$snapshot))
setTimeout(() => process.exit(), 500).unref()
"#;

/// Runs scripts with Node at `bin`, one scratch directory per run under `dir`, or on shuttle when set.
pub struct Scripts {
    bin: PathBuf,
    dir: PathBuf,
    socket: PathBuf,
    shuttle: Option<Client>,
}

impl Scripts {
    /// Creates the runner of `bin` with its scratch root `dir`; scripts reach the toolhost at `socket`, and run on `shuttle` when set.
    pub fn new(bin: PathBuf, dir: PathBuf, socket: PathBuf, shuttle: Option<Client>) -> Self {
        Self {
            bin,
            dir,
            socket,
            shuttle,
        }
    }

    /// Runs one script and reports it as JSON text, with a screenshot taken after the run unless turned off.
    pub async fn run(
        &self,
        screen: &impl Screen,
        workspace: &Path,
        args: Value,
    ) -> Result<Output, String> {
        self.run_with("script_run", &self.bin, screen, workspace, args)
            .await
    }

    /// Runs one script as [`Scripts::run`] does, with the program at `bin` in place of Node; shuttle runs it as tool `tool`.
    pub async fn run_with(
        &self,
        tool: &str,
        bin: &Path,
        screen: &impl Screen,
        workspace: &Path,
        args: Value,
    ) -> Result<Output, String> {
        let shots = self.kept(screen);
        if let Some(shuttle) = &self.shuttle {
            return runs::script(shuttle, &shots, workspace, tool, args).await;
        }
        let a: RunArgs = rpc::parse(args)?;
        let script = Script::of(a.code, a.script, workspace)?;
        let id = uuid::Uuid::new_v4().simple().to_string();
        let params = Value::Object(a.params.unwrap_or_default()).to_string();
        let report = self
            .run_script(
                bin,
                script,
                &params,
                workspace,
                timeout_of(a.timeout_s),
                &id,
            )
            .await?;
        runs::finish_run(report, a.screenshot, a.max_edge, &shots).await
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
            .plan_report(Value::Object(a.params), workspace, timeout_of(a.timeout_s))
            .await?;
        runs::finish_plan(report, a.screenshot, a.max_edge, &self.kept(screen)).await
    }

    /// The plan executor's resolve of `target` in the focused window, pressed by action when `press`; its result.
    pub async fn locate(
        &self,
        workspace: &Path,
        target: &str,
        press: bool,
    ) -> Result<Value, String> {
        let params = json!({"locate": {"target": target, "press": press}});
        let mut report = self.plan_report(params, workspace, LOCATE_TIMEOUT).await?;
        if let Some(error) = report.get("error") {
            return Err(format!("locate: {error}"));
        }
        Ok(report["result"].take())
    }

    /// Rebuilds the flow index with the plan executor after the library sync changed flows.
    pub async fn reindex(&self, workspace: &Path) -> Result<Output, String> {
        let _index = super::flows::INDEX_LOCK.lock().await;
        let report = self
            .plan_report(json!({"reindex": true}), workspace, RELEASE_TIMEOUT)
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

    /// The report of one run of the plan executor with `params`; on shuttle it holds the plan's result only.
    async fn plan_report(
        &self,
        params: Value,
        workspace: &Path,
        timeout: Duration,
    ) -> Result<Value, String> {
        let Some(shuttle) = &self.shuttle else {
            let id = uuid::Uuid::new_v4().simple().to_string();
            return self
                .run_script(
                    &self.bin,
                    Script::File(PathBuf::from(DESK_PLAN)),
                    &params.to_string(),
                    workspace,
                    timeout,
                    &id,
                )
                .await;
        };
        runs::plan_report(shuttle, params, timeout).await
    }

    /// The screenshots of runs on `screen`, kept in the scripts' folder.
    pub fn kept<'a, S>(&'a self, screen: &'a S) -> Kept<'a, S> {
        Kept {
            scripts: self,
            screen,
        }
    }

    /// Runs one script in its own scratch directory.
    async fn run_script(
        &self,
        bin: &Path,
        script: Script,
        params: &str,
        workspace: &Path,
        timeout: Duration,
        id: &str,
    ) -> Result<Value, String> {
        let run_dir = self.dir.join(id);
        tokio::fs::create_dir_all(&run_dir)
            .await
            .map_err(|e| format!("create {}: {e}", run_dir.display()))?;
        let _ = tokio::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700)).await;
        let report = self
            .execute(bin, script, params, workspace, &run_dir, timeout)
            .await;
        let _ = tokio::fs::remove_dir_all(&run_dir).await;
        report
    }

    async fn execute(
        &self,
        bin: &Path,
        script: Script,
        params: &str,
        workspace: &Path,
        run_dir: &Path,
        timeout: Duration,
    ) -> Result<Value, String> {
        let script = match script {
            Script::Code(code) => {
                let path = run_dir.join("script.mts");
                files::write_bytes(&path, code.as_bytes()).await?;
                path
            }
            Script::File(path) => {
                if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    return Err(format!("script {} does not exist", path.display()));
                }
                path
            }
        };
        let entry = run_dir.join("entry.mjs");
        files::write_bytes(&entry, ENTRY.as_bytes()).await?;
        let (result_path, snapshot_path) =
            (run_dir.join("result.json"), run_dir.join("snapshot.txt"));
        let started = Instant::now();
        timing::mark("prepare");
        let path = std::env::var("PATH").unwrap_or_default() + GAMES_PATH;
        let mut node = Command::new(bin);
        node.arg(&entry)
            .current_dir(workspace)
            .env("PATH", path)
            .env("CB_SCRIPT", &script)
            .env("CB_PARAMS", params)
            .env("CB_RESULT", &result_path)
            .env("CB_SNAPSHOT", &snapshot_path)
            .env("TOOLHOST_SOCKET", &self.socket)
            .env("SHUTTLE_FLOWS", workspace.join("flows"))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        sys::unprotect(&mut node);
        let mut child = node
            .spawn()
            .map_err(|e| format!("start {}: {e}", bin.display()))?;
        timing::mark("spawn");
        let mut guard = RunGuard {
            pgid: child.id(),
            armed: true,
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err("script pipes missing".to_owned());
        };
        let (mut logs, mut errs) = (Capture::default(), Capture::default());
        let ran = tokio::time::timeout(timeout, async {
            let drained = async {
                tokio::join!(drain(stdout, &mut logs), drain(stderr, &mut errs));
            };
            tokio::pin!(drained);
            let mut open = true;
            let status = tokio::select! {
                status = child.wait() => status,
                () = &mut drained => {
                    open = false;
                    child.wait().await
                }
            };
            if open
                && tokio::time::timeout(PIPE_GRACE, &mut drained)
                    .await
                    .is_err()
            {
                guard.signal(libc::SIGKILL);
                let _ = tokio::time::timeout(PIPE_GRACE, &mut drained).await;
            }
            status
        })
        .await;
        let status = match ran {
            Ok(status) => Some(status.map_err(|e| format!("wait for the script: {e}"))?),
            Err(_) => {
                guard.signal(libc::SIGTERM);
                if tokio::time::timeout(TERM_GRACE, child.wait())
                    .await
                    .is_err()
                {
                    guard.signal(libc::SIGKILL);
                    let _ = child.wait().await;
                }
                None
            }
        };
        guard.armed = false;
        timing::mark("run");
        let stderr = errs.render();
        let mut report = json!({
            "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "exit_code": Value::Null,
        });
        report["result"] = match read_capped(&result_path).await {
            None => Value::Null,
            Some(raw) => serde_json::from_slice(&raw)
                .unwrap_or_else(|_| String::from_utf8_lossy(&raw).into_owned().into()),
        };
        report["logs"] = logs.render().into();
        match status {
            None => {
                report["error"] = json!({"kind": "timeout", "message": format!("the run passed {}s and was stopped", timeout.as_secs())});
            }
            Some(status) => {
                report["exit_code"] = status.code().into();
                if let Some(error) = failure(status, &stderr) {
                    report["error"] = error;
                }
            }
        }
        report["stderr"] = stderr.into();
        if let Some(snapshot) = read_capped(&snapshot_path).await {
            report["snapshot"] = String::from_utf8_lossy(&snapshot).into_owned().into();
        }
        Ok(report)
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

impl Screen for Android {
    async fn screenshot_once(&self, _max_edge: Option<u32>) -> Result<Option<String>, String> {
        Ok(self.look().await?.image_b64)
    }
}

/// The screen of the desk shuttle serves, through its `desk_look`.
pub struct ShuttleScreen<'a>(pub &'a Client);

impl Screen for ShuttleScreen<'_> {
    async fn screenshot_once(&self, max_edge: Option<u32>) -> Result<Option<String>, String> {
        let call = Call {
            agent_id: "toolhost".to_owned(),
            tool: "desk_look".to_owned(),
            args: json!({"max_edge": max_edge}),
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

/// Signals the run's process group on drop unless disarmed.
struct RunGuard {
    pgid: Option<u32>,
    armed: bool,
}

impl RunGuard {
    fn signal(&self, signal: i32) {
        if let Some(pgid) = self.pgid {
            signal_run(pgid, signal);
        }
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.armed {
            self.signal(libc::SIGKILL);
        }
    }
}

/// Signals the processes of group `pgid` but the apps the run started in app scopes of their own.
fn signal_run(pgid: u32, signal: i32) {
    let (Ok(own), Ok(entries)) = (
        std::fs::read_to_string("/proc/self/cgroup"),
        std::fs::read_dir("/proc"),
    ) else {
        let _ = sys::signal_group(pgid, signal);
        return;
    };
    for pid in entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
    {
        let in_group = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| process_group(&stat))
            == Some(pgid);
        if in_group
            && !std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
                .is_ok_and(|cgroup| launched(&own, &cgroup))
        {
            let _ = sys::signal_process(pid, signal);
        }
    }
}

/// The process group field of a `/proc/<pid>/stat` line, read after the parenthesized command name.
fn process_group(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// Whether `cgroup` is an app scope other than `own`, the toolhost's cgroup that a run inherits.
fn launched(own: &str, cgroup: &str) -> bool {
    cgroup != own
        && cgroup
            .lines()
            .any(|l| l.contains("/app.slice/") && l.trim_end().ends_with(".scope"))
}

async fn drain(mut r: impl AsyncRead + Unpin, capture: &mut Capture) {
    let mut chunk = vec![0u8; 16 << 10];
    while let Ok(n) = r.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        capture.push(&chunk[..n]);
    }
}

/// The file's bytes, or a notice in their place past the size cap; `None` when the script wrote none.
async fn read_capped(path: &Path) -> Option<Vec<u8>> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut raw = Vec::new();
    file.take(MAX_RESULT_BYTES + 1)
        .read_to_end(&mut raw)
        .await
        .ok()?;
    if raw.len() as u64 > MAX_RESULT_BYTES {
        let notice = format!(
            "\"more than {MAX_RESULT_BYTES} bytes; write it to a file and return the path\""
        );
        return Some(notice.into_bytes());
    }
    Some(raw)
}

fn failure(status: ExitStatus, stderr: &str) -> Option<Value> {
    if status.success() {
        return None;
    }
    if let Some(signal) = status.signal() {
        return Some(
            json!({"kind": "interrupted", "message": format!("the run was killed by signal {signal}")}),
        );
    }
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("[info]") && !l.starts_with("Node.js v"))
        .collect();
    let mut error = json!({"kind": "exception"});
    error["message"] = lines[lines.len().saturating_sub(ERROR_LINES)..]
        .join("\n")
        .into();
    Some(error)
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

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn a_run_ends_when_main_returns_though_a_timer_or_server_stays_open() {
        let Some(node) = std::env::var_os("NODE_BIN") else {
            eprintln!("SKIP: no Node; set NODE_BIN to run");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let scripts = Scripts::new(node.into(), dir.path().into(), "/run/t.sock".into(), None);
        for code in [
            "setInterval(() => {}, 1000)\nexport function main(p) { console.log('tick'); return p.n }",
            "import { createServer } from 'node:net'\ncreateServer().listen(0)\nexport async function main(p) { console.log('tick'); return p.n }",
        ] {
            let started = Instant::now();
            let report = scripts
                .run_script(
                    &scripts.bin,
                    Script::Code(code.to_owned()),
                    r#"{"n":7}"#,
                    dir.path(),
                    Duration::from_secs(20),
                    "run",
                )
                .await
                .unwrap();
            assert!(started.elapsed() < Duration::from_secs(5), "{report}");
            assert_eq!(report["result"], 7, "{report}");
            assert_eq!(report["logs"], "tick", "{report}");
            assert_eq!(report["exit_code"], 0, "{report}");
            assert!(report.get("error").is_none(), "{report}");
        }
    }

    #[tokio::test]
    async fn a_script_reaches_the_toolhost_and_the_flows_of_its_workspace() {
        let Some(node) = std::env::var_os("NODE_BIN") else {
            eprintln!("SKIP: no Node; set NODE_BIN to run");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let scripts = Scripts::new(node.into(), dir.path().into(), "/run/t.sock".into(), None);
        let report = scripts
            .run_script(
                &scripts.bin,
                Script::Code("export function main() { return [process.env.TOOLHOST_SOCKET, process.env.SHUTTLE_FLOWS] }".to_owned()),
                "{}",
                dir.path(),
                Duration::from_secs(20),
                "run",
            )
            .await
            .unwrap();
        assert_eq!(report["result"][0], "/run/t.sock", "{report}");
        assert_eq!(
            report["result"][1],
            dir.path().join("flows").to_string_lossy().as_ref(),
            "{report}"
        );
    }

    #[tokio::test]
    async fn a_script_decision_reaches_gw_with_the_key_order_the_script_wrote() {
        let Some(node) = std::env::var_os("NODE_BIN") else {
            eprintln!("SKIP: no Node; set NODE_BIN to run");
            return;
        };
        let gw = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", gw.local_addr().unwrap());
        let (sent, mut got) = tokio::sync::mpsc::channel::<String>(1);
        tokio::spawn(async move {
            let (mut conn, _) = gw.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let body = loop {
                let n = conn.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if body.len() >= len {
                        break body.to_owned();
                    }
                }
            };
            sent.send(body).await.unwrap();
            let reply = json!({"id": "g1", "model": "typesafe/jev-1.14-20261014", "usage": {},
                "answers": {"pick": {"type": "choice", "choice": "zeta", "probabilities": {"zeta": 0.7, "alpha": 0.2, "mid": 0.1}},
                    "on": {"type": "noul", "noul": 0.6}}})
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            conn.write_all(resp.as_bytes()).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("toolhost.sock");
        let cfg = crate::config::ToolhostConfig {
            socket: socket.clone(),
            socket_group: "no-such-group".into(),
            workspace: dir.path().into(),
            chrome_bin: "false".into(),
            chrome_profile: dir.path().join("chrome"),
            cdp_port: 1,
            cua_socket: dir.path().join("cua.sock"),
            script_bin: node.clone().into(),
            session_save: dir.path().join("session-save"),
            home: dir.path().into(),
            automation: crate::config::Automation::Desk,
            plugins: Vec::new(),
            adb_bin: "adb".into(),
            gw_base_url: Some(base),
            shuttle_socket: None,
        };
        tokio::spawn(crate::toolhost::run(cfg));
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let scripts = Scripts::new(node.into(), dir.path().into(), socket, None);
        let code = r"
import { createConnection } from 'node:net'
export function main() {
  const questions = { pick: { type: 'choice', instructions: 'Which?', criteria: { zeta: 'z', alpha: 'a', mid: 'm' } }, on: { type: 'noul', instructions: 'On?' } }
  const body = Buffer.from(JSON.stringify({ agent_id: 'desk', tool: 'model_decide', args: { state: { zz: 1, aa: 2 }, questions } }))
  const head = Buffer.alloc(4)
  head.writeUInt32BE(body.length)
  return new Promise((resolve, reject) => {
    const s = createConnection(process.env.TOOLHOST_SOCKET)
    let got = Buffer.alloc(0)
    s.on('connect', () => s.write(Buffer.concat([head, body])))
    s.on('data', (d) => {
      got = Buffer.concat([got, d])
      if (got.length >= 4 && got.length >= 4 + got.readUInt32BE(0)) { s.destroy(); resolve(JSON.parse(got.subarray(4).toString())) }
    })
    s.on('error', reject)
  })
}";
        let report = scripts
            .run_script(
                &scripts.bin,
                Script::Code(code.to_owned()),
                "{}",
                dir.path(),
                Duration::from_secs(20),
                "run",
            )
            .await
            .unwrap();
        let body = got.recv().await.unwrap();
        let at = |s: &str| body.find(s).unwrap_or_else(|| panic!("{s} in {body}"));
        assert!(at("\"zz\"") < at("\"aa\""), "{body}");
        assert!(at("\"pick\"") < at("\"on\""), "{body}");
        assert!(
            at("\"zeta\"") < at("\"alpha\"") && at("\"alpha\"") < at("\"mid\""),
            "{body}"
        );
        let out: Value = serde_json::from_str(report["result"]["text"].as_str().unwrap()).unwrap();
        assert_eq!(report["result"]["is_error"], false, "{report}");
        assert_eq!(out["answers"]["pick"]["choice"], "zeta", "{out}");
        assert_eq!(
            out["served"]["build"], "typesafe/jev-1.14-20261014",
            "{out}"
        );
        assert_eq!(
            out["answers"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["pick", "on"],
            "{out}"
        );
    }

    #[test]
    fn process_group_reads_past_a_command_name_with_spaces_and_parens() {
        assert_eq!(
            process_group("4242 (node (x) y) S 1 777 777 0 -1"),
            Some(777)
        );
        assert_eq!(process_group("garbage"), None);
    }

    #[test]
    fn a_run_spares_only_the_apps_in_app_scopes_of_their_own() {
        let own = "0::/system.slice/cocoonbox-toolhost.service\n";
        let app =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/x-app-mousepad-1.scope\n";
        let session =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-term-1.scope\n";
        assert!(launched(own, app));
        assert!(!launched(own, own));
        assert!(!launched(session, session));
        assert!(launched(session, app));
        assert!(!launched(
            own,
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/cua-driver.service\n"
        ));
    }

    #[test]
    fn failure_keeps_the_tail_of_stderr_without_info_lines() {
        let stderr = "[info] Opening app Mousepad\nfile:///tmp/s.mts:20\nError: Instance has no visible top-level window\n    at x\n\nNode.js v24.21.0";
        let error = failure(ExitStatus::from_raw(1 << 8), stderr).unwrap();
        assert_eq!(error["kind"], "exception");
        assert_eq!(
            error["message"],
            "file:///tmp/s.mts:20\nError: Instance has no visible top-level window\n    at x"
        );
        assert_eq!(
            failure(ExitStatus::from_raw(9), "").unwrap()["kind"],
            "interrupted"
        );
        assert!(failure(ExitStatus::from_raw(0), "").is_none());
    }
}
