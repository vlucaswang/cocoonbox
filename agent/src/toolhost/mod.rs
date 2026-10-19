//! The tool host: runs every tool as the desktop user behind a unix socket.

mod android;
mod android_desk;
mod atspi;
mod browser;
mod cua;
pub mod desk;
mod exec;
pub(crate) mod files;
pub(crate) mod flows;
mod ground;
mod model;
pub mod rpc;
mod scope;
mod script;
mod shell;
pub mod state;
mod timing;
mod volatile;
mod xshm;
mod xtest;

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, watch};
use x11rb::connection::Connection as _;

use crate::config::{Automation, ToolhostConfig};
use android::Android;
use browser::Browser;
use cua::Cua;
use desk::Desk;
use model::Models;
use rpc::{Call, Client, Output};
use script::{Scripts, ShuttleScreen};
use shell::Shell;

const SOCKET_MODE: u32 = 0o660;
const SCRATCH_DIR: &str = "/tmp/cocoonbox-shell";
const SCRIPT_DIR: &str = "/tmp/cocoonbox-scripts";
const OS_RELEASE: &str = "/etc/os-release";
const DESKTOP_VAR: &str = "XDG_CURRENT_DESKTOP";

/// Shared tool state for all connections.
struct Host {
    workspace: PathBuf,
    scratch: PathBuf,
    shells: Mutex<HashMap<String, Arc<Mutex<Option<Shell>>>>>,
    browser: Browser,
    cua: Cua,
    /// The screen tools of an Android box, in place of cua-driver.
    android: Option<Android>,
    desk: Desk,
    models: Models,
    scripts: Scripts,
    /// The program of each plugin script tool, on a box that runs GUI plans.
    plugins: HashMap<String, PathBuf>,
    session_save: PathBuf,
    home: PathBuf,
    /// A box that runs GUI plans.
    plans: bool,
    /// The saved flows of a box that runs GUI plans, which a read-only share does not read.
    hidden: Option<PathBuf>,
    flow_writes: watch::Sender<u64>,
    /// shuttle's daemon, which answers the desk verbs, model calls and runs of a box that runs it.
    shuttle: Option<Client>,
}

impl Host {
    /// Creates a host from its configuration.
    fn new(cfg: &ToolhostConfig) -> Self {
        let shuttle = cfg.shuttle_socket.clone().map(Client::new);
        Self {
            workspace: cfg.workspace.clone(),
            scratch: PathBuf::from(SCRATCH_DIR),
            shells: Mutex::new(HashMap::new()),
            browser: Browser::new(
                cfg.chrome_bin.clone(),
                cfg.chrome_profile.clone(),
                cfg.cdp_port,
                cfg.automation.plans(),
            ),
            cua: Cua::new(cfg.cua_socket.clone(), cfg.automation.plans()),
            android: (cfg.automation == Automation::Android)
                .then(|| Android::new(cfg.adb_bin.clone())),
            desk: Desk::new(shuttle.clone()),
            models: Models::new(cfg.gw_base_url.as_deref()),
            scripts: Scripts::new(
                cfg.script_bin.clone(),
                PathBuf::from(SCRIPT_DIR),
                cfg.socket.clone(),
                shuttle.clone(),
            ),
            plugins: cfg
                .plugins
                .iter()
                .filter(|_| cfg.automation.plans())
                .map(|p| (p.name.clone(), p.bin.clone()))
                .collect(),
            session_save: cfg.session_save.clone(),
            home: cfg.home.clone(),
            plans: cfg.automation.plans(),
            hidden: cfg.automation.scripts().then(|| flows::dir(&cfg.workspace)),
            flow_writes: watch::channel(0).0,
            shuttle,
        }
    }

    /// Executes one call; failures become error outputs for the model.
    async fn dispatch(&self, call: Call) -> Output {
        let call = match self.android_call(call).await {
            Ok(result) => {
                let out = result.unwrap_or_else(Output::error);
                timing::note("error", out.is_error);
                return out;
            }
            Err(call) => call,
        };
        let call = match self.relay(call).await {
            Ok(out) => {
                timing::note("error", out.is_error);
                return out;
            }
            Err(call) => call,
        };
        let result = match call.tool.as_str() {
            "shell" => {
                let command = call.args["command"].as_str().unwrap_or_default();
                let note = self.held(|name| files::writes(command, name));
                self.shell(&call.agent_id, call.args)
                    .await
                    .map(|out| Output::text(out + &note))
            }
            "shell_close" => self.close_shell(&call.agent_id).await.map(Output::text),
            "read_file" => files::read(&self.workspace, self.hidden.as_deref(), call.args)
                .await
                .map(Output::text),
            "write_file" => {
                let path = call.args["path"].as_str().unwrap_or_default();
                let note = self.held(|name| files::names(path, name));
                files::write(&self.workspace, call.args)
                    .await
                    .map(|out| Output::text(out + &note))
            }
            "edit_file" => {
                let path = call.args["path"].as_str().unwrap_or_default();
                let note = self.held(|name| files::names(path, name));
                files::edit(&self.workspace, call.args)
                    .await
                    .map(|out| Output::text(out + &note))
            }
            "list_dir" => files::list(&self.workspace, self.hidden.as_deref(), call.args)
                .await
                .map(Output::text),
            "upload" => self.upload(call.args).await.map(Output::text),
            "state_walk" => state::walk(call.args).await.map(Output::text),
            "state_apply" => state::apply(call.args).await.map(Output::text),
            "state_swap" => state::swap(call.args).await.map(Output::text),
            "state_place" => state::place(call.args).await.map(Output::text),
            "desktop_save" => self.desktop_save().await.map(Output::text),
            "desktop_info" => Ok(Output::text(desktop_info())),
            "active_origin" => self.browser.origin().await.map(Output::text),
            "credential_fill" => {
                let mut args = call.args;
                let out = self.browser.fill(&args).await.map(Output::text);
                rpc::wipe(&mut args);
                out
            }
            t if t.starts_with("browser_") => self
                .browser
                .run(&call.agent_id, t, call.args)
                .await
                .map(Output::text),
            "computer_state" => self.cua.state(call.args).await,
            "computer_act" if call.args.get("target").is_some_and(|t| !t.is_null()) => {
                ground::act(
                    &self.cua,
                    &self.desk,
                    &self.scripts,
                    &self.workspace,
                    call.args,
                )
                .await
            }
            "computer_act" => self.cua.act(&self.browser, &call.agent_id, call.args).await,
            "computer_end" => self.cua.end(call.args).await.map(Output::text),
            t if t.starts_with("desk_") => self.desk.run(&self.cua, t, call.args).await,
            t if t.starts_with("model_") => self.models.run(t, call.args).await.map(Output::text),
            "script_run" => {
                self.scripts
                    .run(&self.cua, &self.workspace, call.args)
                    .await
            }
            "gui_plan" => {
                self.scripts
                    .plan(&self.cua, &self.workspace, call.args)
                    .await
            }
            "gui_flows" => Ok(Output::text(Scripts::flows(&self.workspace).await)),
            "desk" => {
                let op = call.args["op"].as_str().unwrap_or_default().to_owned();
                self.desk
                    .run(&self.cua, &format!("desk_{op}"), call.args)
                    .await
            }
            "flows_scan" => flows::scan(&self.workspace).await.map(Output::text),
            "flows_read" => flows::read(&self.workspace, call.args)
                .await
                .map(Output::text),
            "flows_write" => flows::write(&self.workspace, call.args)
                .await
                .map(Output::text),
            "flows_remove" => flows::remove(&self.workspace, call.args)
                .await
                .map(Output::text),
            "flows_mark" => flows::mark(&self.workspace, call.args)
                .await
                .map(Output::text),
            "flows_index" => self.scripts.reindex(&self.workspace).await,
            "flows_wait" => flows::wait(&self.flow_writes, call.args)
                .await
                .map(Output::text),
            "screenshot" => self.cua.look(&call.args).await,
            other => match self.plugins.get(other) {
                Some(bin) => {
                    self.scripts
                        .run_with(other, bin, &self.cua, &self.workspace, call.args)
                        .await
                }
                None => Err(format!("unknown tool {other}")),
            },
        };
        let out = result.unwrap_or_else(Output::error);
        timing::note("error", out.is_error);
        out
    }

    /// Runs the screen, desk and script tools of an Android box; on a box that runs shuttle, every call but a run comes back, its screen tools under shuttle's names.
    async fn android_call(&self, mut call: Call) -> Result<Result<Output, String>, Call> {
        let Some(android) = &self.android else {
            return Err(call);
        };
        if let Some(shuttle) = &self.shuttle {
            let screen = ShuttleScreen(shuttle);
            return Ok(match call.tool.as_str() {
                "script_run" => self.scripts.run(&screen, &self.workspace, call.args).await,
                "gui_plan" => self.scripts.plan(&screen, &self.workspace, call.args).await,
                _ => {
                    if let Some(tool) = shuttle_tool(&call.tool) {
                        call.tool = tool;
                    }
                    return Err(call);
                }
            });
        }
        Ok(match call.tool.as_str() {
            "screenshot" => android.look().await,
            "computer_state" => android.state(call.args).await,
            "computer_act" => android.act(call.args).await,
            "computer_end" => android.end(&call.args).await.map(Output::text),
            t if t.starts_with("desk_") => android_desk::run(android, t, call.args).await,
            "script_run" => self.scripts.run(android, &self.workspace, call.args).await,
            "gui_plan" => self.scripts.plan(android, &self.workspace, call.args).await,
            _ => return Err(call),
        })
    }

    /// Sends a desk verb or model call as it came to shuttle, on a box that runs it; every other call comes back.
    async fn relay(&self, mut call: Call) -> Result<Output, Call> {
        let Some(shuttle) = &self.shuttle else {
            return Err(call);
        };
        if call.tool == "desk" {
            call.tool = format!("desk_{}", call.args["op"].as_str().unwrap_or_default());
        }
        if !(call.tool.starts_with("desk_") || call.tool.starts_with("model_")) {
            return Err(call);
        }
        Ok(shuttle
            .desk(&call, rpc::DESK_DEADLINE)
            .await
            .unwrap_or_else(Output::error))
    }

    async fn shell(&self, agent_id: &str, args: Value) -> Result<String, String> {
        let command = args["command"]
            .as_str()
            .ok_or("shell needs a command string")?;
        let timeout = args["timeout_s"].as_u64().map(Duration::from_secs);
        let slot = {
            let mut shells = self.shells.lock().await;
            Arc::clone(shells.entry(agent_id.to_owned()).or_default())
        };
        let mut slot = slot.lock().await;
        if slot.as_ref().is_none_or(Shell::is_dead) {
            tokio::fs::create_dir_all(&self.scratch)
                .await
                .map_err(|e| format!("create {}: {e}", self.scratch.display()))?;
            let _ =
                tokio::fs::set_permissions(&self.scratch, std::fs::Permissions::from_mode(0o700))
                    .await;
            *slot = Some(Shell::spawn(&self.workspace, self.script(agent_id)).await?);
        }
        let shell = slot.as_mut().ok_or("shell missing")?;
        let out = shell.run(command, timeout).await?;
        Ok(if self.plans {
            out + &shell::capped_note(timeout)
        } else {
            out
        })
    }

    /// With plans, a note on the files that a call `changes` and an app window has open (see [`files::held_note`]).
    fn held(&self, changes: impl Fn(&str) -> bool) -> String {
        files::held_note(&self.cua.window_titles(), changes)
    }

    async fn close_shell(&self, agent_id: &str) -> Result<String, String> {
        let slot = self.shells.lock().await.remove(agent_id);
        if let Some(slot) = slot
            && let Some(mut shell) = slot.lock().await.take()
        {
            shell.kill();
        }
        let _ = tokio::fs::remove_file(self.script(agent_id)).await;
        Ok("closed".to_owned())
    }

    fn script(&self, agent_id: &str) -> PathBuf {
        self.scratch.join(format!("{}.sh", safe_name(agent_id)))
    }

    async fn desktop_save(&self) -> Result<String, String> {
        let out = tokio::process::Command::new(&self.session_save)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("run {}: {e}", self.session_save.display()))?;
        if out.status.success() {
            Ok("saved".to_owned())
        } else {
            Err(format!(
                "{}: {}: {}",
                self.session_save.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    async fn upload(&self, args: Value) -> Result<String, String> {
        let path = files::resolve(
            &self.workspace,
            args["path"].as_str().ok_or("upload needs path")?,
        );
        let data = B64
            .decode(args["data_b64"].as_str().unwrap_or_default())
            .map_err(|e| format!("decode upload: {e}"))?;
        files::write_bytes(&path, &data).await?;
        Ok(path.display().to_string())
    }
}

/// Runs the toolhost until SIGTERM or SIGINT, which closes Chrome with its agent windows first.
pub async fn run(cfg: ToolhostConfig) -> Result<(), String> {
    let listener = bind(&cfg.socket, &cfg.socket_group)?;
    eprintln!("toolhost listening on {}", cfg.socket.display());
    let host = Arc::new(Host::new(&cfg));
    if cfg.automation.plans() {
        let (workspace, writes) = (cfg.workspace.clone(), host.flow_writes.clone());
        // detached: lives as long as the toolhost
        tokio::spawn(async move {
            if let Err(e) = flows::watch(workspace, writes).await {
                eprintln!("toolhost: watch the flows: {e}");
            }
        });
    }
    let stop = crate::server::shutdown_signal();
    tokio::pin!(stop);
    loop {
        tokio::select! {
            () = &mut stop => {
                let closed = host.browser.run("toolhost", "browser_close", Value::Null).await;
                eprintln!("toolhost stopping: browser_close: {closed:?}");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(|e| format!("accept: {e}"))?;
                tokio::spawn(serve_conn(Arc::clone(&host), stream));
            }
        }
    }
}

/// Serves one request; the client closing its end cancels the tool.
async fn serve_conn(host: Arc<Host>, stream: UnixStream) {
    let (mut rd, mut wr) = stream.into_split();
    let call: Call = match rpc::read_frame(&mut rd).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("toolhost: bad request: {e}");
            return;
        }
    };
    match call.tool.as_str() {
        "state_read" => {
            if let Err(e) = state::read(call.args, &mut wr).await {
                eprintln!("toolhost: state_read: {e}");
            }
            return;
        }
        "exec" => {
            exec::run(&host.workspace, &host.home, &call.args, rd, wr).await;
            return;
        }
        "state_write" => {
            let out = state::write(call.args, &mut rd).await;
            if let Err(e) = rpc::write_frame(&mut wr, &out).await {
                eprintln!("toolhost: reply: {e}");
            }
            return;
        }
        _ => {}
    }
    let mut probe = [0u8; 1];
    let tool = call.tool.clone();
    let out = tokio::select! {
        out = timing::scope(&tool, host.dispatch(call)) => out,
        _ = rd.read(&mut probe) => return,
    };
    if let Err(e) = rpc::write_frame(&mut wr, &out).await {
        eprintln!("toolhost: reply: {e}");
    }
}

/// Binds a unix socket at `path`, mode 0660, owned by `group` when it exists.
pub(crate) fn bind(path: &Path, group: &str) -> Result<UnixListener, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| format!("bind {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    if let Some(gid) = group_id(group) {
        std::os::unix::fs::chown(path, None, Some(gid))
            .map_err(|e| format!("chgrp {} {group}: {e}", path.display()))?;
    }
    Ok(listener)
}

fn group_id(name: &str) -> Option<u32> {
    let groups = std::fs::read_to_string("/etc/group").ok()?;
    groups.lines().find_map(|line| {
        let mut f = line.split(':');
        (f.next()? == name).then_some(())?;
        f.nth(1)?.parse().ok()
    })
}

/// The box's OS release, desktop and X screen size as JSON, with the fields it could read.
fn desktop_info() -> String {
    let mut info = json!({});
    if let Some(os) = std::fs::read_to_string(OS_RELEASE)
        .ok()
        .and_then(|r| os_name(&r))
    {
        info["os"] = os.into();
    }
    let desktop = std::env::var(DESKTOP_VAR).ok().or_else(session_desktop);
    if let Some(desktop) = desktop.as_deref().and_then(desktop_name) {
        info["desktop"] = desktop.into();
    }
    if let Ok((conn, n)) = x11rb::connect(None) {
        let screen = &conn.setup().roots[n];
        info["width"] = screen.width_in_pixels.into();
        info["height"] = screen.height_in_pixels.into();
    }
    info.to_string()
}

/// `NAME VERSION_ID` of an os-release file, else its `PRETTY_NAME`.
fn os_name(release: &str) -> Option<String> {
    let field = |key: &str| {
        release
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"'))
            .filter(|v| !v.is_empty())
    };
    match (field("NAME"), field("VERSION_ID")) {
        (Some(name), Some(version)) => Some(format!("{name} {version}")),
        _ => field("PRETTY_NAME").map(str::to_owned),
    }
}

/// The desktop of a session such as `ubuntu:GNOME` in `XDG_CURRENT_DESKTOP`: its last name.
fn desktop_name(value: &str) -> Option<&str> {
    value.rsplit(':').next().filter(|d| !d.is_empty())
}

/// `XDG_CURRENT_DESKTOP` of a process of this user, such as the desktop session's.
fn session_desktop() -> Option<String> {
    let prefix = format!("{DESKTOP_VAR}=");
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let environ = std::fs::read(entry.path().join("environ")).ok()?;
            environ
                .split(|b| *b == 0)
                .find_map(|var| var.strip_prefix(prefix.as_bytes()))
                .map(|v| String::from_utf8_lossy(v).into_owned())
        })
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// shuttle's name for an Android screen tool: `screenshot` is `desk_look`, `computer_X` is `desk_computer_X`.
fn shuttle_tool(tool: &str) -> Option<String> {
    match tool {
        "screenshot" => Some("desk_look".to_owned()),
        "computer_state" | "computer_act" | "computer_end" => Some(format!("desk_{tool}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_os_is_its_name_and_version_and_the_desktop_its_last_name() {
        let ubuntu = "PRETTY_NAME=\"Ubuntu 22.04.5 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"22.04\"\nID=ubuntu\n";
        assert_eq!(os_name(ubuntu).as_deref(), Some("Ubuntu 22.04"));
        assert_eq!(
            os_name("PRETTY_NAME=\"Debian GNU/Linux trixie/sid\"\nNAME=\"Debian GNU/Linux\"\n")
                .as_deref(),
            Some("Debian GNU/Linux trixie/sid")
        );
        assert_eq!(os_name("ID=x\n"), None);
        assert_eq!(desktop_name("ubuntu:GNOME"), Some("GNOME"));
        assert_eq!(desktop_name("XFCE"), Some("XFCE"));
        assert_eq!(desktop_name(""), None);
    }

    const SHUTTLE_ANDROID: [&str; 19] = [
        "desk_windows",
        "desk_wait",
        "desk_snapshot",
        "desk_find",
        "desk_read",
        "desk_press",
        "desk_act",
        "desk_at",
        "desk_key",
        "desk_type",
        "desk_click",
        "desk_focus",
        "desk_close",
        "desk_launch",
        "desk_screen",
        "desk_shot",
        "desk_computer_state",
        "desk_computer_act",
        "desk_computer_end",
    ];

    #[tokio::test]
    async fn an_android_box_with_shuttle_sends_its_screen_desk_and_run_calls_there_and_runs_no_adb()
    {
        let dir = tempfile::tempdir().unwrap();
        let ran = dir.path().join("adb-ran");
        let adb = dir.path().join("adb");
        std::fs::write(
            &adb,
            format!("#!/bin/sh\necho \"$@\" >> {}\n", ran.display()),
        )
        .unwrap();
        std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
        let desk = dir.path().join("desk.sock");
        let (seen, mut tools) = tokio::sync::mpsc::unbounded_channel::<String>();
        let listener = UnixListener::bind(&desk).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let call: Call = rpc::read_frame(&mut conn).await.unwrap();
                let out = match call.tool.as_str() {
                    "desk_look" => Output {
                        image_b64: Some("aW1n".into()),
                        ..Output::text("720x1280 screen")
                    },
                    "gui_plan" => Output::text(r#"{"ok":true}"#),
                    "script_run" => Output::text(r#"{"result":null,"logs":"","duration_ms":1}"#),
                    tool if SHUTTLE_ANDROID.contains(&tool) => Output::text("{}"),
                    tool => Output::error(format!("unknown desk tool {tool}")),
                };
                seen.send(call.tool).unwrap();
                rpc::write_frame(&mut conn, &out).await.unwrap();
            }
        });
        let socket = dir.path().join("toolhost.sock");
        let cfg = ToolhostConfig {
            socket: socket.clone(),
            socket_group: "no-such-group".into(),
            workspace: dir.path().into(),
            chrome_bin: "false".into(),
            chrome_profile: dir.path().join("chrome"),
            cdp_port: 1,
            cua_socket: dir.path().join("cua.sock"),
            script_bin: "false".into(),
            session_save: dir.path().join("session-save"),
            home: dir.path().into(),
            automation: Automation::Android,
            plugins: Vec::new(),
            adb_bin: adb,
            gw_base_url: None,
            shuttle_socket: Some(desk),
        };
        tokio::spawn(run(cfg));
        while !socket.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let toolhost = Client::new(socket);
        let calls = [
            ("screenshot", json!({})),
            ("computer_state", json!({"session": "cu"})),
            (
                "computer_act",
                json!({"session": "cu", "action": "tap", "x": 1, "y": 2}),
            ),
            ("computer_end", json!({"session": "cu"})),
            ("desk_type", json!({"text": "你好"})),
            ("desk", json!({"op": "windows"})),
            (
                "gui_plan",
                json!({"steps": [{"app": "x"}], "screenshot": false}),
            ),
            ("script_run", json!({"code": "export function main() {}"})),
            (
                "gui_plan",
                json!({"steps": [{"app": "x"}], "screenshot": true}),
            ),
        ];
        for (tool, args) in calls {
            let shot = args["screenshot"] != false && matches!(tool, "script_run" | "gui_plan");
            let out = toolhost
                .call(&Call {
                    agent_id: "ag".into(),
                    tool: tool.into(),
                    args,
                })
                .await
                .unwrap();
            assert!(!out.is_error, "{tool}: {}", out.text);
            if shot {
                assert_eq!(out.image_b64.as_deref(), Some("aW1n"), "{tool}");
            }
        }
        let mut got = Vec::new();
        while let Ok(tool) = tools.try_recv() {
            got.push(tool);
        }
        assert_eq!(
            got,
            [
                "desk_look",
                "desk_computer_state",
                "desk_computer_act",
                "desk_computer_end",
                "desk_type",
                "desk_windows",
                "gui_plan",
                "script_run",
                "desk_look",
                "gui_plan",
                "desk_look"
            ]
        );
        assert!(
            !ran.exists(),
            "the toolhost ran adb: {:?}",
            std::fs::read_to_string(&ran)
        );
    }
}
