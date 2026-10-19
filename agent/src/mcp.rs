//! MCP Streamable HTTP server at `/mcp`: external agents drive the box through the toolhost (mode B).
//! The owner's sessions run untracked; a share's sessions are agents `mcp:<client>@<share>` with a transcript.

use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tonic::Status;

use crate::config::Automation;
use crate::dispatch::{Caller, ToolOutput, uses_driver};
use crate::identity::{CREDENTIAL_HEADER, Denied, Gate, Principal};
use crate::llm::{b64, image_mime};
use crate::pb::{self, AgentStatus, Role, event, transcript_entry::Body as Entry};
use crate::runner::Runtime;
use crate::{lock, new_id, prompts, tools};

/// Path of the MCP endpoint on the agent listener.
pub const PATH: &str = "/mcp";

const SESSION_HEADER: &str = "mcp-session-id";
const AGENT_PREFIX: &str = "mcp:";
const PROTOCOLS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const FALLBACK_PROTOCOL: &str = "2025-06-18";
const KEEPALIVE: Duration = Duration::from_secs(25);
const MAX_REAP_PERIOD: Duration = Duration::from_secs(60);
const MAX_BODY: usize = 64 << 20;
const DRIVER_ENDED: &str = "session has ended";
const PREEMPTED: &str =
    "interrupted: the owner's save or upgrade of the box needed it; the shell session was reset";
const LIST_CHANGED: &[u8] =
    b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n";
const INSTRUCTIONS: &str = "These tools act on a cloud computer: Ubuntu 24.04 with an XFCE desktop at 1280x800. \
Relative file paths resolve under /workspace, and shell is one persistent bash session. \
Prefer shell and file tools, then browser_* (pages as ref-tagged element lists), and use computer_state / computer_act only for GUI work. \
Every computer_act returns a fresh screenshot; trust the screenshot, not the action's report. \
The user watches the screen live and sees every call.";
const ANDROID_INSTRUCTIONS: &str = "These tools act on an Android 16 phone (720x1280) that runs in a cloud computer: Ubuntu 24.04 with no desktop. \
shell is one persistent bash session on that Ubuntu, with adb on PATH already connected to the phone (adb shell, adb install, adb push/pull); \
relative file paths resolve under /workspace. \
Prefer adb commands (am start, pm, settings, content) for anything a command can do, and use computer_state / computer_act for app UI. \
gui_plan runs several app UI steps in one call (open an app by package, tap by label, set a switch or field, verify), and script_run one script for loops over many items; computer_state lists the screen's UI elements by [index]; computer_act taps, swipes, types any Unicode text into the focused field, presses keys (back, home_screen) or launches an app by package, then returns a fresh screenshot; trust the screenshot, not the action's report. \
A passing gui_plan can be saved as a flow and replayed (save, flow, flows: true). \
The phone reaches the internet only through the egress proxy, over TCP. \
The user watches the screen live and sees every call.";
const PLAN_HEAD: &str =
    "Relative file paths resolve under /workspace, and shell is one persistent bash session.";
const ALWAYS_LOAD: &[&str] = &[
    "script_run",
    "gui_plan",
    "shell",
    "browser_navigate",
    "browser_read",
    "browser_fetch",
];
const NUDGE_STREAK: u32 = 2;
const MAX_INSTRUCTIONS: usize = 1600;
const NUDGE: &str = "\n\n2 GUI steps in a row: write the rest as one gui_plan call (the whole plan, each step checked), or a script_run script for loops.";
const MAX_FLOWS: usize = 30;
const MAX_SHARE_SESSIONS: usize = 4;
const SHARE_SESSIONS_FULL: &str = "a share holds at most 4 open MCP sessions: close one first";
const DESK_RESOURCES: [Resource; 1] = [Resource {
    uri: Cow::Borrowed("cocoonbox://desk/AGENTS.md"),
    path: Cow::Borrowed("/opt/desk/AGENTS.md"),
    mime: Cow::Borrowed("text/markdown"),
    description: Cow::Borrowed(
        "How to write, run and replay desk scripts on this box, with the desk API",
    ),
}];

/// MCP sessions of external agents.
pub struct Mcp {
    rt: Arc<Runtime>,
    gate: Arc<Gate>,
    tools: Vec<(Cow<'static, str>, Value)>,
    /// The guide of the box's automation first, then its plugins' files.
    resources: Vec<Resource>,
    automation: Automation,
    idle: Duration,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// The line that names the box's OS, desktop and screen, once the toolhost has read all three.
    machine: tokio::sync::OnceCell<String>,
    routing_capped: AtomicBool,
    opening: tokio::sync::Mutex<()>,
}

impl Mcp {
    /// Creates the server behind `gate`; sessions idle for `idle` are closed.
    pub fn new(rt: Arc<Runtime>, idle: Duration, gate: Arc<Gate>) -> Arc<Self> {
        let automation = rt.cfg.automation;
        let plugins = &rt.cfg.plugins;
        let mut specs = tools::remote_tools(automation);
        tools::with_plugins(&mut specs, plugins);
        let tools = specs
            .into_iter()
            .map(|t| {
                let mut tool = json!({"name": t.name, "description": t.description});
                tool["inputSchema"] = t.parameters;
                let always = ALWAYS_LOAD.contains(&t.name.as_ref())
                    || plugins.iter().any(|p| p.name == t.name);
                if automation.scripts() && always {
                    tool["_meta"] = json!({"anthropic/alwaysLoad": true});
                }
                (t.name, tool)
            })
            .collect();
        let mut resources = match automation {
            Automation::Plain | Automation::Android => Vec::new(),
            Automation::Desk => DESK_RESOURCES.to_vec(),
        };
        if automation.plans() {
            resources.extend(plugins.iter().flat_map(|p| &p.resources).map(|r| Resource {
                uri: Cow::Owned(r.uri.clone()),
                path: Cow::Owned(r.path.display().to_string()),
                mime: Cow::Owned(r.mime.clone()),
                description: Cow::Owned(r.description.clone()),
            }));
        }
        Arc::new(Self {
            rt,
            gate,
            tools,
            resources,
            automation,
            idle,
            sessions: Mutex::new(HashMap::new()),
            machine: tokio::sync::OnceCell::new(),
            routing_capped: AtomicBool::new(false),
            opening: tokio::sync::Mutex::new(()),
        })
    }

    /// Routes `POST|GET|DELETE /mcp`.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route(PATH, post(post_mcp).get(get_mcp).delete(delete_mcp))
            .layer(DefaultBodyLimit::max(MAX_BODY))
            .with_state(self)
    }

    /// Closes idle sessions until the process exits.
    pub fn spawn_reaper(self: &Arc<Self>) {
        let mcp = Arc::clone(self);
        let period = (self.idle / 2).min(MAX_REAP_PERIOD);
        // detached: runs for the life of the process
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            loop {
                tick.tick().await;
                let idle: Vec<Arc<Session>> = {
                    let mut sessions = lock(&mcp.sessions);
                    let ids: Vec<String> = sessions
                        .iter()
                        .filter(|(_, s)| lock(&s.driver).last_seen.elapsed() >= mcp.idle)
                        .map(|(id, _)| id.clone())
                        .collect();
                    ids.iter().filter_map(|id| sessions.remove(id)).collect()
                };
                for s in idle {
                    mcp.close(&s, "idle").await;
                }
                mcp.prune().await;
            }
        });
    }

    /// Deletes closed `mcp:*` agents untouched for longer than the prune age.
    async fn prune(&self) {
        let age = i64::try_from(self.rt.cfg.mcp_prune_after.as_millis()).unwrap_or(i64::MAX);
        let before = crate::now_ms().saturating_sub(age);
        match self
            .rt
            .journal
            .store
            .prune_agents(AGENT_PREFIX.to_owned(), before)
            .await
        {
            Ok(ids) if !ids.is_empty() => {
                eprintln!("mcp: pruned {} idle agent(s)", ids.len());
                for agent_id in ids {
                    let closed = self.rt.dispatcher.close_shell(&agent_id).await;
                    if closed.is_error {
                        eprintln!("mcp: prune {agent_id}: close its shell: {}", closed.text);
                    }
                    let closed = self.rt.dispatcher.close_window(&agent_id).await;
                    if closed.is_error {
                        eprintln!("mcp: prune {agent_id}: close its window: {}", closed.text);
                    }
                    self.rt
                        .journal
                        .bus
                        .publish(event::Body::AgentDeleted(pb::AgentDeleted { agent_id }));
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("mcp: prune: {e}"),
        }
    }

    /// Whether an open session drives agent `agent_id`.
    pub fn serves(&self, agent_id: &str) -> bool {
        lock(&self.sessions)
            .values()
            .any(|s| s.agent_id() == Some(agent_id))
    }

    /// Closes the session that drives agent `agent_id`.
    pub async fn end_agent(&self, agent_id: &str) -> Result<(), Status> {
        let session = {
            let mut sessions = lock(&self.sessions);
            let id = sessions
                .iter()
                .find(|(_, s)| s.agent_id() == Some(agent_id))
                .map(|(id, _)| id.clone());
            id.and_then(|id| sessions.remove(&id))
        };
        let session = session.ok_or_else(|| {
            Status::not_found(format!("no open MCP session drives agent {agent_id}"))
        })?;
        self.close(&session, "ended by the owner").await;
        Ok(())
    }

    /// Closes every session opened through share `share_id`.
    pub async fn end_share(&self, share_id: &str) {
        let ended: Vec<Arc<Session>> = {
            let mut sessions = lock(&self.sessions);
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| s.guest.as_ref().is_some_and(|g| g.share == share_id))
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        for s in ended {
            self.close(&s, "the share ended").await;
        }
    }

    /// Closes every session; used when the agent shuts down.
    pub async fn close_all(&self) {
        let sessions: Vec<Arc<Session>> = lock(&self.sessions).drain().map(|(_, s)| s).collect();
        for s in sessions {
            self.close(&s, "agent shutting down").await;
        }
    }

    fn admit(&self, headers: &HeaderMap) -> Result<Principal, StatusCode> {
        let verdict = self
            .gate
            .check(headers.get(CREDENTIAL_HEADER).and_then(|v| v.to_str().ok()));
        verdict.map_err(|d| match d {
            Denied::Unprovisioned(_) => StatusCode::SERVICE_UNAVAILABLE,
            Denied::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
        })
    }

    /// Finds the session a request names; a session opened by another principal is as unknown as a missing one.
    fn session(
        &self,
        headers: &HeaderMap,
        principal: &Principal,
    ) -> Result<Arc<Session>, (StatusCode, i64, &'static str)> {
        let Some(id) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
            return Err((StatusCode::BAD_REQUEST, -32000, "missing Mcp-Session-Id"));
        };
        let session = lock(&self.sessions).get(id).cloned();
        let session = session.filter(|s| s.opened_by(principal)).ok_or((
            StatusCode::NOT_FOUND,
            -32001,
            "unknown or expired session",
        ))?;
        lock(&session.driver).last_seen = Instant::now();
        session.observe(principal);
        Ok(session)
    }

    async fn open(
        &self,
        params: &Value,
        principal: &Principal,
    ) -> Result<(Arc<Session>, Value), (StatusCode, i64, String)> {
        let _opening = match principal {
            Principal::Owner => None,
            Principal::Share { .. } => {
                let opening = self.opening.lock().await;
                let open = lock(&self.sessions)
                    .values()
                    .filter(|s| s.opened_by(principal))
                    .count();
                if open >= MAX_SHARE_SESSIONS {
                    return Err((
                        StatusCode::TOO_MANY_REQUESTS,
                        -32000,
                        SHARE_SESSIONS_FULL.to_owned(),
                    ));
                }
                Some(opening)
            }
        };
        let client = params["clientInfo"]["name"].as_str().unwrap_or("client");
        let version = params["clientInfo"]["version"].as_str().unwrap_or_default();
        let guest = match principal {
            Principal::Owner => None,
            Principal::Share { id, user, .. } => Some(Guest {
                share: id.clone(),
                user: user.clone(),
            }),
        };
        let record = match &guest {
            None => None,
            Some(g) => {
                let mut name = format!("{AGENT_PREFIX}{client}@{}", g.share);
                if let Some(user) = &g.user {
                    name = format!("{name}:{user}");
                }
                let agent = self
                    .rt
                    .create_agent(pb::CreateAgentRequest {
                        name,
                        main_model: "external".to_owned(),
                        client_nonce: String::new(),
                    })
                    .await
                    .map_err(|e| internal(e.message().to_owned()))?;
                let turn_id = self
                    .rt
                    .journal
                    .store
                    .open_turn(agent.id.clone())
                    .await
                    .map_err(internal)?;
                Some(Record {
                    agent_id: agent.id,
                    turn_id,
                    blocks: AtomicU32::new(0),
                })
            }
        };
        let session = Arc::new(Session {
            id: new_id("mcp"),
            guest,
            record,
            driver: Mutex::new(Driver::default()),
            listed: Mutex::new(None),
            changed: Notify::new(),
            streak: AtomicU32::new(0),
        });
        if let Some(r) = &session.record {
            let note = format!("MCP session opened by {client} {version}");
            self.note(&session, note.trim_end()).await;
            self.rt
                .journal
                .set_status(&r.agent_id, AgentStatus::Running)
                .await;
        }
        lock(&self.sessions).insert(session.id.clone(), Arc::clone(&session));
        let requested = params["protocolVersion"].as_str().unwrap_or_default();
        let protocol = PROTOCOLS
            .iter()
            .find(|p| **p == requested)
            .copied()
            .unwrap_or(FALLBACK_PROTOCOL);
        let mut result = json!({
            "protocolVersion": protocol,
            "capabilities": {"tools": {"listChanged": session.guest.is_some()}},
            "serverInfo": {"name": "cocoonbox-agent", "version": env!("CB_AGENT_VERSION")},
            "instructions": if self.automation == Automation::Android { ANDROID_INSTRUCTIONS } else { INSTRUCTIONS },
        });
        if self.automation.plans() {
            result["capabilities"]["resources"] = json!({});
            let machine = self.machine().await;
            let head = format!("{machine} {PLAN_HEAD}\n\n{}", prompts::ROUTING);
            let (text, skipped) = with_routing(head, &self.rt.cfg.routing_extra);
            if !skipped.is_empty() && !self.routing_capped.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "mcp: initialize leaves out {skipped:?} of routing.d, past the {MAX_INSTRUCTIONS}-character cap"
                );
            }
            result["instructions"] = text.into();
        }
        Ok((session, result))
    }

    /// The box's OS, desktop and screen in one line, as the toolhost reads them from the box.
    async fn machine(&self) -> String {
        if let Some(line) = self.machine.get() {
            return line.clone();
        }
        let out = self
            .rt
            .dispatcher
            .run("", None, "desktop_info", json!({}))
            .await;
        let info: Value = serde_json::from_str(&out.text).unwrap_or_default();
        let line = machine_line(&info);
        if info["desktop"].is_string() && info["width"].is_u64() {
            let _ = self.machine.set(line.clone());
        }
        line
    }

    async fn close(&self, s: &Session, reason: &str) {
        self.end_driver(s, reason).await;
        let closed = self.rt.dispatcher.close_window(s.owner()).await;
        if closed.is_error {
            eprintln!("mcp: close the window of {}: {}", s.owner(), closed.text);
        }
        let Some(r) = &s.record else {
            let closed = self.rt.dispatcher.close_shell(s.owner()).await;
            if closed.is_error {
                eprintln!("mcp: close the shell of {}: {}", s.id, closed.text);
            }
            return;
        };
        self.note(s, &format!("MCP session closed: {reason}")).await;
        let end = Entry::TurnEnd(pb::TurnEnd {
            ok: true,
            error: String::new(),
            usage: None,
        });
        self.record(s, Role::System, end).await;
        if let Err(e) = self.rt.journal.store.finish_turn(r.turn_id.clone()).await {
            eprintln!("mcp: finish turn {}: {e}", r.turn_id);
        }
        self.rt
            .journal
            .set_status(&r.agent_id, AgentStatus::Idle)
            .await;
    }

    async fn end_driver(&self, s: &Session, reason: &str) {
        let label = {
            let mut d = lock(&s.driver);
            if !d.used {
                return;
            }
            d.used = false;
            std::mem::replace(&mut d.label, new_id("cu"))
        };
        eprintln!("mcp: end driver session {label} of {}: {reason}", s.owner());
        self.release(s, &label).await;
    }

    async fn release(&self, s: &Session, label: &str) {
        let out = self.rt.dispatcher.end_driver(s.owner(), label).await;
        if out.is_error {
            eprintln!("mcp: end driver session {label}: {}", out.text);
        }
    }

    async fn dispatch(
        &self,
        s: Option<&Session>,
        principal: &Principal,
        msg: &Value,
    ) -> Option<Value> {
        let id = msg.get("id")?.clone();
        let method = msg["method"].as_str()?;
        let result = match (method, s) {
            ("ping", _) => Ok(json!({})),
            ("tools/list", Some(s)) => Ok(self.list_tools(s, principal)),
            ("tools/call", Some(s)) => self.call_tool(s, principal, &msg["params"]).await,
            ("resources/list", _) if self.automation.plans() => Ok(list_resources(&self.resources)),
            ("resources/read", _) if self.automation.plans() => {
                let mut flows = String::new();
                let reads_flows = !matches!(principal, Principal::Share { write: false, .. });
                let guide = self.resources.first().map(|r| r.uri.as_ref());
                if reads_flows && msg["params"]["uri"].as_str() == guide {
                    let index = self
                        .rt
                        .dispatcher
                        .run("", None, "gui_flows", json!({}))
                        .await;
                    if !index.is_error {
                        flows = index.text;
                    }
                }
                read_resource(&self.resources, &msg["params"], &flows).await
            }
            ("initialize", _) => Err((-32600, "initialize must be the only message".to_owned())),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    /// Replaces and releases `stale` after the driver ended it; a concurrent call may have replaced it already.
    async fn rotate_ended(&self, s: &Session, stale: &str) -> String {
        let (label, rotated) = {
            let mut d = lock(&s.driver);
            let rotated = d.label == stale;
            if rotated {
                d.label = new_id("cu");
                eprintln!(
                    "mcp: driver session {stale} of {} ended in the driver; now {}",
                    s.owner(),
                    d.label
                );
            }
            d.used = true;
            (d.label.clone(), rotated)
        };
        if rotated {
            self.release(s, stale).await;
        }
        label
    }

    /// The tools `principal` may call; a share session remembers the mode it listed, to announce a switch.
    fn list_tools(&self, s: &Session, principal: &Principal) -> Value {
        if let Principal::Share { write, .. } = principal {
            *lock(&s.listed) = Some(*write);
        }
        let tools: Vec<&Value> = self
            .tools
            .iter()
            .filter(|(name, _)| allows(principal, name))
            .map(|(_, tool)| tool)
            .collect();
        json!({"tools": tools})
    }

    async fn call_tool(
        &self,
        s: &Session,
        principal: &Principal,
        params: &Value,
    ) -> Result<Value, (i64, String)> {
        let name = params["name"].as_str().unwrap_or_default();
        let Some((name, _)) = self.tools.iter().find(|(n, _)| *n == name) else {
            return Err((-32602, format!("unknown tool: {name}")));
        };
        if !allows(principal, name) {
            return Err((
                -32602,
                format!("{name} is not allowed: the owner shares this box read-only"),
            ));
        }
        let mut requested = match &params["arguments"] {
            Value::Null => json!({}),
            v @ Value::Object(_) => v.clone(),
            _ => return Err((-32602, "arguments must be an object".to_owned())),
        };
        if let Principal::Share { write: false, .. } = principal {
            requested["workspace_only"] = true.into();
        }
        let label = {
            let mut d = lock(&s.driver);
            d.used |= uses_driver(name);
            d.label.clone()
        };
        let launch = requested["action"] == "launch_app";
        let retry = uses_driver(name).then(|| requested.clone());
        let _in_flight = self.rt.turns.read().await;
        let mut out = self.run(s, &label, name, requested).await?;
        if let Some(args) = retry.filter(|_| out.is_error && out.text.contains(DRIVER_ENDED)) {
            let label = self.rotate_ended(s, &label).await;
            out = self.run(s, &label, name, args).await?;
        }
        lock(&s.driver).last_seen = Instant::now();
        let mut text = out.text;
        if self.automation.plans() && s.gui_streak(name, launch) {
            text.push_str(NUDGE);
        }
        let mut content = vec![json!({"type": "text"})];
        content[0]["text"] = text.into();
        if let Some(img) = &out.image {
            let mut part = json!({"type": "image", "mimeType": image_mime(img)});
            part["data"] = b64(img).into();
            content.push(part);
        }
        let mut result = json!({"isError": out.is_error});
        result["content"] = Value::Array(content);
        Ok(result)
    }

    async fn run(
        &self,
        s: &Session,
        label: &str,
        name: &str,
        args: Value,
    ) -> Result<ToolOutput, (i64, String)> {
        let Some(r) = &s.record else {
            return Ok(self
                .rt
                .dispatcher
                .run(s.owner(), Some(label), name, args)
                .await);
        };
        let caller = Caller {
            agent_id: &r.agent_id,
            turn_id: &r.turn_id,
            subagent: "",
            driver: Some(label),
        };
        let call_id = new_id("call");
        let preempted = self.rt.guests.token();
        tokio::select! {
            out = self.rt.dispatcher.call(&caller, &call_id, name, args) => out.map_err(|e| (-32603, e)),
            () = preempted.cancelled() => {
                eprintln!("mcp: a quiesce interrupted {name} of {}", r.agent_id);
                let closed = self.rt.dispatcher.close_shell(s.owner()).await;
                if closed.is_error {
                    eprintln!("mcp: close the shell of {}: {}", r.agent_id, closed.text);
                }
                let out = ToolOutput::error(PREEMPTED);
                self.rt
                    .dispatcher
                    .record_result(&caller, &call_id, &out)
                    .await
                    .map_err(|e| (-32603, e))?;
                Ok(out)
            }
        }
    }

    async fn note(&self, s: &Session, text: &str) {
        let Some(r) = &s.record else {
            return;
        };
        let body = Entry::Text(pb::TextBlock {
            text: text.to_owned(),
            block: r.blocks.fetch_add(1, Ordering::Relaxed),
            ..Default::default()
        });
        self.record(s, Role::System, body).await;
    }

    async fn record(&self, s: &Session, role: Role, body: Entry) {
        let Some(r) = &s.record else {
            return;
        };
        if let Err(e) = self
            .rt
            .journal
            .record(&r.agent_id, &r.turn_id, role, body, false)
            .await
        {
            eprintln!("mcp: record for {}: {e}", r.agent_id);
        }
    }
}

struct Session {
    id: String,
    /// The share the session came through; `None` for the owner.
    guest: Option<Guest>,
    /// The agent that records a share session; the owner's sessions have none.
    record: Option<Record>,
    driver: Mutex<Driver>,
    /// The write mode a share session last listed tools in.
    listed: Mutex<Option<bool>>,
    /// Wakes the session's server stream to send `notifications/tools/list_changed`.
    changed: Notify,
    /// Consecutive `computer_act` calls other than `launch_app` since the last other tool call.
    streak: AtomicU32,
}

impl Session {
    fn agent_id(&self) -> Option<&str> {
        self.record.as_ref().map(|r| r.agent_id.as_str())
    }

    /// Keys the session's shell and desktop state in the toolhost: its agent, or the session itself.
    fn owner(&self) -> &str {
        self.agent_id().unwrap_or(&self.id)
    }

    fn opened_by(&self, principal: &Principal) -> bool {
        match (principal, &self.guest) {
            (Principal::Owner, None) => true,
            (Principal::Share { id, user, .. }, Some(g)) => *id == g.share && *user == g.user,
            _ => false,
        }
    }

    /// Counts a call of `name` into the `computer_act` streak, which a `launch_app` leaves as is; true on the call that reaches [`NUDGE_STREAK`].
    fn gui_streak(&self, name: &str, launch: bool) -> bool {
        if name != "computer_act" {
            self.streak.store(0, Ordering::Relaxed);
            return false;
        }
        !launch && self.streak.fetch_add(1, Ordering::Relaxed) + 1 == NUDGE_STREAK
    }

    /// Announces a tool list change when a share's write mode differs from the one the client listed in.
    fn observe(&self, principal: &Principal) {
        if let Principal::Share { write, .. } = principal
            && lock(&self.listed).is_some_and(|listed| listed != *write)
        {
            self.changed.notify_one();
        }
    }
}

struct Guest {
    share: String,
    user: Option<String>,
}

struct Record {
    agent_id: String,
    turn_id: String,
    blocks: AtomicU32,
}

/// The cua-driver session behind an MCP session; the label rotates each time it is ended.
struct Driver {
    label: String,
    used: bool,
    last_seen: Instant,
}

impl Default for Driver {
    fn default() -> Self {
        Self {
            label: new_id("cu"),
            used: false,
            last_seen: Instant::now(),
        }
    }
}

/// Ends the driver session when the client's GET stream closes.
struct StreamGuard {
    mcp: Arc<Mcp>,
    session: Arc<Session>,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let mcp = Arc::clone(&self.mcp);
        let session = Arc::clone(&self.session);
        tokio::spawn(async move { mcp.end_driver(&session, "stream closed").await });
    }
}

/// A file of the box served as an MCP resource.
#[derive(Clone)]
struct Resource {
    uri: Cow<'static, str>,
    path: Cow<'static, str>,
    mime: Cow<'static, str>,
    description: Cow<'static, str>,
}

async fn post_mcp(State(mcp): State<Arc<Mcp>>, headers: HeaderMap, body: Bytes) -> Response {
    let principal = match mcp.admit(&headers) {
        Ok(p) => p,
        Err(code) => return code.into_response(),
    };
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        return rpc_failure(StatusCode::BAD_REQUEST, -32700, "parse error");
    };
    if msg["method"] == "initialize" {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        return match mcp.open(&msg["params"], &principal).await {
            Ok((session, result)) => {
                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                let mut resp = json_response(StatusCode::OK, &reply);
                if let Ok(v) = session.id.parse() {
                    resp.headers_mut().insert(SESSION_HEADER, v);
                }
                resp
            }
            Err((status, code, message)) => rpc_failure(status, code, &message),
        };
    }
    let (batch, mut messages) = match msg {
        Value::Array(items) => (true, items),
        single => (false, vec![single]),
    };
    let sessionless = !headers.contains_key(SESSION_HEADER)
        && !messages.iter().any(|m| {
            m["method"]
                .as_str()
                .is_some_and(|m| m.starts_with("tools/"))
        });
    let session = if sessionless {
        None
    } else {
        match mcp.session(&headers, &principal) {
            Ok(s) => Some(s),
            Err((status, code, message)) => return rpc_failure(status, code, message),
        }
    };
    if !batch && messages[0]["method"] == "tools/call" && accepts_events(&headers) {
        let msg = messages.swap_remove(0);
        let token = Some(msg["params"]["_meta"]["progressToken"].clone()).filter(|t| !t.is_null());
        let call = {
            let session = session.clone();
            async move { mcp.dispatch(session.as_deref(), &principal, &msg).await }
        };
        return answer(call, token, KEEPALIVE, session).await;
    }
    let mut replies = Vec::new();
    for m in &messages {
        if let Some(reply) = mcp.dispatch(session.as_deref(), &principal, m).await {
            replies.push(reply);
        }
    }
    match (batch, replies.len()) {
        (_, 0) => StatusCode::ACCEPTED.into_response(),
        (false, _) => json_response(StatusCode::OK, &replies[0]),
        (true, _) => json_response(StatusCode::OK, &Value::Array(replies)),
    }
}

async fn get_mcp(State(mcp): State<Arc<Mcp>>, headers: HeaderMap) -> Response {
    let principal = match mcp.admit(&headers) {
        Ok(p) => p,
        Err(code) => return code.into_response(),
    };
    let session = match mcp.session(&headers, &principal) {
        Ok(s) => s,
        Err((status, code, message)) => return rpc_failure(status, code, message),
    };
    let guard = StreamGuard { mcp, session };
    let stream = futures_util::stream::unfold((guard, true), |(guard, first)| async move {
        let mut chunk = &b": keepalive\n\n"[..];
        if !first {
            tokio::select! {
                () = tokio::time::sleep(KEEPALIVE) => {}
                () = guard.session.changed.notified() => chunk = LIST_CHANGED,
                () = guard.mcp.rt.stop.cancelled() => return None,
            }
            lock(&guard.session.driver).last_seen = Instant::now();
        }
        let chunk: Result<Bytes, Infallible> = Ok(Bytes::from_static(chunk));
        Some((chunk, (guard, false)))
    });
    event_stream(Body::from_stream(stream))
}

/// Answers a tool call as JSON when it ends within `every`. A longer call gets an SSE stream that sends a
/// progress notification every `every`, an SSE comment when the client sent no progress token, and then
/// the answer, so that a client's idle timeout does not cut it.
async fn answer<F>(
    call: F,
    token: Option<Value>,
    every: Duration,
    session: Option<Arc<Session>>,
) -> Response
where
    F: Future<Output = Option<Value>> + Send + 'static,
{
    let mut call = Box::pin(call);
    match tokio::time::timeout(every, &mut call).await {
        Ok(Some(reply)) => return json_response(StatusCode::OK, &reply),
        Ok(None) => return StatusCode::ACCEPTED.into_response(),
        Err(_) => {}
    }
    let started = Instant::now();
    let stream = futures_util::stream::unfold(Some((call, 0u64)), move |state| {
        let (token, session) = (token.clone(), session.clone());
        async move {
            let (mut call, ticks) = state?;
            let (chunk, next) = tokio::select! {
                reply = &mut call => (event(&reply?), None),
                () = tokio::time::sleep(every) => {
                    if let Some(s) = &session {
                        lock(&s.driver).last_seen = Instant::now();
                    }
                    let chunk = match &token {
                        Some(token) => event(&json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/progress",
                            "params": {
                                "progressToken": token,
                                "progress": ticks + 1,
                                "message": format!("running for {} s", started.elapsed().as_secs()),
                            },
                        })),
                        None => Bytes::from_static(b": keepalive\n\n"),
                    };
                    (chunk, Some((call, ticks + 1)))
                }
            };
            Some((Ok::<Bytes, Infallible>(chunk), next))
        }
    });
    event_stream(Body::from_stream(stream))
}

fn event(message: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {message}\n\n"))
}

fn event_stream(body: Body) -> Response {
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    h.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    resp
}

/// Whether the client takes an SSE stream as the answer to a POST.
fn accepts_events(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/event-stream"))
}

async fn delete_mcp(State(mcp): State<Arc<Mcp>>, headers: HeaderMap) -> Response {
    let principal = match mcp.admit(&headers) {
        Ok(p) => p,
        Err(code) => return code.into_response(),
    };
    let removed = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|id| {
            let mut sessions = lock(&mcp.sessions);
            sessions
                .get(id)
                .is_some_and(|s| s.opened_by(&principal))
                .then(|| sessions.remove(id))
                .flatten()
        });
    match removed {
        Some(s) => {
            mcp.close(&s, "deleted by the client").await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `These tools act on a cloud computer: Ubuntu 24.04 with the XFCE desktop at 1280x800.`, without
/// the parts `info` lacks.
fn with_routing(mut text: String, extra: &[String]) -> (String, Vec<usize>) {
    let mut skipped = Vec::new();
    let mut len = text.chars().count();
    for (i, part) in extra.iter().enumerate() {
        let add = 2 + part.chars().count();
        if len + add > MAX_INSTRUCTIONS {
            skipped.push(i);
            continue;
        }
        text.push_str("\n\n");
        text.push_str(part);
        len += add;
    }
    (text, skipped)
}

fn machine_line(info: &Value) -> String {
    let os = info["os"].as_str().unwrap_or("Linux");
    let mut line = format!("These tools act on a cloud computer: {os}");
    if let Some(desktop) = info["desktop"].as_str() {
        let _ = write!(line, " with the {desktop} desktop");
    }
    if let (Some(w), Some(h)) = (info["width"].as_u64(), info["height"].as_u64()) {
        let _ = write!(line, " at {w}x{h}");
    }
    line.push('.');
    line
}

fn list_resources(resources: &[Resource]) -> Value {
    let resources: Vec<Value> = resources
        .iter()
        .map(|r| {
            let name = r.uri.rsplit('/').next().unwrap_or(&r.uri);
            json!({"uri": r.uri, "name": name, "mimeType": r.mime, "description": r.description})
        })
        .collect();
    json!({"resources": resources})
}

/// Reads a resource; the guide gains a "Saved flows" section from `flows`, the flow index, up to [`MAX_FLOWS`] lines.
async fn read_resource(
    resources: &[Resource],
    params: &Value,
    flows: &str,
) -> Result<Value, (i64, String)> {
    let uri = params["uri"].as_str().unwrap_or_default();
    let Some(r) = resources.iter().find(|r| r.uri == uri) else {
        return Err((-32002, format!("resource not found: {uri}")));
    };
    let mut text = tokio::fs::read_to_string(r.path.as_ref())
        .await
        .map_err(|e| (-32002, format!("read {}: {e}", r.path)))?;
    let saved: Vec<&str> = flows
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(MAX_FLOWS)
        .collect();
    if !saved.is_empty() {
        text.push_str("\n## Saved flows\n\nReplay one with `gui_plan {flow, params}`.\n\n");
        for line in saved {
            text.push_str("- ");
            text.push_str(line.trim());
            text.push('\n');
        }
    }
    let mut content = json!({"uri": r.uri, "mimeType": r.mime});
    content["text"] = text.into();
    Ok(json!({"contents": [content]}))
}

/// Whether `principal` may call tool `name`: a read-only share gets only the tools that read.
fn allows(principal: &Principal, name: &str) -> bool {
    match principal {
        Principal::Owner | Principal::Share { write: true, .. } => true,
        Principal::Share { write: false, .. } => tools::READ_ONLY.contains(&name),
    }
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut resp = (status, body.to_string()).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    resp
}

fn internal(message: String) -> (StatusCode, i64, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, -32603, message)
}

fn rpc_failure(status: StatusCode, code: i64, message: &str) -> Response {
    let body = json!({"jsonrpc": "2.0", "id": null, "error": {"code": code, "message": message}});
    json_response(status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_extra_joins_the_instructions_until_the_cap() {
        let head = "h".repeat(1500);
        let extra = [
            "a".repeat(40),
            "b".repeat(100),
            "c".repeat(50),
            "d".repeat(10),
        ];
        let (text, skipped) = with_routing(head.clone(), &extra);
        assert_eq!(text, format!("{head}\n\n{}\n\n{}", extra[0], extra[2]));
        assert_eq!(skipped, [1, 3]);
        assert!(text.chars().count() <= MAX_INSTRUCTIONS);
        let (text, skipped) = with_routing("h".repeat(MAX_INSTRUCTIONS), &extra);
        assert_eq!(text.len(), MAX_INSTRUCTIONS);
        assert_eq!(skipped, [0, 1, 2, 3]);
    }

    async fn body(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn the_machine_line_names_what_the_box_reports() {
        assert_eq!(
            machine_line(
                &json!({"os": "Ubuntu 24.04", "desktop": "XFCE", "width": 1280, "height": 800})
            ),
            "These tools act on a cloud computer: Ubuntu 24.04 with the XFCE desktop at 1280x800."
        );
        assert_eq!(
            machine_line(
                &json!({"os": "Ubuntu 22.04", "desktop": "GNOME", "width": 1920, "height": 1080})
            ),
            "These tools act on a cloud computer: Ubuntu 22.04 with the GNOME desktop at 1920x1080."
        );
        assert_eq!(
            machine_line(&json!({"os": "Ubuntu 24.04"})),
            "These tools act on a cloud computer: Ubuntu 24.04."
        );
        assert_eq!(
            machine_line(&Value::Null),
            "These tools act on a cloud computer: Linux."
        );
    }

    #[tokio::test]
    async fn a_long_call_streams_progress_or_keepalives_before_its_answer() {
        let every = Duration::from_millis(40);
        let reply = json!({"jsonrpc": "2.0", "id": 7, "result": {"isError": false}});
        let call = |after: Duration| {
            let reply = reply.clone();
            async move {
                tokio::time::sleep(after).await;
                Some(reply)
            }
        };

        let quick = answer(call(Duration::ZERO), Some(json!("t")), every, None).await;
        assert_eq!(quick.headers()[header::CONTENT_TYPE], "application/json");
        assert_eq!(body(quick).await, reply.to_string());

        let slow = answer(call(every * 4), Some(json!("t")), every, None).await;
        assert_eq!(slow.headers()[header::CONTENT_TYPE], "text/event-stream");
        let events: Vec<Value> = body(slow)
            .await
            .split("\n\n")
            .filter_map(|e| e.strip_prefix("event: message\ndata: "))
            .map(|d| serde_json::from_str(d).unwrap())
            .collect();
        let (last, progress) = events.split_last().unwrap();
        assert_eq!(*last, reply);
        assert!(!progress.is_empty(), "{events:?}");
        for (i, p) in progress.iter().enumerate() {
            assert_eq!(p["method"], "notifications/progress");
            assert_eq!(p["params"]["progressToken"], "t");
            assert_eq!(p["params"]["progress"], i + 1);
        }

        let silent = body(answer(call(every * 4), None, every, None).await).await;
        assert!(silent.starts_with(": keepalive\n\n"), "{silent}");
        assert!(
            silent.ends_with(&format!("event: message\ndata: {reply}\n\n")),
            "{silent}"
        );
        assert!(!silent.contains("notifications/progress"), "{silent}");
    }
}
