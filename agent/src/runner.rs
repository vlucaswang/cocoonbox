//! Per-agent turn queue and the main agent loop.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::{OwnedRwLockReadGuard, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tonic::Status;

use crate::config::ServeConfig;
use crate::connect::{self, Outcome};
use crate::dispatch::{Caller, Dispatcher, ToolOutput, Tools};
use crate::journal::Journal;
use crate::llm::{self, Delta, Message, Model, Part, Request, Tally, ToolCall, ToolSpec, Usage};
use crate::pb::{self, AgentStatus, Role, TranscriptEntry, event, transcript_entry::Body};
use crate::route::Router;
use crate::secrets::SECRETS;
use crate::store::Store;
use crate::{Preempt, computer, flows, lock, new_id, prompts, route, tools};

const QUEUE_DEPTH: usize = 32;
const DOOR_ANSWERS: usize = 64;
const TITLE_CHARS: usize = 24;
const MAIN_MAX_STEPS: usize = 40;
const PLAN_MAX_STEPS: usize = 150;
const MAIN_MAX_TOKENS: u32 = 4096;
const REPORT_MAX_TOKENS: u32 = 1024;
const INTERRUPTED: &str = "(no result: the turn was interrupted)";
const SHUTTING_DOWN: &str = "interrupted: the agent is shutting down";
const NO_ANSWER: &str = "the model returned no answer";
const TOOL_MARKUP: &str = "｜DSML｜";
const ANNOUNCE_CHARS: usize = 80;
const NO_RESPONSE: &str = "no response";
const ANNOUNCE_LEADS: [&str; 2] = ["continu", "继续"];
const ANNOUNCE_PHRASES: [&str; 4] = [
    "i'll continu",
    "i’ll continu",
    "i will continu",
    "continuing.",
];
const DONE_WORDS: [&str; 5] = ["done", "finished", "completed", "saved", "all"];
const DONE_MARKS: [&str; 4] = ["完成", "已", "保存", "全部"];
const LEAD_IN_ENDS: [&str; 4] = [":", "：", "…", "..."];
const NEXT_LEADS: [&str; 8] = [
    "let me ",
    "i'll ",
    "i’ll ",
    "i will ",
    "now i'll",
    "now i’ll",
    "starting with",
    "接下来",
];

/// Shared state of `serve`: journal, tool dispatcher, model and per-agent queues.
pub struct Runtime {
    pub cfg: ServeConfig,
    pub journal: Arc<Journal>,
    pub dispatcher: Arc<Dispatcher>,
    pub(crate) model: Arc<dyn Model>,
    pub(crate) computer_tools: Vec<ToolSpec>,
    pub(crate) computer_prompt: String,
    pub(crate) screen: tokio::sync::Mutex<()>,
    /// Cancelled when the process starts shutting down.
    pub stop: CancellationToken,
    /// Work that shutdown waits for: running turns and desktop-session cleanups.
    pub tasks: TaskTracker,
    /// Held shared by every running turn not waiting for the user and every MCP tool call; the state layer takes it exclusively to pause them.
    pub turns: Arc<tokio::sync::RwLock<()>>,
    /// Interrupts the MCP calls of share sessions when a quiesce has waited long enough.
    pub guests: Arc<Preempt>,
    /// The tier router of a box that runs GUI plans; `None` elsewhere.
    pub router: Option<Router>,
    connect: Option<Arc<connect::Client>>,
    main_tools: Vec<ToolSpec>,
    main_vision_tools: Vec<ToolSpec>,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    door_answered: Mutex<VecDeque<String>>,
}

impl Runtime {
    /// Opens the store, closes turns left open by a previous process, and builds the runtime over `model` and `tools`.
    pub async fn open(
        mut cfg: ServeConfig,
        model: Arc<dyn Model>,
        tools: Arc<dyn Tools>,
    ) -> Result<Arc<Self>, String> {
        if !cfg.automation.plans() {
            cfg.plugins.clear();
        }
        std::fs::create_dir_all(&cfg.state_dir)
            .map_err(|e| format!("create {}: {e}", cfg.state_dir.display()))?;
        let journal = Arc::new(Journal::new(Store::open(&cfg.state_dir.join("agent.db"))?));
        let stop = CancellationToken::new();
        let connect = cfg
            .connect_url
            .as_deref()
            .map(connect::Client::new)
            .transpose()?
            .map(Arc::new);
        let router = cfg
            .automation
            .plans()
            .then(|| Router::new(&cfg.gw_base_url, &cfg.gw_api_key))
            .transpose()?;
        let flows = match &cfg.connect_url {
            Some(url) if cfg.automation.plans() => {
                let sync = Arc::new(flows::Syncer::new(
                    url,
                    journal.store.clone(),
                    Arc::clone(&tools),
                )?);
                sync.spawn(stop.clone());
                Some(sync)
            }
            _ => None,
        };
        let rt = Arc::new(Self {
            model,
            router,
            dispatcher: Arc::new(
                Dispatcher::new(tools, Arc::clone(&journal))
                    .with_flows(flows)
                    .with_plugins(cfg.plugins.iter().map(|p| p.name.clone()).collect()),
            ),
            journal,
            main_tools: loop_tools(false, &cfg),
            main_vision_tools: loop_tools(true, &cfg),
            computer_tools: tools::computer_tools(cfg.automation),
            computer_prompt: prompts::computer(&cfg.model_computer, cfg.automation),
            screen: tokio::sync::Mutex::new(()),
            stop,
            connect,
            tasks: TaskTracker::new(),
            turns: Arc::new(tokio::sync::RwLock::new(())),
            guests: Arc::default(),
            slots: Mutex::new(HashMap::new()),
            door_answered: Mutex::new(VecDeque::new()),
            cfg,
        });
        if let Some(client) = &rt.connect {
            let weak = Arc::downgrade(&rt);
            client.watch(rt.stop.clone(), move |id, approve| {
                if let Some(rt) = weak.upgrade() {
                    rt.door_answer(id, approve);
                }
            });
        }
        rt.recover("interrupted: the agent restarted").await?;
        Ok(rt)
    }

    /// Ends every turn left open in the store, e.g. after a restart or a restored agent.db.
    pub async fn recover(&self, reason: &str) -> Result<(), String> {
        for (agent_id, turn_id) in self.journal.store.unfinished_turns().await? {
            let end = pb::TurnEnd {
                ok: false,
                error: reason.to_owned(),
                usage: None,
            };
            self.journal
                .record(&agent_id, &turn_id, Role::System, Body::TurnEnd(end), false)
                .await?;
            self.journal.store.finish_turn(turn_id).await?;
            self.journal
                .set_status(&agent_id, AgentStatus::Failed)
                .await;
        }
        Ok(())
    }

    /// Creates an agent; an empty model selects the box default, an empty name waits for the first message.
    pub async fn create_agent(&self, req: pb::CreateAgentRequest) -> Result<pb::Agent, Status> {
        let model = if req.main_model.is_empty() {
            self.cfg.model_main.clone()
        } else {
            req.main_model
        };
        let agent = self
            .journal
            .store
            .create_agent(req.name, model, req.client_nonce)
            .await
            .map_err(Status::internal)?;
        self.journal.bus.publish(event::Body::Agent(agent.clone()));
        Ok(agent)
    }

    /// Queues a user turn and returns its id; duplicate nonces return the first turn.
    pub async fn send(self: &Arc<Self>, req: pb::SendRequest) -> Result<String, Status> {
        if self
            .journal
            .store
            .agent(req.agent_id.clone())
            .await
            .map_err(Status::internal)?
            .is_none()
        {
            return Err(Status::not_found(format!(
                "agent {} not found",
                req.agent_id
            )));
        }
        let title = title(&req.text);
        let mut text = req.text;
        for a in req.attachments {
            let path = format!("uploads/{}", sanitize(&a.name));
            let out = self
                .dispatcher
                .put_file(&req.agent_id, &path, &a.data)
                .await;
            if out.is_error {
                return Err(Status::internal(out.text));
            }
            let _ = write!(
                text,
                "\n[attached file: {} ({}, {} bytes)]",
                out.text,
                a.mime_type,
                a.data.len()
            );
        }
        let queued = self
            .journal
            .store
            .queue_turn(req.agent_id.clone(), text, req.client_nonce)
            .await
            .map_err(Status::internal)?;
        let Some(entry) = queued.entry else {
            return Ok(queued.turn_id);
        };
        self.journal.bus.publish(event::Body::Transcript(entry));
        if let Some(title) = title {
            self.journal.title(&req.agent_id, title).await;
        }
        let slot = self.slot(&req.agent_id);
        if slot.queue.try_send(queued.turn_id.clone()).is_err() {
            let end = pb::TurnEnd {
                ok: false,
                error: "turn queue is full".to_owned(),
                usage: None,
            };
            self.journal
                .record(
                    &req.agent_id,
                    &queued.turn_id,
                    Role::System,
                    Body::TurnEnd(end),
                    false,
                )
                .await
                .map_err(Status::internal)?;
            self.journal
                .store
                .finish_turn(queued.turn_id)
                .await
                .map_err(Status::internal)?;
            return Err(Status::resource_exhausted("turn queue is full"));
        }
        Ok(queued.turn_id)
    }

    /// Sets an agent's name.
    pub async fn rename(&self, req: pb::RenameAgentRequest) -> Result<pb::Agent, Status> {
        let name = req.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("name is empty"));
        }
        let agent = self
            .journal
            .store
            .rename_agent(req.agent_id.clone(), name.to_owned())
            .await
            .map_err(Status::internal)?
            .ok_or_else(|| Status::not_found(format!("agent {} not found", req.agent_id)))?;
        self.journal.bus.publish(event::Body::Agent(agent.clone()));
        Ok(agent)
    }

    /// Cancels the agent's running turn, waits for it to end, then removes the agent and its history.
    pub async fn delete(&self, agent_id: &str) -> Result<(), Status> {
        let slot = lock(&self.slots).remove(agent_id);
        let _idle = match &slot {
            Some(slot) => {
                {
                    let mut state = lock(&slot.state);
                    state.deleted = true;
                    if let Some(token) = &state.cancel {
                        token.cancel();
                    }
                }
                Some(slot.turn.lock().await)
            }
            None => None,
        };
        let existed = self
            .journal
            .store
            .delete_agent(agent_id.to_owned())
            .await
            .map_err(Status::internal)?;
        if !existed {
            return Err(Status::not_found(format!("agent {agent_id} not found")));
        }
        let closed = self.dispatcher.close_shell(agent_id).await;
        if closed.is_error {
            eprintln!("delete {agent_id}: close its shell: {}", closed.text);
        }
        let closed = self.dispatcher.close_window(agent_id).await;
        if closed.is_error {
            eprintln!("delete {agent_id}: close its window: {}", closed.text);
        }
        self.journal
            .bus
            .publish(event::Body::AgentDeleted(pb::AgentDeleted {
                agent_id: agent_id.to_owned(),
            }));
        Ok(())
    }

    /// Delivers the user's answer to a pending approval; one the broker already reported answered is accepted.
    pub fn respond(&self, req: pb::RespondRequest) -> Result<(), Status> {
        let slot = self.existing_slot(&req.agent_id);
        let Some(tx) = slot.and_then(|s| lock(&s.state).approvals.remove(&req.approval_id)) else {
            if lock(&self.door_answered).contains(&req.approval_id) {
                return Ok(());
            }
            return Err(Status::not_found(format!(
                "approval {} is not pending",
                req.approval_id
            )));
        };
        tx.send(Reply {
            approve: req.approve,
            text: req.text,
        })
        .map_err(|_| Status::failed_precondition("the turn waiting for this approval has ended"))
    }

    /// Aborts the agent's running turn, if any.
    pub fn cancel(&self, agent_id: &str) {
        if let Some(slot) = self.existing_slot(agent_id)
            && let Some(token) = &lock(&slot.state).cancel
        {
            token.cancel();
        }
    }

    /// Waits up to `limit` for interrupted turns and cleanups, then checkpoints the store.
    pub async fn drain(&self, limit: std::time::Duration) {
        self.tasks.close();
        if tokio::time::timeout(limit, self.tasks.wait())
            .await
            .is_err()
        {
            eprintln!("shutdown: {} task(s) still running", self.tasks.len());
        }
        if let Err(e) = self.journal.store.checkpoint().await {
            eprintln!("shutdown: checkpoint: {e}");
        }
    }

    /// Streams one model step, recording blocks and tool calls as they complete.
    pub async fn step(
        &self,
        ctx: &TurnCtx,
        req: &Request<'_>,
        subagent: &str,
    ) -> Result<StepOut, String> {
        let mut stream = self.model.stream(req).await?;
        let mut text = String::new();
        let mut pending = Block::default();
        let mut reasoning = Block::default();
        let mut calls: Vec<(usize, ToolCall)> = Vec::new();
        let mut usage = Usage::default();
        while let Some(d) = stream.next().await? {
            match d {
                Delta::Reasoning(t) => {
                    self.stream_delta(ctx, &mut reasoning, Kind::Reasoning, &t, subagent);
                }
                Delta::Text(t) => {
                    self.flush_block(ctx, &mut reasoning, Kind::Reasoning, subagent)
                        .await?;
                    self.stream_delta(ctx, &mut pending, Kind::Text, &t, subagent);
                }
                Delta::ToolStart { index, id, name } => {
                    self.flush_block(ctx, &mut reasoning, Kind::Reasoning, subagent)
                        .await?;
                    text.push_str(
                        &self
                            .flush_block(ctx, &mut pending, Kind::Text, subagent)
                            .await?,
                    );
                    calls.push((
                        index,
                        ToolCall {
                            id,
                            name,
                            arguments: String::new(),
                        },
                    ));
                }
                Delta::ToolArgs { index, fragment } => {
                    if let Some((_, c)) = calls.iter_mut().rev().find(|(i, _)| *i == index) {
                        c.arguments.push_str(&fragment);
                    }
                }
                Delta::Usage(u) => usage = u,
                Delta::Done => break,
            }
        }
        self.flush_block(ctx, &mut reasoning, Kind::Reasoning, subagent)
            .await?;
        text.push_str(
            &self
                .flush_block(ctx, &mut pending, Kind::Text, subagent)
                .await?,
        );
        let mut out = Vec::with_capacity(calls.len());
        for (n, (_, mut call)) in calls.into_iter().enumerate() {
            if call.id.is_empty() {
                call.id = format!(
                    "call_{}_{n}",
                    &ctx.turn_id[ctx.turn_id.len().saturating_sub(8)..]
                );
            }
            if call.arguments.trim().is_empty() {
                "{}".clone_into(&mut call.arguments);
            }
            self.dispatcher
                .record_call(
                    &ctx.caller(subagent, None),
                    &call.id,
                    &call.name,
                    &call.arguments,
                )
                .await?;
            out.push(call);
        }
        Ok(StepOut {
            text,
            calls: out,
            usage,
        })
    }

    /// Asks for a progress report with tool calls forbidden once a loop has used all `cap` steps of `req`; `None` when none came.
    pub async fn cap_report(
        &self,
        ctx: &TurnCtx,
        req: &Request<'_>,
        cap: usize,
        subagent: &str,
        usage: &mut Tally,
    ) -> Option<String> {
        let mut messages = req.messages.to_vec();
        messages.push(Message::User(vec![Part::Text(prompts::cap_report(cap))]));
        let req = Request {
            messages: &messages,
            max_tokens: REPORT_MAX_TOKENS,
            forbid_calls: true,
            ..*req
        };
        match self.step(ctx, &req, subagent).await {
            Ok(step) => {
                usage.add(req.model, 1, step.usage);
                let report = step.text.trim();
                (!report.is_empty()).then(|| report.to_owned())
            }
            Err(e) => {
                eprintln!("progress report of turn {}: {e}", ctx.turn_id);
                None
            }
        }
    }

    /// Persists an entry of the current turn and publishes it.
    pub async fn record(
        &self,
        ctx: &TurnCtx,
        role: Role,
        body: Body,
        subagent: bool,
    ) -> Result<(), String> {
        self.journal
            .record(&ctx.agent_id, &ctx.turn_id, role, body, subagent)
            .await
    }

    fn slot(self: &Arc<Self>, agent_id: &str) -> Arc<Slot> {
        let mut slots = lock(&self.slots);
        if let Some(s) = slots.get(agent_id) {
            return Arc::clone(s);
        }
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let slot = Arc::new(Slot {
            queue: tx,
            turn: tokio::sync::Mutex::new(()),
            state: Mutex::new(SlotState::default()),
        });
        slots.insert(agent_id.to_owned(), Arc::clone(&slot));
        // detached worker: runs this agent's turns in order for the life of the process
        tokio::spawn(worker(
            Arc::clone(self),
            agent_id.to_owned(),
            Arc::clone(&slot),
            rx,
        ));
        slot
    }

    fn existing_slot(&self, agent_id: &str) -> Option<Arc<Slot>> {
        lock(&self.slots).get(agent_id).cloned()
    }

    fn door_answer(&self, approval_id: &str, approve: bool) {
        let slots: Vec<Arc<Slot>> = lock(&self.slots).values().cloned().collect();
        let Some(tx) = slots
            .iter()
            .find_map(|s| lock(&s.state).approvals.remove(approval_id))
        else {
            return;
        };
        {
            let mut answered = lock(&self.door_answered);
            if answered.len() == DOOR_ANSWERS {
                answered.pop_front();
            }
            answered.push_back(approval_id.to_owned());
        }
        let _ = tx.send(Reply {
            approve,
            text: String::new(),
        });
    }

    async fn run_turn(&self, agent_id: &str, slot: Arc<Slot>, turn_id: String) {
        let model = match self.journal.store.agent(agent_id.to_owned()).await {
            Ok(Some(a)) => a.main_model,
            _ => self.cfg.model_main.clone(),
        };
        let gate = Arc::clone(&self.turns).read_owned().await;
        let cancel = CancellationToken::new();
        {
            let mut state = lock(&slot.state);
            if state.deleted {
                return;
            }
            state.cancel = Some(cancel.clone());
        }
        let ctx = TurnCtx {
            agent_id: agent_id.to_owned(),
            turn_id,
            // block 0 is the user's entry
            blocks: AtomicU32::new(1),
            model,
            slot,
            gate: Mutex::new(Some(gate)),
        };
        self.journal
            .set_status(agent_id, AgentStatus::Running)
            .await;
        let mut usage = Tally::default();
        let result = tokio::select! {
            r = self.main_loop(&ctx, &mut usage) => r,
            () = cancel.cancelled() => Err("cancelled".to_owned()),
            () = self.stop.cancelled() => Err(SHUTTING_DOWN.to_owned()),
        };
        {
            let mut state = lock(&ctx.slot.state);
            state.cancel = None;
            state.approvals.clear();
        }
        let (ok, error) = match result {
            Ok(()) => (true, String::new()),
            Err(e) => (false, e),
        };
        let total = usage.total();
        let models = usage
            .iter()
            .map(|(model, calls, u)| pb::ModelUsage {
                model: model.to_owned(),
                calls,
                input_tokens: u.input,
                output_tokens: u.output,
                cached_input_tokens: u.cached,
            })
            .collect();
        let end = pb::TurnEnd {
            ok,
            error,
            usage: Some(pb::Usage {
                input_tokens: total.input,
                output_tokens: total.output,
                cached_input_tokens: total.cached,
                models,
            }),
        };
        if let Err(e) = self
            .record(&ctx, Role::System, Body::TurnEnd(end), false)
            .await
        {
            eprintln!("record turn end {}: {e}", ctx.turn_id);
        }
        if let Err(e) = self.journal.store.finish_turn(ctx.turn_id.clone()).await {
            eprintln!("finish turn {}: {e}", ctx.turn_id);
        }
        let status = if ok {
            AgentStatus::Idle
        } else {
            AgentStatus::Failed
        };
        self.journal.set_status(agent_id, status).await;
    }

    async fn main_loop(&self, ctx: &TurnCtx, usage: &mut Tally) -> Result<(), String> {
        let history = self
            .journal
            .store
            .history(ctx.agent_id.clone(), ctx.turn_id.clone())
            .await?;
        let mut messages = history_messages(history);
        let vision = llm::has_vision(&ctx.model);
        let tools = if vision {
            &self.main_vision_tools
        } else {
            &self.main_tools
        };
        let mut system = prompts::main(
            &ctx.model,
            &self.cfg.model_computer,
            vision,
            self.cfg.automation,
            &self.cfg.routing_extra,
        );
        if let Some(keys) = tokio::fs::read_to_string(prompts::KEYS_FILE)
            .await
            .ok()
            .as_deref()
            .and_then(prompts::keys)
        {
            system.push_str("\n\n");
            system.push_str(&keys);
        }
        let tier = match &self.router {
            Some(router) => self.front_route(ctx, router, &mut messages, usage).await?,
            None => None,
        };
        let cap = if self.cfg.automation.scripts() {
            PLAN_MAX_STEPS
        } else {
            MAIN_MAX_STEPS
        };
        let mut steps = 0;
        let mut nudged = false;
        loop {
            let connectors = match &self.connect {
                Some(c) => c.tools().await,
                None => Arc::default(),
            };
            let mut offered: Vec<&ToolSpec> = tools
                .iter()
                .chain(
                    connectors
                        .iter()
                        .filter(|c| !tools.iter().any(|t| t.name == c.name)),
                )
                .collect();
            if let Some(tier) = tier {
                offered.sort_by_key(|t| {
                    let plugin =
                        tier == "plan" && self.cfg.plugins.iter().any(|p| p.name == t.name);
                    !(route::leads(tier, &t.name) || plugin)
                });
            }
            let req = Request {
                model: &ctx.model,
                system: &system,
                messages: &messages,
                tools: &offered,
                max_tokens: MAIN_MAX_TOKENS,
                vision,
                forbid_calls: false,
            };
            if steps == cap {
                return Err(match self.cap_report(ctx, &req, cap, "", usage).await {
                    Some(_) => format!(
                        "stopped at the {cap}-step cap; the last reply reports the progress"
                    ),
                    None => format!("stopped after {cap} steps without a final answer"),
                });
            }
            steps += 1;
            let step = self.step(ctx, &req, "").await?;
            usage.add(&ctx.model, 1, step.usage);
            if step.calls.is_empty() {
                match ending(&step.text) {
                    Ending::Answer => return Ok(()),
                    Ending::Announcement if nudged => return Ok(()),
                    _ if nudged => return Err(NO_ANSWER.to_owned()),
                    _ => {}
                }
                nudged = true;
                messages.push(Message::User(vec![Part::Text(
                    prompts::CONTINUE.to_owned(),
                )]));
                continue;
            }
            nudged = false;
            let mut results = Vec::with_capacity(step.calls.len());
            for call in &step.calls {
                let out = self.main_tool(ctx, call, usage).await?;
                results.push(Message::Tool {
                    call_id: call.id.clone(),
                    text: out.text,
                    image: out.image,
                    is_error: out.is_error,
                });
            }
            messages.push(Message::Assistant {
                text: step.text,
                calls: step.calls,
            });
            messages.extend(results);
        }
    }

    /// Routes the turn's request; a confident route leads the request with one line and returns its tier.
    async fn front_route(
        &self,
        ctx: &TurnCtx,
        router: &Router,
        messages: &mut [Message],
        usage: &mut Tally,
    ) -> Result<Option<&'static str>, String> {
        let Some(Message::User(parts)) = messages.last_mut() else {
            return Ok(None);
        };
        let task = parts
            .iter()
            .find_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                Part::Image(_) => None,
            })
            .unwrap_or_default();
        let r = router.route(task).await;
        if let Some(u) = r.usage {
            usage.add(router.model(), 1, u);
        }
        let tier = route::TIERS
            .iter()
            .find(|(t, _)| r.confident() && *t == r.tier)
            .map(|(t, _)| *t);
        let note = match (&r.error, tier) {
            (Some(e), _) => format!("Router: unavailable ({e}); the model decides."),
            (None, Some(t)) => format!(
                "Router: this task is tier {t} ({:.2}): {}.",
                r.confidence,
                route::meaning(t).unwrap_or_default()
            ),
            (None, None) => format!(
                "Router: uncertain (best {} at {:.2}); the model decides.",
                r.tier, r.confidence
            ),
        };
        if tier.is_some() {
            parts.insert(0, Part::Text(note.clone()));
        }
        let block = ctx.blocks.fetch_add(1, Ordering::Relaxed);
        let body = Body::Text(pb::TextBlock {
            text: note,
            block,
            ..Default::default()
        });
        self.record(ctx, Role::System, body, false).await?;
        Ok(tier)
    }

    /// Runs one call of the main model; box tools go through the dispatcher, loop tools run here.
    async fn main_tool(
        &self,
        ctx: &TurnCtx,
        call: &ToolCall,
        usage: &mut Tally,
    ) -> Result<ToolOutput, String> {
        let caller = ctx.caller("", None);
        let args: Value = match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => {
                let out = ToolOutput::error(format!("arguments are not valid JSON: {e}"));
                self.dispatcher
                    .record_result(&caller, &call.id, &out)
                    .await?;
                return Ok(out);
            }
        };
        let mut out = match call.name.as_str() {
            "delegate_computer" => match args["task"].as_str().filter(|t| !t.is_empty()) {
                Some(task) => computer::run(self, ctx, task, usage)
                    .await
                    .unwrap_or_else(|e| {
                        ToolOutput::error(format!("computer subagent failed: {e}"))
                    }),
                None => ToolOutput::error("delegate_computer needs a task"),
            },
            "request_take_over" => {
                let reason = args["reason"].as_str().unwrap_or("The agent needs you.");
                self.take_over(ctx, reason)
                    .await
                    .unwrap_or_else(ToolOutput::error)
            }
            name => match self.connect.as_ref().filter(|c| c.serves(name)) {
                Some(client) => self.connector(ctx, client, name, &args).await?,
                None => {
                    let out = self
                        .dispatcher
                        .execute(&caller, &call.id, name, args)
                        .await?;
                    if name == "gui_plan" {
                        usage.add_reported(&out.text);
                    }
                    return Ok(out);
                }
            },
        };
        SECRETS.scrub_prose(&mut out.text);
        self.dispatcher
            .record_result(&caller, &call.id, &out)
            .await?;
        Ok(out)
    }

    async fn take_over(&self, ctx: &TurnCtx, reason: &str) -> Result<ToolOutput, String> {
        let reply = self
            .ask(ctx, new_id("ap"), reason.to_owned(), "take_over")
            .await?;
        let mut text = if reply.approve {
            "The user finished the take-over; continue from the current screen state.".to_owned()
        } else {
            "The user declined the take-over.".to_owned()
        };
        if !reply.text.is_empty() {
            let _ = write!(text, " User note: {}", reply.text);
        }
        Ok(ToolOutput {
            text,
            image: None,
            is_error: !reply.approve,
        })
    }

    /// Runs a connector call; one the broker holds for approval runs again only after the user approves it.
    async fn connector(
        &self,
        ctx: &TurnCtx,
        client: &connect::Client,
        name: &str,
        args: &Value,
    ) -> Result<ToolOutput, String> {
        let (id, summary) = match client.call(name, args, None).await {
            Outcome::Done(out) => return Ok(out),
            Outcome::Approval { id, summary } => (id, summary),
        };
        if !self
            .ask(ctx, id.clone(), summary, "connector")
            .await?
            .approve
        {
            return Ok(ToolOutput::error("The user declined the call."));
        }
        Ok(match client.call(name, args, Some(&id)).await {
            Outcome::Done(out) => out,
            Outcome::Approval { .. } => {
                ToolOutput::error("Connect asked for another approval; the call did not run.")
            }
        })
    }

    /// Records an approval request under `approval_id` and waits for the user's answer.
    async fn ask(
        &self,
        ctx: &TurnCtx,
        approval_id: String,
        summary: String,
        kind: &str,
    ) -> Result<Reply, String> {
        let (tx, rx) = oneshot::channel();
        lock(&ctx.slot.state)
            .approvals
            .insert(approval_id.clone(), tx);
        let block = pb::ApprovalRequest {
            approval_id,
            summary,
            kind: kind.to_owned(),
        };
        self.record(ctx, Role::Assistant, Body::Approval(block), false)
            .await?;
        self.journal
            .set_status(&ctx.agent_id, AgentStatus::WaitingForUser)
            .await;
        drop(lock(&ctx.gate).take());
        let reply = rx
            .await
            .map_err(|_| format!("the {kind} approval was abandoned"))?;
        let gate = Arc::clone(&self.turns).read_owned().await;
        *lock(&ctx.gate) = Some(gate);
        self.journal
            .set_status(&ctx.agent_id, AgentStatus::Running)
            .await;
        Ok(reply)
    }

    fn stream_delta(&self, ctx: &TurnCtx, block: &mut Block, kind: Kind, t: &str, subagent: &str) {
        block.text.push_str(t);
        if block.index.is_none() {
            if block.text.trim().is_empty() {
                return;
            }
            block.index = Some(ctx.blocks.fetch_add(1, Ordering::Relaxed));
        }
        self.publish_delta(ctx, block, kind, subagent, false);
    }

    fn publish_delta(
        &self,
        ctx: &TurnCtx,
        block: &mut Block,
        kind: Kind,
        subagent: &str,
        end: bool,
    ) {
        let text = SECRETS.stream(&block.text, &mut block.sent, end);
        if text.is_empty() {
            return;
        }
        let (agent_id, turn_id) = (ctx.agent_id.clone(), ctx.turn_id.clone());
        let (block, subagent) = (block.index.unwrap_or_default(), subagent.to_owned());
        self.journal.bus.publish_live(match kind {
            Kind::Text => event::Body::TextDelta(pb::TextDelta {
                agent_id,
                turn_id,
                block,
                text,
                subagent,
            }),
            Kind::Reasoning => event::Body::ReasoningDelta(pb::ReasoningDelta {
                agent_id,
                turn_id,
                block,
                text,
                subagent,
            }),
        });
    }

    /// Records a finished block and returns its text; blank blocks were never streamed and are not recorded.
    async fn flush_block(
        &self,
        ctx: &TurnCtx,
        block: &mut Block,
        kind: Kind,
        subagent: &str,
    ) -> Result<String, String> {
        if block.index.is_some() {
            self.publish_delta(ctx, block, kind, subagent, true);
        }
        let Block { text, index, .. } = std::mem::take(block);
        let Some(index) = index else {
            return Ok(text);
        };
        let body = match kind {
            Kind::Text => Body::Text(pb::TextBlock {
                text: text.clone(),
                subagent: subagent.to_owned(),
                block: index,
            }),
            Kind::Reasoning => Body::Reasoning(pb::ReasoningBlock {
                text: text.clone(),
                block: index,
                subagent: subagent.to_owned(),
            }),
        };
        self.record(ctx, Role::Assistant, body, !subagent.is_empty())
            .await?;
        Ok(text)
    }
}

/// Identity of the running turn.
pub struct TurnCtx {
    pub agent_id: String,
    pub turn_id: String,
    blocks: AtomicU32,
    model: String,
    slot: Arc<Slot>,
    gate: Mutex<Option<OwnedRwLockReadGuard<()>>>,
}

impl TurnCtx {
    /// The dispatcher's view of this turn.
    pub fn caller<'a>(&'a self, subagent: &'a str, driver: Option<&'a str>) -> Caller<'a> {
        Caller {
            agent_id: &self.agent_id,
            turn_id: &self.turn_id,
            subagent,
            driver,
        }
    }
}

/// One model step: its text, requested tool calls and usage.
pub struct StepOut {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// A streamed text or reasoning block; `index` is assigned when its first non-blank delta goes out, and
/// `sent` bytes of `text` went out.
#[derive(Default)]
struct Block {
    text: String,
    index: Option<u32>,
    sent: usize,
}

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Reasoning,
}

struct Slot {
    queue: mpsc::Sender<String>,
    /// Held while a turn runs, so a delete can wait for it.
    turn: tokio::sync::Mutex<()>,
    state: Mutex<SlotState>,
}

#[derive(Default)]
struct SlotState {
    cancel: Option<CancellationToken>,
    approvals: HashMap<String, oneshot::Sender<Reply>>,
    deleted: bool,
}

struct Reply {
    approve: bool,
    text: String,
}

/// Rebuilds the main agent's model messages from its transcript.
pub fn history_messages(entries: Vec<TranscriptEntry>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut open: Vec<String> = Vec::new();
    for e in entries {
        let user = e.role == i32::from(Role::User);
        match e.body {
            Some(Body::Text(_)) if e.role == i32::from(Role::System) => {}
            Some(Body::Text(t)) if !user && t.text.contains(TOOL_MARKUP) => {}
            Some(Body::Text(t)) if user => {
                close_open(&mut out, &mut open);
                out.push(Message::User(vec![Part::Text(t.text)]));
            }
            Some(Body::Text(t)) => {
                close_open(&mut out, &mut open);
                match out.last_mut() {
                    Some(Message::Assistant { text, calls }) if calls.is_empty() => {
                        text.push_str(&t.text);
                    }
                    _ => out.push(Message::Assistant {
                        text: t.text,
                        calls: Vec::new(),
                    }),
                }
            }
            Some(Body::ToolCall(c)) => {
                open.push(c.call_id.clone());
                let call = ToolCall {
                    id: c.call_id,
                    name: c.name,
                    arguments: c.arguments_json,
                };
                match out.last_mut() {
                    Some(Message::Assistant { calls, .. }) => calls.push(call),
                    _ => out.push(Message::Assistant {
                        text: String::new(),
                        calls: vec![call],
                    }),
                }
            }
            Some(Body::ToolResult(r)) => {
                if let Some(pos) = open.iter().position(|id| *id == r.call_id) {
                    open.remove(pos);
                    out.push(Message::Tool {
                        call_id: r.call_id,
                        text: r.text,
                        image: None,
                        is_error: r.is_error,
                    });
                }
            }
            _ => {}
        }
    }
    close_open(&mut out, &mut open);
    out
}

/// The main agent's tools: those of `cfg`'s automation and its plugins.
fn loop_tools(vision: bool, cfg: &ServeConfig) -> Vec<ToolSpec> {
    let mut tools = tools::main_tools(vision, cfg.automation);
    tools::with_plugins(&mut tools, &cfg.plugins);
    tools
}

fn close_open(out: &mut Vec<Message>, open: &mut Vec<String>) {
    for call_id in open.drain(..) {
        out.push(Message::Tool {
            call_id,
            text: INTERRUPTED.to_owned(),
            image: None,
            is_error: true,
        });
    }
}

async fn worker(
    rt: Arc<Runtime>,
    agent_id: String,
    slot: Arc<Slot>,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(turn_id) = rx.recv().await {
        if rt.stop.is_cancelled() || lock(&slot.state).deleted {
            break;
        }
        let _turn = slot.turn.lock().await;
        let turn = rt.run_turn(&agent_id, Arc::clone(&slot), turn_id);
        rt.tasks.track_future(turn).await;
    }
}

#[derive(Debug, PartialEq)]
enum Ending {
    Answer,
    Announcement,
    Empty,
}

/// Sorts a reply without a tool call: a final answer, a bare announcement of more work, or empty (blank, tool markup, a lone closing tag, "no response"); a done word counts only in the last sentence.
fn ending(text: &str) -> Ending {
    let text = text.trim();
    let last = text.lines().last().unwrap_or_default().trim();
    let closing_tag = last
        .strip_prefix("</")
        .and_then(|l| l.strip_suffix('>'))
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    let lower = text.to_lowercase();
    let line = !text.contains('\n') && text.chars().count() <= ANNOUNCE_CHARS;
    if text.is_empty()
        || text.contains(TOOL_MARKUP)
        || closing_tag
        || (line && lower.starts_with(NO_RESPONSE))
    {
        return Ending::Empty;
    }
    let announces = line
        && (ANNOUNCE_LEADS.iter().any(|l| lower.starts_with(l))
            || ANNOUNCE_PHRASES.iter().any(|p| lower.contains(p)));
    let next = lower
        .split(['.', '!', '。', '！'])
        .map(str::trim_start)
        .any(|s| NEXT_LEADS.iter().any(|l| s.starts_with(l)) && !s.starts_with("let me know"));
    let lead_in = LEAD_IN_ENDS.iter().any(|e| text.ends_with(e));
    let body = lower.trim_end_matches(['.', '!', '。', '！', ' ']);
    let last = [". ", "! ", "。", "！"]
        .iter()
        .filter_map(|e| body.rfind(e).map(|i| i + e.len()))
        .max()
        .map_or(body, |i| &body[i..]);
    let done = last
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| DONE_WORDS.contains(&w))
        || DONE_MARKS.iter().any(|m| last.contains(m));
    if (announces || (!text.contains('\n') && (lead_in || next)))
        && !text.ends_with(['?', '？'])
        && !done
    {
        Ending::Announcement
    } else {
        Ending::Answer
    }
}

/// The first non-blank line of `text`, trimmed and cut to [`TITLE_CHARS`] characters.
fn title(text: &str) -> Option<String> {
    let line = text.trim().lines().next()?.trim();
    let cut: String = line.chars().take(TITLE_CHARS).collect();
    let cut = cut.trim_end();
    (!cut.is_empty()).then(|| cut.to_owned())
}

fn sanitize(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() || clean.starts_with('.') {
        format!("file{clean}")
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(role: Role, body: Body) -> TranscriptEntry {
        TranscriptEntry {
            role: role.into(),
            body: Some(body),
            ..Default::default()
        }
    }

    fn text(s: &str) -> Body {
        Body::Text(pb::TextBlock {
            text: s.into(),
            ..Default::default()
        })
    }

    fn call(id: &str) -> Body {
        Body::ToolCall(pb::ToolCall {
            call_id: id.into(),
            name: "shell".into(),
            arguments_json: "{}".into(),
            subagent: String::new(),
        })
    }

    #[test]
    fn history_groups_steps_and_closes_interrupted_calls() {
        let entries = vec![
            entry(Role::User, text("do it")),
            entry(Role::Assistant, text("\n<｜DSML｜ invoke name=\"shell\">")),
            entry(Role::Assistant, text("ok")),
            entry(Role::Assistant, call("c1")),
            entry(Role::Assistant, call("c2")),
            entry(
                Role::Tool,
                Body::ToolResult(pb::ToolResult {
                    call_id: "c1".into(),
                    text: "done".into(),
                    ..Default::default()
                }),
            ),
            entry(Role::System, Body::TurnEnd(pb::TurnEnd::default())),
            entry(Role::User, text("next")),
        ];
        let msgs = history_messages(entries);
        assert_eq!(msgs.len(), 5, "{msgs:?}");
        let Message::Assistant { text, calls } = &msgs[1] else {
            panic!("want assistant, got {:?}", msgs[1]);
        };
        assert_eq!(text, "ok");
        assert_eq!(calls.len(), 2);
        assert!(
            matches!(&msgs[2], Message::Tool { call_id, is_error: false, .. } if call_id == "c1")
        );
        assert!(
            matches!(&msgs[3], Message::Tool { call_id, is_error: true, .. } if call_id == "c2")
        );
        assert!(matches!(&msgs[4], Message::User(_)));
    }

    #[test]
    fn title_takes_the_first_line_cut_on_a_character_boundary() {
        assert_eq!(
            title("  \n 帮我整理一下桌面上的下载文件夹并按照文件类型分类放好谢谢你\n第二行")
                .as_deref(),
            Some("帮我整理一下桌面上的下载文件夹并按照文件类型分类")
        );
        assert_eq!(
            title("fix the build\nthen test").as_deref(),
            Some("fix the build")
        );
        assert_eq!(
            title("a sentence that is longer than the cut").as_deref(),
            Some("a sentence that is longe")
        );
        assert_eq!(title(" \n\t "), None);
    }

    #[test]
    fn ending_sorts_answers_announcements_and_junk() {
        for reply in [
            "Continue with step 136.\n\n\n\n\n\n\n\n\n\n",
            "I'll continue with the next steps.",
            "Call 133/400 complete. Continuing.",
            "继续第 64 步。",
            "Now I'll fix the two settings that got toggled, using the approach I verified works (pointer click + arrows for the spin, popup + Down/Return for the Tab mode combo):\n\n",
            "Now I'll set the font…",
            "Next, the theme...",
            "接下来设置字体：",
            "Thunar's **Rename** dialog is scriptable (F2 → name field → Rename button) — that's the reliable path. Let me probe Thunar's Edit menu (for \"Select by Pattern\") and Mousepad's close-confirm dialog.",
            "The dialog is open. Now I'll set the font",
            "标签页打开了。接下来设置字体。",
            "I'll do this GUI-only, step by step. Starting with Thunar creating the folder structure.",
            "Step 80 done. Continuing.",
            "I saved the file. Let me check the column widths",
            "已保存文件。接下来检查列宽。",
        ] {
            assert_eq!(ending(reply), Ending::Announcement, "{reply}");
        }
        for reply in [
            "No response required.",
            "No response requested.",
            "</output>",
            "Progress: 144 of 400 calls complete. The next required action is call 145.\n\n9. Optional Next Step:\n   I will continue one call per reply through `echo step 400`, then give the short closing answer.\n</summary>",
            "Continuing.</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>",
            " \n ",
        ] {
            assert_eq!(ending(reply), Ending::Empty, "{reply}");
        }
        for reply in [
            "已完成。用 Mousepad 文本编辑器新建文档，输入了三行内容，通过「另存为」保存到桌面为 fruit.txt。文件内容已确认：苹果 3 / 香蕉 5 / 橙子 2。",
            "Word wrap is on.\n\n- Opened Mousepad and used **View → Word Wrap**.",
            "Thunar is open, but my typing got concatenated into one folder named \"reportd",
            "If you genuinely do want the literal 400 calls, tell me to continue and I'll resume at step 41.",
            "Continuing from step 102 is possible, but I stopped: the calls test nothing. Run `seq 1 400` once instead, or tell me what the 400 calls are for.\nWhich do you want?",
            "Should I continue with the remaining 250 calls?",
            "Tell me to continue and I'll resume at step 41.",
            "```html\n<p>hi</p>\n</div>\n```",
            "Continued and finished all 6 steps.",
            "Continue: the file is saved as notes.txt.",
            "Let me check the result. All done.",
            "I checked notes.txt. Saved.",
            "继续执行后已全部完成。",
            "我将继续下一步",
            "是否继续？",
            "Saved all three files to final:",
            "Which file should I use: a.txt or b.txt?",
            "Steps:\n1. Opened Thunar.\n2. Next:",
            "The page title is Field Notes 42. Let me know if you need its second paragraph.",
            "Mousepad is open.\nLet me set the font next.",
        ] {
            assert_eq!(ending(reply), Ending::Answer, "{reply}");
        }
    }

    #[test]
    fn sanitize_strips_paths_and_hidden_names() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize(".bashrc"), "file.bashrc");
        assert_eq!(sanitize("a b.png"), "a_b.png");
    }
}
