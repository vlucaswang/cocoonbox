//! gRPC surface: Agents, Session and State behind the edge credential check.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::service::interceptor::InterceptedService;
use tonic::service::{Interceptor, Routes};
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status, Streaming};

use crate::config::{Automation, ServeConfig};
use crate::dispatch::Tools;
use crate::exec;
use crate::identity::{CREDENTIAL_HEADER, Denied, Gate, Principal};
use crate::llm;
use crate::mcp::Mcp;
use crate::offload;
use crate::pb::agents_server::{Agents, AgentsServer};
use crate::pb::session_server::{Session, SessionServer};
use crate::pb::state_server::{State, StateServer};
use crate::pb::{self, Event, fill_credential_request::Secret};
use crate::runner::Runtime;
use crate::screen;
use crate::shuttle::Split;
use crate::state::{Pause, StateEngine, StateError};
use crate::toolhost::rpc::{self, Call};

const MAX_REQUEST_BYTES: usize = 64 << 20;
// The api pages chunk URLs at 2048 a message, about 1 MB.
const MAX_STATE_MESSAGE_BYTES: usize = 16 << 20;
const DEFAULT_PAGE: u32 = 100;
const MAX_PAGE: u32 = 1000;
const EVENT_BUFFER: usize = 256;
const SCREEN: (u32, u32) = (1280, 800);
// redroid's boot size in image/android/base/bake.sh.
const ANDROID_SCREEN: (u32, u32) = (720, 1280);
// h2 frees connection credit only as a stream is read, so each stalled Exec stdin holds one stream window.
const STREAM_WINDOW: u32 = 1 << 20;
const CONNECTION_WINDOW: u32 = 64 << 20;
const CONNECTION_GRACE: Duration = Duration::from_millis(1000);
const DRAIN_LIMIT: Duration = Duration::from_millis(700);

/// Rejects calls the gate does not admit and puts the call's [`Principal`] in its extensions;
/// with `owner_only`, a share's call is refused too.
#[derive(Clone)]
pub struct EdgeAuth {
    gate: Arc<Gate>,
    owner_only: bool,
}

impl Interceptor for EdgeAuth {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        let md = req.metadata();
        let verdict = self
            .gate
            .check(md.get(CREDENTIAL_HEADER).and_then(|v| v.to_str().ok()));
        match verdict {
            Ok(Principal::Share { .. }) if self.owner_only => Err(share_refused()),
            Ok(principal) => {
                req.extensions_mut().insert(principal);
                Ok(req)
            }
            Err(Denied::Unprovisioned(m)) => Err(Status::unavailable(m)),
            Err(Denied::Unauthenticated(m)) => Err(Status::unauthenticated(m)),
        }
    }
}

/// `cocoonbox.agent.v1.Agents`.
pub struct AgentsSvc {
    rt: Arc<Runtime>,
    mcp: Arc<Mcp>,
}

#[tonic::async_trait]
impl Agents for AgentsSvc {
    async fn create(
        &self,
        req: Request<pb::CreateAgentRequest>,
    ) -> Result<Response<pb::Agent>, Status> {
        self.rt
            .create_agent(req.into_inner())
            .await
            .map(Response::new)
    }

    async fn list(
        &self,
        _req: Request<pb::ListAgentsRequest>,
    ) -> Result<Response<pb::ListAgentsResponse>, Status> {
        let agents = self
            .rt
            .journal
            .store
            .list_agents()
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(pb::ListAgentsResponse { agents }))
    }

    async fn send(
        &self,
        req: Request<pb::SendRequest>,
    ) -> Result<Response<pb::SendResponse>, Status> {
        let turn_id = self.rt.send(req.into_inner()).await?;
        Ok(Response::new(pb::SendResponse { turn_id }))
    }

    async fn transcript(
        &self,
        req: Request<pb::TranscriptRequest>,
    ) -> Result<Response<pb::TranscriptPage>, Status> {
        let req = req.into_inner();
        let limit = match req.limit {
            0 => DEFAULT_PAGE,
            n => n.min(MAX_PAGE),
        };
        self.rt
            .journal
            .store
            .transcript(req.agent_id, req.after_seq, limit)
            .await
            .map(Response::new)
            .map_err(Status::internal)
    }

    async fn respond(
        &self,
        req: Request<pb::RespondRequest>,
    ) -> Result<Response<pb::RespondResponse>, Status> {
        self.rt.respond(req.into_inner())?;
        Ok(Response::new(pb::RespondResponse {}))
    }

    async fn cancel(
        &self,
        req: Request<pb::CancelRequest>,
    ) -> Result<Response<pb::CancelResponse>, Status> {
        self.rt.cancel(&req.into_inner().agent_id);
        Ok(Response::new(pb::CancelResponse {}))
    }

    async fn rename(
        &self,
        req: Request<pb::RenameAgentRequest>,
    ) -> Result<Response<pb::Agent>, Status> {
        self.rt.rename(req.into_inner()).await.map(Response::new)
    }

    async fn delete(
        &self,
        req: Request<pb::DeleteAgentRequest>,
    ) -> Result<Response<pb::DeleteAgentResponse>, Status> {
        let agent_id = req.into_inner().agent_id;
        if self.mcp.serves(&agent_id) {
            return Err(Status::failed_precondition(format!(
                "agent {agent_id} has an open MCP session; close the client's session first"
            )));
        }
        self.rt.delete(&agent_id).await?;
        Ok(Response::new(pb::DeleteAgentResponse {}))
    }

    async fn end(&self, req: Request<pb::EndRequest>) -> Result<Response<pb::EndResponse>, Status> {
        match req.into_inner().target {
            Some(pb::end_request::Target::AgentId(id)) => self.mcp.end_agent(&id).await?,
            Some(pb::end_request::Target::ShareId(id)) => self.mcp.end_share(&id).await,
            None => {
                return Err(Status::invalid_argument(
                    "end needs an agent_id or a share_id",
                ));
            }
        }
        Ok(Response::new(pb::EndResponse {}))
    }
}

/// `cocoonbox.agent.v1.Session`.
pub struct SessionSvc {
    rt: Arc<Runtime>,
    tools: rpc::Client,
}

impl SessionSvc {
    /// Runs a toolhost call outside any transcript.
    async fn toolhost(&self, tool: &str, args: Value) -> Result<String, Status> {
        let mut call = Call {
            agent_id: String::new(),
            tool: tool.to_owned(),
            args,
        };
        let out = self.tools.call(&call).await;
        rpc::wipe(&mut call.args);
        match out {
            Ok(out) if out.is_error => Err(Status::failed_precondition(out.text)),
            Ok(out) => Ok(out.text),
            Err(e) => Err(Status::unavailable(e)),
        }
    }
}

#[tonic::async_trait]
impl Session for SessionSvc {
    type EventsStream = ReceiverStream<Result<Event, Status>>;
    type ScreenStream = screen::ServerStream;
    type ExecStream = exec::ServerStream;

    async fn hello(
        &self,
        req: Request<pb::HelloRequest>,
    ) -> Result<Response<pb::HelloResponse>, Status> {
        owner(&req)?;
        let cfg = &self.rt.cfg;
        let mut models = vec![cfg.model_main.clone()];
        if cfg.model_computer != cfg.model_main {
            models.push(cfg.model_computer.clone());
        }
        let (automation, (screen_width, screen_height)) = match cfg.automation {
            Automation::Plain => (pb::Automation::Plain, SCREEN),
            Automation::Desk => (pb::Automation::Desk, SCREEN),
            Automation::Android => (pb::Automation::Android, ANDROID_SCREEN),
        };
        Ok(Response::new(pb::HelloResponse {
            agent_version: env!("CB_AGENT_VERSION").to_owned(),
            screen_width,
            screen_height,
            models,
            default_main_model: cfg.model_main.clone(),
            automation: automation.into(),
        }))
    }

    async fn events(
        &self,
        req: Request<pb::EventsRequest>,
    ) -> Result<Response<Self::EventsStream>, Status> {
        owner(&req)?;
        let sub = self
            .rt
            .journal
            .bus
            .subscribe(req.into_inner().from_offset)
            .map_err(Status::out_of_range)?;
        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let stop = self.rt.stop.clone();
        // detached pump: ends when the client goes away, falls behind the live channel, or the agent stops
        tokio::spawn(async move {
            let mut last = 0;
            for e in sub.replay {
                last = e.offset;
                if tx.send(Ok(Event::clone(&e))).await.is_err() {
                    return;
                }
            }
            let mut live = sub.live;
            loop {
                let next = tokio::select! {
                    r = live.recv() => r,
                    () = stop.cancelled() => return,
                };
                match next {
                    Ok(e) if e.offset > last => {
                        last = e.offset;
                        if tx.send(Ok(Event::clone(&e))).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let msg =
                            format!("event stream fell behind; reconnect with from_offset {last}");
                        let _ = tx.send(Err(Status::data_loss(msg))).await;
                        return;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn screen(
        &self,
        req: Request<Streaming<pb::ScreenClientFrame>>,
    ) -> Result<Response<Self::ScreenStream>, Status> {
        screen::relay(
            &self.rt.cfg.vnc_addr,
            req.into_inner(),
            self.rt.stop.clone(),
        )
        .await
        .map(Response::new)
    }

    async fn active_origin(
        &self,
        req: Request<pb::ActiveOriginRequest>,
    ) -> Result<Response<pb::ActiveOriginResponse>, Status> {
        owner(&req)?;
        let origin = self.toolhost("active_origin", json!({})).await?;
        Ok(Response::new(pb::ActiveOriginResponse { origin }))
    }

    async fn fill_credential(
        &self,
        req: Request<pb::FillCredentialRequest>,
    ) -> Result<Response<pb::FillCredentialResponse>, Status> {
        owner(&req)?;
        let mut req = req.into_inner();
        let mut args = json!({
            "origin": req.origin,
            "username": std::mem::take(&mut req.username),
            "submit": req.submit,
        });
        match req.secret.take() {
            Some(Secret::Password(p)) => args["password"] = p.into(),
            Some(Secret::Code(c)) => args["code"] = c.into(),
            None => return Err(Status::invalid_argument("fill needs a password or a code")),
        }
        let text = self.toolhost("credential_fill", args).await?;
        let out: Value =
            serde_json::from_str(&text).map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(pb::FillCredentialResponse {
            filled: out["filled"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            submitted: out["submitted"].as_bool().unwrap_or(false),
        }))
    }

    async fn exec(
        &self,
        req: Request<Streaming<pb::ExecClientFrame>>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        owner(&req)?;
        exec::relay(&self.tools, req.into_inner(), self.rt.stop.clone())
            .await
            .map(Response::new)
    }
}

/// `cocoonbox.agent.v1.State`, called by the api to move user state to and from object storage.
pub struct StateSvc {
    rt: Arc<Runtime>,
    engine: Arc<StateEngine>,
}

type ProgressStream = ReceiverStream<Result<pb::TransferProgress, Status>>;

#[tonic::async_trait]
impl State for StateSvc {
    type UploadStream = ProgressStream;
    type RestoreStream = ProgressStream;
    type RestorePathsStream = ProgressStream;

    async fn plan(
        &self,
        req: Request<pb::PlanRequest>,
    ) -> Result<Response<pb::PlanResponse>, Status> {
        self.engine
            .plan(req.into_inner())
            .await
            .map(Response::new)
            .map_err(state_status)
    }

    async fn upload(
        &self,
        req: Request<pb::UploadRequest>,
    ) -> Result<Response<Self::UploadStream>, Status> {
        let engine = Arc::clone(&self.engine);
        let req = req.into_inner();
        Ok(Response::new(progress_stream(move |tx| async move {
            engine.upload(req, tx).await
        })))
    }

    async fn restore(
        &self,
        req: Request<Streaming<pb::RestoreRequest>>,
    ) -> Result<Response<Self::RestoreStream>, Status> {
        let engine = Arc::clone(&self.engine);
        let rt = Arc::clone(&self.rt);
        let reqs = req
            .into_inner()
            .map(|r| r.map_err(|s| format!("restore stream: {}", s.message())));
        Ok(Response::new(progress_stream(move |tx| async move {
            engine.restore(reqs, tx).await?;
            if let Err(e) = offload::inbox::rescan(&rt.cfg.offload_socket).await
                && rt.cfg.offload_socket.exists()
            {
                eprintln!("restore: tell the offload inbox: {e}");
            }
            rt.recover("interrupted: the user state was restored")
                .await
                .map_err(StateError::Failed)
        })))
    }

    async fn restore_paths(
        &self,
        req: Request<Streaming<pb::RestoreRequest>>,
    ) -> Result<Response<Self::RestorePathsStream>, Status> {
        let engine = Arc::clone(&self.engine);
        let reqs = req
            .into_inner()
            .map(|r| r.map_err(|s| format!("restore stream: {}", s.message())));
        Ok(Response::new(progress_stream(move |tx| async move {
            engine.restore_paths(reqs, tx).await
        })))
    }
}

/// Binds `AGENT_LISTEN` and serves until SIGTERM or SIGINT.
pub async fn serve(cfg: ServeConfig) -> Result<(), String> {
    let listener = TcpListener::bind(cfg.listen)
        .await
        .map_err(|e| format!("bind {}: {e}", cfg.listen))?;
    eprintln!("serve listening on {}", cfg.listen);
    let model = Arc::new(llm::Client::new(
        &cfg.gw_base_url,
        &cfg.gw_api_key,
        cfg.anthropic_messages,
    )?);
    let toolhost = rpc::Client::new(cfg.toolhost_socket.clone());
    let tools: Arc<dyn Tools> = match &cfg.shuttle_socket {
        Some(shuttle) => Arc::new(Split::new(
            rpc::Client::new(shuttle.clone()),
            toolhost,
            cfg.workspace.clone(),
            cfg.plugins.iter().map(|p| p.name.clone()).collect(),
        )),
        None => Arc::new(toolhost),
    };
    let rt = Runtime::open(cfg, model, tools).await?;
    let stop = rt.stop.clone();
    // detached: turns the first SIGTERM or SIGINT into a shutdown
    tokio::spawn(async move {
        shutdown_signal().await;
        eprintln!("serve: stopping");
        stop.cancel();
    });
    serve_on(listener, rt).await
}

/// Serves both services on `listener` until `shutdown` resolves.
/// Serves both services and `/mcp` on `listener` until `rt.stop` is cancelled, then winds down within a bounded time.
pub async fn serve_on(listener: TcpListener, rt: Arc<Runtime>) -> Result<(), String> {
    let gate = Arc::new(Gate::new(
        rt.cfg.identity_path.clone(),
        rt.cfg.identity_owner,
    ));
    let auth = EdgeAuth {
        gate: Arc::clone(&gate),
        owner_only: true,
    };
    let screen_auth = EdgeAuth {
        gate: Arc::clone(&gate),
        owner_only: false,
    };
    let mcp = Mcp::new(Arc::clone(&rt), rt.cfg.mcp_idle, gate);
    mcp.spawn_reaper();
    // detached: ends with rt.stop
    tokio::spawn(offload::inbox::relay(
        rt.cfg.offload_socket.clone(),
        Arc::clone(&rt.journal),
        rt.stop.clone(),
    ));
    let agents = AgentsServer::new(AgentsSvc {
        rt: Arc::clone(&rt),
        mcp: Arc::clone(&mcp),
    })
    .max_decoding_message_size(MAX_REQUEST_BYTES);
    let session = SessionServer::new(SessionSvc {
        rt: Arc::clone(&rt),
        tools: rpc::Client::new(rt.cfg.toolhost_socket.clone()),
    });
    let engine = StateEngine::new(
        rpc::Client::new(rt.cfg.toolhost_socket.clone()),
        rt.journal.store.clone(),
        rt.cfg.state_roots.clone(),
        rt.cfg.state_dir.join("state"),
        Pause {
            turns: Arc::clone(&rt.turns),
            guests: Arc::clone(&rt.guests),
            wait: rt.cfg.quiesce_wait,
        },
        rt.cfg.desktop_unit.clone(),
        rt.cfg.automation == Automation::Android,
    )?;
    let state = StateServer::new(StateSvc {
        rt: Arc::clone(&rt),
        engine: Arc::new(engine),
    })
    .max_decoding_message_size(MAX_STATE_MESSAGE_BYTES);
    let routes = Routes::new(InterceptedService::new(agents, auth.clone()))
        .add_service(InterceptedService::new(session, screen_auth))
        .add_service(InterceptedService::new(state, auth))
        .into_axum_router()
        .merge(Arc::clone(&mcp).router());
    let server = Server::builder()
        .initial_stream_window_size(STREAM_WINDOW)
        .initial_connection_window_size(CONNECTION_WINDOW)
        .accept_http1(true)
        .add_routes(Routes::from(routes))
        .serve_with_incoming_shutdown(incoming(listener), rt.stop.clone().cancelled_owned());
    let grace = async {
        rt.stop.cancelled().await;
        tokio::time::sleep(CONNECTION_GRACE).await;
    };
    tokio::select! {
        r = server => r.map_err(|e| format!("grpc server: {e}"))?,
        () = grace => eprintln!("serve: connections still open after {CONNECTION_GRACE:?}; closing"),
    }
    mcp.close_all().await;
    rt.drain(DRAIN_LIMIT).await;
    Ok(())
}

fn incoming(listener: TcpListener) -> TcpIncoming {
    TcpIncoming::from(listener).with_nodelay(Some(true))
}

/// Refuses `req` unless the owner sent it: a share reaches only `Session.Screen` and MCP.
fn owner<T>(req: &Request<T>) -> Result<(), Status> {
    match req.extensions().get::<Principal>() {
        Some(Principal::Owner) => Ok(()),
        _ => Err(share_refused()),
    }
}

fn share_refused() -> Status {
    Status::permission_denied("a share reaches only the screen and MCP")
}

/// Resolves on the first SIGTERM or SIGINT.
pub(crate) async fn shutdown_signal() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = term => {}
    }
}

/// Runs a transfer and streams its progress; a failure ends the stream with a status.
fn progress_stream<F, Fut>(op: F) -> ProgressStream
where
    F: FnOnce(mpsc::Sender<pb::TransferProgress>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<(), StateError>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(EVENT_BUFFER);
    // detached: the transfer runs to completion even if the caller stops reading
    tokio::spawn(async move {
        let (ptx, mut prx) = mpsc::channel(EVENT_BUFFER);
        let out = tx.clone();
        let relay = tokio::spawn(async move {
            while let Some(p) = prx.recv().await {
                let _ = out.send(Ok(p)).await;
            }
        });
        let result = op(ptx).await;
        let _ = relay.await;
        if let Err(e) = result {
            let _ = tx.send(Err(state_status(e))).await;
        }
    });
    ReceiverStream::new(rx)
}

fn state_status(e: StateError) -> Status {
    match e {
        StateError::Busy(m) => Status::unavailable(m),
        StateError::Invalid(m) => Status::invalid_argument(m),
        StateError::Failed(m) => Status::internal(m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepted_sockets_send_without_delay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut accepted = incoming(listener);
        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let stream = accepted.next().await.unwrap().unwrap();
        assert!(
            stream.nodelay().unwrap(),
            "TCP_NODELAY is off on an accepted socket"
        );
    }
}
