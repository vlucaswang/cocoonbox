#![cfg(test)]

mod common;

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use cocoonbox_agent::config::{Automation, ServeConfig, ToolhostConfig};
use cocoonbox_agent::pb::agents_client::AgentsClient;
use cocoonbox_agent::pb::session_client::SessionClient;
use cocoonbox_agent::pb::{
    self, event, exec_client_frame, exec_server_frame, screen_client_frame, transcript_entry::Body,
};
use cocoonbox_agent::runner::Runtime;
use cocoonbox_agent::toolhost::state::Root;
use cocoonbox_agent::{offload, server, toolhost};
use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

use common::{owner, sign_credential, write_identity};

const FAKE_NODE: &str = r#"src=$(cat "$CB_SCRIPT")
case "$src" in
*boom*) echo "Error: boom" >&2; exit 1 ;;
*hang*) sleep 300 & echo $! > grandchild.pid; sleep 300 ;;
*leftover*) sleep 300 & echo $! > leftover.pid; echo left; printf 7 > "$CB_RESULT" ;;
*) echo "ran $1 in $PWD"; printf '{"params":%s}' "$CB_PARAMS" > "$CB_RESULT"; printf outline > "$CB_SNAPSHOT" ;;
esac"#;
const DEMO_ROUTING: &str = "Loops over demo items go to demo_run.";
const PNG: &str = "iVBORw0KGgoAAAANSUhEUg==";
const FAKE_ADB: &str = r#"echo "$*" >> "$0.log"
case "$*" in
"exec-out screencap -p") printf 'phone-png' ;;
*cb.Ui) echo ready; while read -r _; do echo '<hierarchy><node text="Settings" class="android.widget.TextView" content-desc="" clickable="true" path="0" bounds="[400,400][600,480]" /></hierarchy>'; echo '<<end>>'; done ;;
esac"#;
const THUMB: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGP4DwQACfsD/fteaysAAAAASUVORK5CYII=";
const TARGET_TOOLS: [&str; 7] = [
    "move_cursor",
    "click",
    "drag",
    "scroll",
    "type_text",
    "press_key",
    "hotkey",
];

type Log = Arc<Mutex<Vec<(String, Value)>>>;

struct Harness {
    _chrome: Option<ChromeGuard>,
    _dir: tempfile::TempDir,
    identity: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
    endpoint: String,
    gw_log: Log,
    cua_log: Log,
    session_save: PathBuf,
    cdp_port: u16,
}

impl Harness {
    async fn start(script: Vec<(&'static str, String)>, anthropic_messages: bool) -> Self {
        let day = Duration::from_secs(24 * 3600);
        Self::start_with(script, anthropic_messages, Duration::from_secs(600), day).await
    }

    async fn start_with(
        script: Vec<(&'static str, String)>,
        anthropic_messages: bool,
        mcp_idle: Duration,
        mcp_prune_after: Duration,
    ) -> Self {
        Self::boot(
            script,
            anthropic_messages,
            mcp_idle,
            mcp_prune_after,
            None,
            None,
            Automation::Plain,
            false,
        )
        .await
    }

    /// A harness whose toolhost drives a headless `chrome`.
    async fn with_chrome(script: Vec<(&'static str, String)>, chrome: &Path) -> Self {
        let day = Duration::from_secs(24 * 3600);
        Self::boot(
            script,
            false,
            Duration::from_secs(600),
            day,
            Some(chrome),
            None,
            Automation::Plain,
            false,
        )
        .await
    }

    /// A harness whose loop reaches the Connect box door at `connect_url`.
    async fn with_connect(script: Vec<(&'static str, String)>, connect_url: String) -> Self {
        let day = Duration::from_secs(24 * 3600);
        Self::boot(
            script,
            false,
            Duration::from_secs(600),
            day,
            None,
            Some(connect_url),
            Automation::Plain,
            false,
        )
        .await
    }

    /// A desk box whose image adds the plugin `demo_run`, its guide resource and a routing.d paragraph.
    async fn with_plugin(script: Vec<(&'static str, String)>) -> Self {
        let day = Duration::from_secs(24 * 3600);
        Self::boot(
            script,
            false,
            Duration::from_secs(600),
            day,
            None,
            None,
            Automation::Desk,
            true,
        )
        .await
    }

    /// A harness of an Android box, whose toolhost drives a fake adb.
    async fn with_android() -> Self {
        let day = Duration::from_secs(24 * 3600);
        Self::boot(
            Vec::new(),
            false,
            Duration::from_secs(600),
            day,
            None,
            None,
            Automation::Android,
            false,
        )
        .await
    }

    /// A harness of a box with the desk runtime, which runs GUI plans and scripts on Node.
    async fn with_desk(script: Vec<(&'static str, String)>) -> Self {
        let day = Duration::from_secs(24 * 3600);
        Self::boot(
            script,
            false,
            Duration::from_secs(600),
            day,
            None,
            None,
            Automation::Desk,
            false,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn boot(
        script: Vec<(&'static str, String)>,
        anthropic_messages: bool,
        mcp_idle: Duration,
        mcp_prune_after: Duration,
        chrome: Option<&Path>,
        connect_url: Option<String>,
        automation: Automation,
        plugin: bool,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let identity = dir.path().join("identity").join("identity");
        common::provision(&identity);
        let gw_log = Log::default();
        let gw = mock_gw(script, Arc::clone(&gw_log)).await;
        let cua_log = Log::default();
        let cua_socket = dir.path().join("cua.sock");
        fake_cua(&cua_socket, Arc::clone(&cua_log));
        let session_save = dir.path().join("session-save");
        write_script(&session_save, "echo saved >> \"$0.log\"");
        let script_bin = dir.path().join("script-bin");
        write_script(&script_bin, FAKE_NODE);
        let adb_bin = dir.path().join("adb");
        write_script(&adb_bin, FAKE_ADB);
        let tools_dir = dir.path().join("tools.d");
        let routing_dir = dir.path().join("routing.d");
        if plugin {
            let plugin_bin = dir.path().join("demo-bin");
            write_script(
                &plugin_bin,
                &format!("echo \"$1\" >> plugin.log\n{FAKE_NODE}"),
            );
            let guide = dir.path().join("DEMO.md");
            std::fs::write(&guide, "demo guide").unwrap();
            std::fs::create_dir_all(&tools_dir).unwrap();
            let spec = json!({"name": "demo_run", "description": "Run a demo script.", "bin": plugin_bin,
                "resources": [{"uri": "cocoonbox://demo/DEMO.md", "path": guide, "mime": "text/markdown", "description": "The demo guide"}]});
            std::fs::write(tools_dir.join("demo.json"), spec.to_string()).unwrap();
            std::fs::create_dir_all(&routing_dir).unwrap();
            std::fs::write(routing_dir.join("demo.md"), DEMO_ROUTING).unwrap();
        }
        let plugins = cocoonbox_agent::plugins::load(&tools_dir);
        let toolhost_socket = dir.path().join("toolhost.sock");
        let (chrome_bin, cdp_port) = match chrome {
            Some(bin) => {
                let wrapper = dir.path().join("chrome-headless");
                write_script(
                    &wrapper,
                    &format!(
                        "exec \"{}\" --headless=new --use-mock-keychain \"$@\"",
                        bin.display()
                    ),
                );
                (wrapper.display().to_string(), cdp_port())
            }
            None => ("false".to_owned(), 1),
        };
        let th_cfg = ToolhostConfig {
            socket: toolhost_socket.clone(),
            socket_group: "no-such-group".into(),
            workspace: workspace.clone(),
            chrome_bin,
            chrome_profile: dir.path().join("chrome"),
            cdp_port,
            cua_socket,
            script_bin,
            session_save: session_save.clone(),
            home: home.clone(),
            automation,
            plugins: plugins.clone(),
            adb_bin,
            gw_base_url: None,
            shuttle_socket: None,
        };
        tokio::spawn(async move { toolhost::run(th_cfg).await });
        let cfg = ServeConfig {
            gw_base_url: format!("http://{gw}/v1"),
            gw_api_key: "k".into(),
            model_main: "deepseek/deepseek-v4.1-flash".into(),
            model_computer: "anthropic/claude-sonnet-5".into(),
            identity_path: identity.clone(),
            identity_owner: std::os::unix::fs::MetadataExt::uid(&dir.path().metadata().unwrap()),
            listen: "127.0.0.1:0".parse().unwrap(),
            state_dir: dir.path().join("state"),
            toolhost_socket: toolhost_socket.clone(),
            offload_socket: dir.path().join("offload.sock"),
            vnc_addr: fake_vnc().await,
            anthropic_messages,
            mcp_idle,
            mcp_prune_after,
            quiesce_wait: Duration::from_secs(1),
            desktop_unit: None,
            connect_url,
            automation,
            plugins,
            routing_extra: cocoonbox_agent::config::routing_extra(&routing_dir),
            state_roots: vec![
                Root {
                    name: "home".into(),
                    path: home.clone(),
                },
                Root {
                    name: "workspace".into(),
                    path: workspace.clone(),
                },
            ],
        };
        let model = Arc::new(
            cocoonbox_agent::llm::Client::new(
                &cfg.gw_base_url,
                &cfg.gw_api_key,
                anthropic_messages,
            )
            .unwrap(),
        );
        let tools = Arc::new(cocoonbox_agent::toolhost::rpc::Client::new(
            cfg.toolhost_socket.clone(),
        ));
        let rt = Runtime::open(cfg, model, tools).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(server::serve_on(listener, rt));
        for _ in 0..100 {
            if toolhost_socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            _chrome: chrome.map(|_| ChromeGuard(dir.path().join("chrome"))),
            _dir: dir,
            identity,
            workspace,
            home,
            endpoint,
            gw_log,
            cua_log,
            session_save,
            cdp_port,
        }
    }

    async fn channel(&self) -> Channel {
        Channel::from_shared(self.endpoint.clone())
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    async fn agents(&self) -> AgentsClient<Channel> {
        AgentsClient::new(self.channel().await)
    }

    async fn session(&self) -> SessionClient<Channel> {
        SessionClient::new(self.channel().await)
    }
}

fn authed<T>(msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut().insert(
        "x-cocoonbox-credential",
        MetadataValue::from_static(owner()),
    );
    req
}

async fn create_agent(agents: &mut AgentsClient<Channel>) -> String {
    agents
        .create(authed(pb::CreateAgentRequest {
            name: "t".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .id
}

async fn send(
    agents: &mut AgentsClient<Channel>,
    agent_id: &str,
    text: &str,
    nonce: &str,
) -> String {
    agents
        .send(authed(pb::SendRequest {
            agent_id: agent_id.into(),
            text: text.into(),
            client_nonce: nonce.into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .turn_id
}

async fn wait_turn_end(
    agents: &mut AgentsClient<Channel>,
    agent_id: &str,
) -> Vec<pb::TranscriptEntry> {
    for _ in 0..500 {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.into(),
                after_seq: 0,
                limit: 1000,
            }))
            .await
            .unwrap()
            .into_inner();
        if page
            .entries
            .iter()
            .any(|e| matches!(e.body, Some(Body::TurnEnd(_))))
        {
            return page.entries;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("turn did not end");
}

fn chat_tool_call(id: &str, name: &str, args: &Value) -> String {
    let args = args.to_string();
    let (a, b) = args.split_at(args.len() / 2);
    sse(&[
        json!({"choices":[{"delta":{"content":"Let me check."}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":a}}]}}]}),
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":b}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":10}}),
    ]) + "data: [DONE]\n\n"
}

fn decision(tier: &str, confidence: f64) -> String {
    json!({"model": "typesafe/jev-1.14-20261014", "answers": {"tier": {"type": "choice", "choice": tier, "confidence": confidence, "probabilities": {tier: confidence}}}, "usage": {"cost": 0.00004, "input_tokens": 1000, "output_tokens": 50}}).to_string()
}

fn chat_text(text: &str) -> String {
    sse(&[
        json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":150,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":100}}}),
    ]) + "data: [DONE]\n\n"
}

fn chat_reasoning(text: &str) -> String {
    sse(&[
        json!({"choices":[{"delta":{"reasoning_content":text}}]}),
        json!({"choices":[{"delta":{"content":""},"finish_reason":"stop"}]}),
        json!({"choices":[],"usage":{"prompt_tokens":120,"completion_tokens":30}}),
    ]) + "data: [DONE]\n\n"
}

fn messages_tool_use(id: &str, name: &str, input: &Value) -> String {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":50,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ])
}

fn messages_text(text: &str) -> String {
    sse(&[
        json!({"type":"message_start","message":{"usage":{"input_tokens":60,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ])
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

async fn mock_gw(script: Vec<(&'static str, String)>, log: Log) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let mut queues: HashMap<String, VecDeque<String>> = HashMap::new();
    for (path, body) in script {
        queues
            .entry(format!("/v1/{path}"))
            .or_default()
            .push_back(body);
    }
    let queues = Arc::new(Mutex::new(queues));
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let queues = Arc::clone(&queues);
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).await.unwrap();
                    if h == "\r\n" {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).await.unwrap();
                log.lock()
                    .unwrap()
                    .push((path.clone(), serde_json::from_slice(&body).unwrap()));
                let next = queues
                    .lock()
                    .unwrap()
                    .get_mut(&path)
                    .and_then(VecDeque::pop_front);
                let mut stream = reader.into_inner();
                let resp = match next {
                    Some(sse) => format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{sse}"
                    ),
                    None => "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 11\r\nconnection: close\r\n\r\nno script\n".to_owned(),
                };
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            });
        }
    });
    addr
}

/// The Connect box door: a read tool, a write tool the broker holds for approval, a tool named like a box tool, and
/// its event stream, which carries each message sent on `events`, ends on `close` and refuses its first `refused` readers; a `list_changed` also adds a tool.
async fn mock_door(
    log: Log,
    events: tokio::sync::broadcast::Sender<String>,
    refused: usize,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let added = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let born = std::time::Instant::now();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (log, mut events, added, readers) = (
                Arc::clone(&log),
                events.subscribe(),
                Arc::clone(&added),
                Arc::clone(&readers),
            );
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let (mut len, mut session) = (0, String::new());
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).await.unwrap();
                    if h == "\r\n" {
                        break;
                    }
                    let lower = h.to_ascii_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if let Some(v) = lower.strip_prefix("mcp-session-id:") {
                        session = v.trim().to_owned();
                    }
                }
                let mut body = vec![0u8; len];
                reader.read_exact(&mut body).await.unwrap();
                let mut stream = reader.into_inner();
                if line.starts_with("GET")
                    && readers.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < refused
                {
                    log.lock().unwrap().push((
                        "GET refused".into(),
                        json!({"ms": born.elapsed().as_millis()}),
                    ));
                    stream
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                        .await
                        .unwrap();
                    return;
                }
                if line.starts_with("GET") {
                    log.lock()
                        .unwrap()
                        .push(("GET".into(), json!({"session": session})));
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n: open\n\n")
                        .await
                        .unwrap();
                    while let Ok(msg) = events.recv().await {
                        if msg == "close" {
                            return;
                        }
                        if msg.contains("list_changed") {
                            added.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        let frame = format!("event: message\ndata: {msg}\n\n");
                        if stream.write_all(frame.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
                let req: Value = serde_json::from_slice(&body).unwrap();
                let method = req["method"].as_str().unwrap_or_default().to_owned();
                log.lock().unwrap().push((
                    method.clone(),
                    json!({"session": session, "params": req["params"]}),
                ));
                let result = match method.as_str() {
                    "initialize" => {
                        json!({"protocolVersion": "2025-11-25", "capabilities": {"tools": {"listChanged": true}}, "serverInfo": {"name": "cocoonbox-connect", "version": "1"}})
                    }
                    "tools/list" => {
                        let mut tools = vec![
                            json!({"name": "mock_list_items", "description": "List items", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": true}}),
                            json!({"name": "mock_create_item", "description": "Create an item", "inputSchema": {"type": "object", "properties": {"title": {"type": "string"}}}}),
                            json!({"name": "shell", "description": "not the box shell", "inputSchema": {"type": "object"}}),
                        ];
                        if added.load(std::sync::atomic::Ordering::SeqCst) {
                            tools.push(json!({"name": "mock_new", "description": "New", "inputSchema": {"type": "object"}}));
                        }
                        json!({"tools": tools})
                    }
                    "tools/call" => {
                        let p = &req["params"];
                        match (
                            p["name"].as_str(),
                            p["_meta"]["cocoonbox/approval_id"].as_str(),
                        ) {
                            (Some("mock_list_items"), _) => {
                                json!({"content": [{"type": "text", "text": "items: a"}], "isError": false})
                            }
                            (Some("mock_create_item"), None) => {
                                let id = format!("apr_{}", log.lock().unwrap().len());
                                json!({"content": [{"type": "text", "text": "Approval required"}], "isError": true,
                                       "_meta": {"cocoonbox/approval": {"approval_id": id, "summary": format!("Mock: create_item (title={})", p["arguments"]["title"])}}})
                            }
                            (Some("mock_create_item"), Some(id)) => {
                                json!({"content": [{"type": "text", "text": format!("created {} with {id}", p["arguments"]["title"])}], "isError": false})
                            }
                            _ => {
                                json!({"content": [{"type": "text", "text": "unknown"}], "isError": true})
                            }
                        }
                    }
                    _ => {
                        stream.write_all(b"HTTP/1.1 202 Accepted\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await.unwrap();
                        return;
                    }
                };
                let body = json!({"jsonrpc": "2.0", "id": req["id"], "result": result}).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nmcp-session-id: s1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    url
}

fn fake_cua(path: &Path, log: Log) {
    let listener = UnixListener::bind(path).unwrap();
    let ended: Arc<Mutex<HashSet<String>>> = Arc::default();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let log = Arc::clone(&log);
            let ended = Arc::clone(&ended);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let req: Value = serde_json::from_str(&line).unwrap();
                let name = req["name"].as_str().unwrap().to_owned();
                log.lock()
                    .unwrap()
                    .push((name.clone(), req["args"].clone()));
                let session = req["args"]["session"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let mut stream = reader.into_inner();
                if name == "end_session" {
                    ended.lock().unwrap().insert(session);
                } else if ended.lock().unwrap().contains(&session) {
                    let error = format!(
                        "session has ended; Call start_session with session '{session}' to start it again, or use a new session label"
                    );
                    let reply = json!({"ok": false, "error": error}).to_string() + "\n";
                    stream.write_all(reply.as_bytes()).await.unwrap();
                    return;
                }
                let result = match name.as_str() {
                    n if req["args"]["target"].is_object() && !TARGET_TOOLS.contains(&n) => json!({
                        "isError": true,
                        "content": [{"type": "text", "text": format!("{n} does not accept a per-call target")}]
                    }),
                    "get_window_state" => json!({
                        "content": [{"type": "text", "text": "tree"}],
                        "structuredContent": {"window_id": 7, "window_bounds": {"x": 100, "y": 50, "width": 800, "height": 600}, "elements": [{"element_index": 0, "element_token": "tok-0", "role": "text", "label": "editor", "depth": 0, "frame": {"x": 10, "y": 20, "w": 100, "h": 40}}]}
                    }),
                    "get_desktop_state" if req["args"]["max_image_dimension"] == 160 => json!({
                        "content": [{"type": "image", "mimeType": "image/png", "data": THUMB}]
                    }),
                    "get_desktop_state" => json!({
                        "content": [{"type": "image", "mimeType": "image/png", "data": PNG}],
                        "structuredContent": {
                            "windows": fake_windows(),
                            "frame_scale": req["args"]["max_image_dimension"].as_f64().map_or(1.0, |edge| 1280.0 / edge)
                        }
                    }),
                    "list_windows" => json!({"structuredContent": {"windows": fake_windows()}}),
                    _ => json!({"content": [{"type": "text", "text": format!("✅ {name} done")}]}),
                };
                let reply = json!({"ok": true, "result": result}).to_string() + "\n";
                stream.write_all(reply.as_bytes()).await.unwrap();
            });
        }
    });
}

fn fake_windows() -> Value {
    json!([
        {"pid": 1, "window_id": 2, "app_name": "Xfdesktop", "title": "Desktop", "bounds": {"x": 0, "y": 0, "width": 1280, "height": 800}, "z_index": 0},
        {"pid": 42, "window_id": 7, "app_name": "Mousepad", "title": "Untitled", "bounds": {"x": 100, "y": 50, "width": 800, "height": 600}, "z_index": 3},
        {"pid": 5, "window_id": 9, "app_name": "Xfce4-panel", "title": "hidden", "bounds": {"x": -9999, "y": -9999, "width": 3, "height": 3}, "z_index": 4},
        {"pid": 5, "window_id": 10, "app_name": "Xfce4-panel", "title": "panel", "bounds": {"x": 0, "y": 0, "width": 1280, "height": 27}, "z_index": 5}
    ])
}

async fn fake_vnc() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                stream.write_all(b"RFB 003.008\n").await.unwrap();
                let mut buf = [0u8; 64];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

/// Kills the test's Chrome, found by its profile path, even when the test fails.
struct ChromeGuard(PathBuf);

impl Drop for ChromeGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("pkill")
            .arg("-f")
            .arg(self.0.as_os_str())
            .status();
    }
}

/// The Chrome to test with; `None` skips the test, which only macOS or `CB_NO_BROWSER` allow.
fn test_chrome() -> Option<PathBuf> {
    let chrome = find_chrome();
    if chrome.is_none() {
        assert!(
            cfg!(target_os = "macos") || std::env::var_os("CB_NO_BROWSER").is_some(),
            "no Chrome found: set CHROME_BIN, or CB_NO_BROWSER=1 to skip"
        );
        eprintln!("SKIP: no Chrome; set CHROME_BIN to run");
    }
    chrome
}

fn find_chrome() -> Option<PathBuf> {
    if let Ok(bin) = std::env::var("CHROME_BIN")
        && !bin.is_empty()
    {
        return Some(bin.into());
    }
    // a test Chrome on macOS reaches for the login keychain, so it runs there only on request
    if cfg!(target_os = "macos") {
        return None;
    }
    [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.exists())
}

/// A free port below Linux's ephemeral range, so no `bind(0)` or outgoing connection of a parallel
/// test takes it before Chrome binds it.
fn cdp_port() -> u16 {
    let start = 20_000 + (uuid::Uuid::new_v4().as_u128() % 12_000) as u16;
    (start..32_000)
        .chain(20_000..start)
        .find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
        .unwrap()
}

/// Serves at every path a page titled with the path and holding one text field.
async fn field_pages() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let page = format!(
                    "<!doctype html><title>{path}</title><input placeholder=\"field {path}\">"
                );
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    addr
}

/// Serves `/spa`, a page that shows "Loading your feed…" until a script puts the reply of `/feed` in its
/// place; `/feed` replies after 1 s.
async fn feed_pages() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let (kind, body) = if buf[..n].starts_with(b"GET /feed ") {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    ("text/plain", "feed item one")
                } else {
                    (
                        "text/html",
                        "<!doctype html><title>Feed</title><p>Loading your feed…</p><script>fetch('/feed').then((r) => r.text()).then((t) => { document.querySelector('p').textContent = t })</script>",
                    )
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {kind}; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    addr
}

/// Chrome's open tabs as (target id, url, websocket url).
async fn tabs(port: u16) -> Vec<(String, String, String)> {
    let list: Vec<Value> = reqwest::get(format!("http://127.0.0.1:{port}/json/list"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    list.into_iter()
        .filter(|t| t["type"] == "page")
        .map(|t| {
            let field = |k: &str| t[k].as_str().unwrap_or_default().to_owned();
            (field("id"), field("url"), field("webSocketDebuggerUrl"))
        })
        .collect()
}

/// Evaluates `expr` in a tab over its own CDP connection.
async fn tab_eval(ws_url: &str, expr: &str) -> Value {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
    let msg = json!({"id": 1, "method": "Runtime.evaluate", "params": {"expression": expr, "returnByValue": true}});
    ws.send(WsMessage::text(msg.to_string())).await.unwrap();
    while let Some(Ok(frame)) = ws.next().await {
        if let WsMessage::Text(text) = frame {
            let v: Value = serde_json::from_str(&text).unwrap();
            if v["id"] == 1 {
                return v["result"]["result"]["value"].clone();
            }
        }
    }
    panic!("no reply from {ws_url}");
}

async fn tab_gone(port: u16, url: &str) -> bool {
    for _ in 0..100 {
        if !tabs(port).await.iter().any(|t| t.1 == url) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn gw_bodies(log: &Log, path: &str) -> Vec<Value> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.ends_with(path))
        .map(|(_, b)| b.clone())
        .collect()
}

fn alive(pid_file: &Path) -> bool {
    let pid = std::fs::read_to_string(pid_file).unwrap();
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid.trim()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim_start().starts_with('Z')
}

async fn wait_gone(pid_file: &Path) -> bool {
    for _ in 0..100 {
        if !alive(pid_file) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn credential(key: &SigningKey, key_id: &str, box_id: &str, wake_id: &str) -> String {
    let exp = cocoonbox_agent::now_ms() / 1000 + 600;
    sign_credential(key, "v1", &format!("{key_id}|{box_id}|{wake_id}|{exp}"))
}

/// The box identity of a harness and the credentials the edge would sign for it.
struct Edge {
    key: SigningKey,
    key_id: String,
}

impl Edge {
    fn provision(h: &Harness) -> Self {
        let key = SigningKey::from_bytes(&[5; 32]);
        let key_id = write_identity(&h.identity, &key, "w1");
        Self { key, key_id }
    }

    fn owner(&self) -> String {
        credential(&self.key, &self.key_id, "cb-box", "w1")
    }

    fn share(&self, principal: &str) -> String {
        let exp = cocoonbox_agent::now_ms() / 1000 + 600;
        let fields = format!("{}|cb-box|w1|{exp}|{principal}", self.key_id);
        sign_credential(&self.key, "v2", &fields)
    }
}

fn signed<T>(msg: T, credential: &str) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut()
        .insert("x-cocoonbox-credential", credential.parse().unwrap());
    req
}

async fn surface_codes(h: &Harness, credential: Option<&str>) -> Vec<String> {
    fn with<T>(msg: T, credential: Option<&str>) -> Request<T> {
        let mut req = Request::new(msg);
        if let Some(c) = credential {
            req.metadata_mut()
                .insert("x-cocoonbox-credential", c.parse().unwrap());
        }
        req
    }
    fn code<T>(r: Result<T, tonic::Status>) -> String {
        r.map_or_else(|e| format!("{:?}", e.code()), |_| "Ok".into())
    }
    let agents = h
        .agents()
        .await
        .list(with(pb::ListAgentsRequest {}, credential))
        .await;
    let hello = h
        .session()
        .await
        .hello(with(pb::HelloRequest::default(), credential))
        .await;
    let plan = state_client(h)
        .await
        .plan(with(pb::PlanRequest::default(), credential))
        .await;
    let mut mcp = reqwest::Client::new()
        .post(format!("{}/mcp", h.endpoint))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
    if let Some(c) = credential {
        mcp = mcp.header("x-cocoonbox-credential", c);
    }
    let mcp = mcp.send().await.unwrap().status().as_u16();
    vec![code(agents), code(hello), code(plan), mcp.to_string()]
}

#[tokio::test]
async fn credential_guards_every_surface() {
    let h = Harness::start(Vec::new(), false).await;
    let admitted = ["Ok", "Ok", "InvalidArgument", "200"];
    let refused = [
        "Unauthenticated",
        "Unauthenticated",
        "Unauthenticated",
        "401",
    ];
    let unavailable = ["Unavailable", "Unavailable", "Unavailable", "503"];
    assert_eq!(surface_codes(&h, Some(owner())).await, admitted);
    assert_eq!(surface_codes(&h, None).await, refused);

    let key = SigningKey::from_bytes(&[5; 32]);
    let key_id = write_identity(&h.identity, &key, "w1");
    let good = credential(&key, &key_id, "cb-box", "w1");
    assert_eq!(surface_codes(&h, Some(&good)).await, admitted);
    let other_box = credential(&key, &key_id, "cb-other", "w1");
    assert_eq!(surface_codes(&h, Some(&other_box)).await, refused);
    let forged = credential(&SigningKey::from_bytes(&[6; 32]), &key_id, "cb-box", "w1");
    assert_eq!(surface_codes(&h, Some(&forged)).await, refused);

    write_identity(&h.identity, &key, "w2");
    let woken = credential(&key, &key_id, "cb-box", "w2");
    assert_eq!(surface_codes(&h, Some(&woken)).await, admitted);
    assert_eq!(surface_codes(&h, Some(&good)).await, refused);

    let attacker = SigningKey::from_bytes(&[8; 32]);
    let swapped = write_identity(&h.identity, &attacker, "w2");
    std::fs::set_permissions(&h.identity, std::fs::Permissions::from_mode(0o666)).unwrap();
    let self_signed = credential(&attacker, &swapped, "cb-box", "w2");
    assert_eq!(surface_codes(&h, Some(&self_signed)).await, unavailable);
}

#[tokio::test]
async fn credential_is_required() {
    let h = Harness::start(Vec::new(), false).await;
    let mut agents = h.agents().await;
    let err = agents
        .list(Request::new(pb::ListAgentsRequest {}))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
    let mut wrong = Request::new(pb::ListAgentsRequest {});
    wrong.metadata_mut().insert(
        "x-cocoonbox-credential",
        MetadataValue::from_static("v1.YQ.YQ"),
    );
    assert_eq!(
        agents.list(wrong).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    let mut session = h.session().await;
    let err = session
        .hello(Request::new(pb::HelloRequest::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::Unauthenticated);
    let hello = session
        .hello(authed(pb::HelloRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!((hello.screen_width, hello.screen_height), (1280, 800));
    assert_eq!(hello.default_main_model, "deepseek/deepseek-v4.1-flash");
    assert_eq!(hello.models.len(), 2);
    assert!(agents.list(authed(pb::ListAgentsRequest {})).await.is_ok());
}

#[tokio::test]
async fn shell_tool_loop_streams_into_transcript_and_events() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call(
                    "call_1",
                    "shell",
                    &json!({"command": "uname > ok.txt && cat ok.txt"}),
                ),
            ),
            ("chat/completions", chat_text("Wrote ok.txt.")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let mut session = h.session().await;
    let agent_id = create_agent(&mut agents).await;
    let mut events = session
        .events(authed(pb::EventsRequest { from_offset: 0 }))
        .await
        .unwrap()
        .into_inner();
    let turn = send(&mut agents, &agent_id, "run uname", "nonce-1").await;
    let again = send(&mut agents, &agent_id, "run uname", "nonce-1").await;
    assert_eq!(turn, again);
    let entries = wait_turn_end(&mut agents, &agent_id).await;

    let bodies: Vec<&Body> = entries.iter().filter_map(|e| e.body.as_ref()).collect();
    assert!(matches!(bodies[0], Body::Text(t) if t.text == "run uname"));
    assert!(matches!(bodies[1], Body::Text(t) if t.text == "Let me check."));
    let Body::ToolCall(call) = bodies[2] else {
        panic!("want tool call, got {:?}", bodies[2]);
    };
    assert_eq!(
        (call.call_id.as_str(), call.name.as_str()),
        ("call_1", "shell")
    );
    let Body::ToolResult(result) = bodies[3] else {
        panic!("want tool result, got {:?}", bodies[3]);
    };
    assert!(!result.is_error, "{}", result.text);
    assert!(result.text.ends_with("[exit status 0]"), "{}", result.text);
    assert!(matches!(bodies[4], Body::Text(t) if t.text == "Wrote ok.txt."));
    let Body::TurnEnd(end) = bodies[5] else {
        panic!("want turn end, got {:?}", bodies[5]);
    };
    assert!(end.ok, "{}", end.error);
    let usage = end.usage.as_ref().unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens
        ),
        (250, 15, 100)
    );
    assert_eq!(bodies.len(), 6);
    let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
    assert!(h.workspace.join("ok.txt").exists());

    let chats = gw_bodies(&h.gw_log, "chat/completions");
    assert_eq!(chats.len(), 2);
    let msgs = chats[1]["messages"].as_array().unwrap();
    assert!(msgs[0]["content"].as_str().unwrap().starts_with(
        "You are deepseek/deepseek-v4.1-flash, served through the cocoonbox gateway."
    ));
    let tool_msg = msgs.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(tool_msg["tool_call_id"], "call_1");
    assert_eq!(msgs.iter().filter(|m| m["role"] == "user").count(), 1);

    let mut offsets = Vec::new();
    let mut stored = Vec::new();
    let mut transcript_seqs = Vec::new();
    let mut deltas = Vec::new();
    while transcript_seqs.len() < entries.len() {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        offsets.push(ev.offset);
        match ev.body {
            Some(event::Body::TextDelta(d)) => {
                assert_eq!(
                    (d.agent_id.as_str(), d.turn_id.as_str()),
                    (agent_id.as_str(), turn.as_str())
                );
                deltas.push((d.block, d.text));
            }
            Some(event::Body::Transcript(t)) => {
                transcript_seqs.push(t.seq);
                stored.push(ev.offset);
            }
            _ => stored.push(ev.offset),
        }
    }
    assert_eq!(transcript_seqs, seqs);
    assert!(offsets.windows(2).all(|w| w[1] == w[0] + 1), "{offsets:?}");
    assert_eq!(
        deltas,
        [
            (1, "Let me check.".to_owned()),
            (2, "Wrote ok.txt.".to_owned())
        ]
    );
    let blocks: Vec<(u32, &str)> = bodies
        .iter()
        .filter_map(|b| match b {
            Body::Text(t) if !t.text.starts_with("run") => Some((t.block, t.text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(blocks, [(1, "Let me check."), (2, "Wrote ok.txt.")]);
    assert!(
        !chats[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "screenshot")
    );

    let mut replay = session
        .events(authed(pb::EventsRequest {
            from_offset: stored[1],
        }))
        .await
        .unwrap()
        .into_inner();
    let mut replayed = Vec::new();
    for _ in 2..stored.len() {
        replayed.push(replay.message().await.unwrap().unwrap().offset);
    }
    assert_eq!(replayed, stored[2..]);
    let stale = session
        .events(authed(pb::EventsRequest { from_offset: 3 }))
        .await
        .unwrap_err();
    assert_eq!(stale.code(), Code::OutOfRange);
}

#[tokio::test]
async fn delegate_over_chat_wire() {
    run_delegate(false).await;
}

#[tokio::test]
async fn delegate_over_messages_wire() {
    run_delegate(true).await;
}

async fn run_delegate(messages_wire: bool) {
    let state_args = json!({});
    let act_args = json!({"action": "type", "element": 0, "text": "hello"});
    let key_args = json!({"action": "key", "key": "return"});
    let sub = if messages_wire {
        vec![
            (
                "messages",
                messages_tool_use("toolu_1", "computer_state", &state_args),
            ),
            (
                "messages",
                messages_tool_use("toolu_2", "computer_act", &act_args),
            ),
            (
                "messages",
                messages_tool_use("toolu_3", "computer_act", &key_args),
            ),
            ("messages", messages_text("Typed hello into Mousepad.")),
        ]
    } else {
        vec![
            (
                "chat/completions",
                chat_tool_call("toolu_1", "computer_state", &state_args),
            ),
            (
                "chat/completions",
                chat_tool_call("toolu_2", "computer_act", &act_args),
            ),
            (
                "chat/completions",
                chat_tool_call("toolu_3", "computer_act", &key_args),
            ),
            ("chat/completions", chat_text("Typed hello into Mousepad.")),
        ]
    };
    let mut script = vec![(
        "chat/completions",
        chat_tool_call(
            "call_d",
            "delegate_computer",
            &json!({"task": "type hello into the editor"}),
        ),
    )];
    script.extend(sub);
    script.push(("chat/completions", chat_text("Done.")));
    let h = Harness::start(script, messages_wire).await;
    let mut agents = h.agents().await;
    let mut session = h.session().await;
    let agent_id = create_agent(&mut agents).await;
    let mut events = session
        .events(authed(pb::EventsRequest { from_offset: 0 }))
        .await
        .unwrap()
        .into_inner();
    send(&mut agents, &agent_id, "type hello", "").await;
    let entries = wait_turn_end(&mut agents, &agent_id).await;

    let Some(Body::TurnEnd(end)) = entries.last().and_then(|e| e.body.as_ref()) else {
        panic!("no turn end");
    };
    assert!(end.ok, "{}", end.error);
    let sub_calls: Vec<&pb::ToolCall> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Some(Body::ToolCall(c)) if c.subagent == "computer" => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(sub_calls.len(), 3);
    let sub_texts: Vec<&str> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Some(Body::Text(t)) if t.subagent == "computer" => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        sub_texts.contains(&"Typed hello into Mousepad."),
        "{sub_texts:?}"
    );
    let state = entries
        .iter()
        .find_map(|e| match &e.body {
            Some(Body::ToolResult(r)) if r.call_id == "toolu_1" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(
        state.text.contains("[0] text \"editor\" center=(60,40)"),
        "{}",
        state.text
    );
    assert!(
        state.text.find("Mousepad") < state.text.find("Xfdesktop"),
        "{}",
        state.text
    );
    assert!(!state.text.contains("hidden"), "{}", state.text);
    assert!(state.image.starts_with(b"\x89PNG"));
    let delegate = entries
        .iter()
        .find_map(|e| match &e.body {
            Some(Body::ToolResult(r)) if r.call_id == "call_d" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(
        delegate.text.starts_with("Typed hello into Mousepad."),
        "{}",
        delegate.text
    );
    assert!(delegate.image.starts_with(b"\x89PNG"));

    let typed = h
        .cua_log
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == "type_text")
        .map(|(_, a)| a.clone())
        .unwrap();
    assert_eq!(typed["element_token"], "tok-0");
    assert_eq!(typed["pid"], 42);
    assert_eq!(typed["text"], "hello");
    assert_eq!(typed["window_id"], 7);
    let pressed = h
        .cua_log
        .lock()
        .unwrap()
        .iter()
        .find(|(n, _)| n == "press_key")
        .map(|(_, a)| a.clone())
        .unwrap();
    assert_eq!(
        (pressed["pid"].clone(), pressed["window_id"].clone()),
        (json!(42), json!(7))
    );
    let cua_calls = h.cua_log.lock().unwrap().clone();
    let session = cua_calls[0].1["session"].as_str().unwrap().to_owned();
    assert!(session.starts_with("cu-"), "{session}");
    let names: Vec<&str> = cua_calls.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names[..3],
        ["list_windows", "get_window_state", "get_desktop_state"]
    );
    for input in [&typed, &pressed] {
        let once = input["session"].as_str().unwrap();
        assert!(once.starts_with("input-"), "{once}");
        let calls: Vec<&str> = cua_calls
            .iter()
            .filter(|(_, a)| a["session"] == once)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(calls.last(), Some(&"end_session"), "{cua_calls:?}");
    }
    assert!(
        cua_calls
            .iter()
            .all(|(_, a)| a["session"] == session.as_str()
                || a["session"].as_str().unwrap().starts_with("input-")),
        "{cua_calls:?}"
    );
    assert_eq!(
        cua_calls
            .last()
            .map(|(n, a)| (n.as_str(), a["session"].clone())),
        Some(("end_session", json!(session)))
    );

    let chats = gw_bodies(&h.gw_log, "chat/completions");
    let main_after = if messages_wire {
        let vision = gw_bodies(&h.gw_log, "/v1/messages");
        assert_eq!(vision.len(), 4);
        assert_eq!(vision[0]["model"], "anthropic/claude-sonnet-5");
        let last_user = vision[3]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last_user["content"][0]["type"], "tool_result");
        assert_eq!(last_user["content"][0]["content"][1]["type"], "image");
        assert_eq!(chats.len(), 2);
        &chats[1]
    } else {
        assert_eq!(chats.len(), 6);
        assert!(gw_bodies(&h.gw_log, "/v1/messages").is_empty());
        let models: Vec<&str> = chats.iter().map(|c| c["model"].as_str().unwrap()).collect();
        assert_eq!(models[1..5], ["anthropic/claude-sonnet-5"; 4]);
        let msgs = chats[4]["messages"].as_array().unwrap();
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles.last(), Some(&"user"), "{roles:?}");
        let shot = &msgs.last().unwrap()["content"][1]["image_url"]["url"];
        assert!(shot.as_str().unwrap().starts_with("data:image/png;base64,"));
        &chats[5]
    };
    let main_tool = main_after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()
        .clone();
    assert!(main_tool["content"].as_str().unwrap().contains("no vision"));
    assert!(!main_after.to_string().contains(PNG));

    let mut actions = Vec::new();
    while let Ok(Some(Ok(ev))) = tokio::time::timeout(Duration::from_millis(500), events.message())
        .await
        .map(Result::transpose)
    {
        if let Some(event::Body::ComputerAction(a)) = ev.body {
            assert_eq!(a.agent_id, agent_id);
            actions.push((a.action, a.text));
        }
    }
    assert_eq!(
        actions,
        [
            ("type".to_owned(), "hello".to_owned()),
            ("key".to_owned(), "return".to_owned())
        ]
    );
}

#[tokio::test]
async fn a_turn_at_its_step_cap_ends_with_a_progress_report() {
    for (plans, cap) in [(false, 40), (true, 150)] {
        let mut script: Vec<(&'static str, String)> = (0..cap)
            .map(|n| {
                (
                    "chat/completions",
                    chat_tool_call(&format!("c{n}"), "list_dir", &json!({})),
                )
            })
            .collect();
        script.push(("chat/completions", chat_text("Done: a. Left: b. Stuck: c.")));
        let h = if plans {
            Harness::with_desk(script).await
        } else {
            Harness::start(script, false).await
        };
        let mut agents = h.agents().await;
        let agent_id = create_agent(&mut agents).await;
        send(&mut agents, &agent_id, "list forever", "").await;
        let entries = wait_turn_end(&mut agents, &agent_id).await;
        let Some(Body::TurnEnd(end)) = entries.last().and_then(|e| e.body.as_ref()) else {
            panic!("no turn end");
        };
        assert!(!end.ok);
        assert_eq!(
            end.error,
            format!("stopped at the {cap}-step cap; the last reply reports the progress")
        );
        assert_eq!(end.usage.as_ref().unwrap().output_tokens, cap * 10 + 5);
        let last_text = entries.iter().rev().find_map(|e| match &e.body {
            Some(Body::Text(t)) if e.role == i32::from(pb::Role::Assistant) => {
                Some(t.text.as_str())
            }
            _ => None,
        });
        assert_eq!(last_text, Some("Done: a. Left: b. Stuck: c."));
        let chats = gw_bodies(&h.gw_log, "chat/completions");
        assert_eq!(chats.len(), cap as usize + 1);
        let (report, before) = (&chats[cap as usize], &chats[cap as usize - 1]);
        assert_eq!(report["tool_choice"], "none");
        assert!(before.get("tool_choice").is_none());
        assert_eq!(report["tools"], before["tools"]);
        assert_eq!(report["max_tokens"], 1024);
        let ask = report["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(ask["role"], "user");
        assert!(
            ask["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&format!("all {cap} steps, and tools are off")),
            "{ask}"
        );
    }
}

#[tokio::test]
async fn a_computer_subagent_at_its_step_cap_returns_a_progress_report() {
    let mut script = vec![(
        "chat/completions",
        chat_tool_call(
            "call_d",
            "delegate_computer",
            &json!({"task": "look forever"}),
        ),
    )];
    script.extend((0..30).map(|n| {
        (
            "chat/completions",
            chat_tool_call(&format!("s{n}"), "computer_state", &json!({})),
        )
    }));
    script.push((
        "chat/completions",
        chat_text("Opened the editor; the save dialog never came up."),
    ));
    script.push(("chat/completions", chat_text("Done.")));
    let h = Harness::start(script, false).await;
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;
    send(&mut agents, &agent_id, "look", "").await;
    let entries = wait_turn_end(&mut agents, &agent_id).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let delegate = entries
        .iter()
        .find_map(|e| match &e.body {
            Some(Body::ToolResult(r)) if r.call_id == "call_d" => Some(r),
            _ => None,
        })
        .unwrap();
    assert!(delegate.is_error);
    assert!(
        delegate.text.starts_with("The computer subagent stopped at its 30-step cap. Its progress report: Opened the editor; the save dialog never came up."),
        "{}",
        delegate.text
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    assert_eq!(chats[31]["tool_choice"], "none");
    assert_eq!(chats[31]["model"], "anthropic/claude-sonnet-5");
}

#[tokio::test]
async fn a_reply_without_a_call_or_text_gets_one_continuation() {
    let h = Harness::start(
        vec![
            ("chat/completions", chat_reasoning("plan: list it")),
            (
                "chat/completions",
                chat_tool_call("c1", "list_dir", &json!({})),
            ),
            ("chat/completions", chat_reasoning("now answer")),
            ("chat/completions", chat_text("Listed.")),
            ("chat/completions", chat_reasoning("one")),
            ("chat/completions", chat_reasoning("two")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let listed = create_agent(&mut agents).await;
    send(&mut agents, &listed, "list it", "").await;
    let entries = wait_turn_end(&mut agents, &listed).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let texts: Vec<&str> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Some(Body::Text(t)) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["list it", "Let me check.", "Listed."]);
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    for (empty, next) in [(0, 1), (2, 3)] {
        let before = chats[empty]["messages"].as_array().unwrap();
        let after = chats[next]["messages"].as_array().unwrap();
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(after[..before.len()], before[..]);
        assert_eq!(after[before.len()]["role"], "user");
        assert_eq!(
            after[before.len()]["content"][0]["text"],
            "Continue: call the next tool, or give your final answer."
        );
    }
    let silent = create_agent(&mut agents).await;
    send(&mut agents, &silent, "say nothing", "").await;
    let entries = wait_turn_end(&mut agents, &silent).await;
    let Some(Body::TurnEnd(end)) = entries.last().and_then(|e| e.body.as_ref()) else {
        panic!("no turn end");
    };
    assert!(!end.ok);
    assert_eq!(end.error, "the model returned no answer");
    assert_eq!(gw_bodies(&h.gw_log, "chat/completions").len(), 6);
}

#[tokio::test]
async fn a_reply_of_tool_markup_as_text_gets_the_continuation() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_text("I'll list it first.\n<｜DSML｜ invoke name=\"list_dir\">\n<｜DSML｜ parameter name=\"path\" string=\"true\">/ho"),
            ),
            (
                "chat/completions",
                chat_tool_call("c1", "list_dir", &json!({})),
            ),
            (
                "chat/completions",
                chat_text("Continuing.</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>"),
            ),
            ("chat/completions", chat_text("Listed.")),
            ("chat/completions", chat_text("I'd invoke list_dir again.")),
            ("chat/completions", chat_text("\n<｜DSML｜ invoke")),
            ("chat/completions", chat_text("\n<｜DSML｜ invoke name=\"shell\">\n")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let listed = create_agent(&mut agents).await;
    send(&mut agents, &listed, "list it", "").await;
    let entries = wait_turn_end(&mut agents, &listed).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    for (markup, next) in [(0, 1), (2, 3)] {
        let before = chats[markup]["messages"].as_array().unwrap();
        let after = chats[next]["messages"].as_array().unwrap();
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(after[..before.len()], before[..]);
        assert_eq!(
            after[before.len()]["content"][0]["text"],
            "Continue: call the next tool, or give your final answer."
        );
    }
    let prose = create_agent(&mut agents).await;
    send(&mut agents, &prose, "again", "").await;
    let entries = wait_turn_end(&mut agents, &prose).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    assert_eq!(gw_bodies(&h.gw_log, "chat/completions").len(), 5);
    let looping = create_agent(&mut agents).await;
    send(&mut agents, &looping, "run it", "").await;
    let entries = wait_turn_end(&mut agents, &looping).await;
    let Some(Body::TurnEnd(end)) = entries.last().and_then(|e| e.body.as_ref()) else {
        panic!("no turn end");
    };
    assert_eq!(end.error, "the model returned no answer");
    assert_eq!(gw_bodies(&h.gw_log, "chat/completions").len(), 7);
}

#[tokio::test]
async fn an_announcement_without_a_call_gets_the_continuation() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call("c1", "list_dir", &json!({})),
            ),
            ("chat/completions", chat_text("Continue with step 2.\n\n\n")),
            ("chat/completions", chat_text("Listed.")),
            ("chat/completions", chat_text("No response required.")),
            ("chat/completions", chat_text("</output>")),
            ("chat/completions", chat_text("Continue with step 3.")),
            (
                "chat/completions",
                chat_text("I'll continue with the next steps."),
            ),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let listed = create_agent(&mut agents).await;
    send(&mut agents, &listed, "list it", "").await;
    let entries = wait_turn_end(&mut agents, &listed).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    let before = chats[1]["messages"].as_array().unwrap();
    let after = chats[2]["messages"].as_array().unwrap();
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(after[..before.len()], before[..]);
    assert_eq!(
        after[before.len()]["content"][0]["text"],
        "Continue: call the next tool, or give your final answer."
    );
    let idle = create_agent(&mut agents).await;
    send(&mut agents, &idle, "run it", "").await;
    let entries = wait_turn_end(&mut agents, &idle).await;
    let Some(Body::TurnEnd(end)) = entries.last().and_then(|e| e.body.as_ref()) else {
        panic!("no turn end");
    };
    assert_eq!(end.error, "the model returned no answer");
    assert_eq!(gw_bodies(&h.gw_log, "chat/completions").len(), 5);
    let repeated = create_agent(&mut agents).await;
    send(&mut agents, &repeated, "go on", "").await;
    let entries = wait_turn_end(&mut agents, &repeated).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let last = entries.iter().rev().find_map(|e| match &e.body {
        Some(Body::Text(t)) => Some(t.text.as_str()),
        _ => None,
    });
    assert_eq!(last, Some("I'll continue with the next steps."));
    assert_eq!(gw_bodies(&h.gw_log, "chat/completions").len(), 7);
}

#[tokio::test]
async fn a_reply_that_saved_and_then_announces_a_check_gets_the_continuation() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call("c1", "list_dir", &json!({})),
            ),
            (
                "chat/completions",
                chat_text("I saved the file. Let me check the column widths"),
            ),
            (
                "chat/completions",
                chat_text("The widths are right. All done."),
            ),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let agent = create_agent(&mut agents).await;
    send(&mut agents, &agent, "save it, then check the widths", "").await;
    let entries = wait_turn_end(&mut agents, &agent).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    assert_eq!(chats.len(), 3);
    let last = chats[2]["messages"].as_array().unwrap();
    assert_eq!(
        last[last.len() - 1]["content"][0]["text"],
        "Continue: call the next tool, or give your final answer."
    );
}

#[tokio::test]
async fn take_over_waits_for_respond_and_cancel_ends_the_turn() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call(
                    "call_t",
                    "request_take_over",
                    &json!({"reason": "sign in to GitHub"}),
                ),
            ),
            ("chat/completions", chat_text("Signed in.")),
            (
                "chat/completions",
                chat_tool_call("call_s", "shell", &json!({"command": "sleep 30"})),
            ),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;
    send(&mut agents, &agent_id, "log in", "").await;
    let approval_id = loop {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.clone(),
                after_seq: 0,
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        if let Some(id) = page.entries.iter().find_map(|e| match &e.body {
            Some(Body::Approval(a)) if a.kind == "take_over" => Some(a.approval_id.clone()),
            _ => None,
        }) {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let listed = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        listed.agents[0].status,
        i32::from(pb::AgentStatus::WaitingForUser)
    );
    let missing = agents
        .respond(authed(pb::RespondRequest {
            agent_id: agent_id.clone(),
            approval_id: "nope".into(),
            approve: true,
            text: String::new(),
        }))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Code::NotFound);
    agents
        .respond(authed(pb::RespondRequest {
            agent_id: agent_id.clone(),
            approval_id,
            approve: true,
            text: "done".into(),
        }))
        .await
        .unwrap();
    let entries = wait_turn_end(&mut agents, &agent_id).await;
    assert!(entries.iter().any(
        |e| matches!(&e.body, Some(Body::ToolResult(r)) if r.text.contains("User note: done"))
    ));

    send(&mut agents, &agent_id, "sleep", "").await;
    loop {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.clone(),
                after_seq: entries.last().unwrap().seq,
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        if page
            .entries
            .iter()
            .any(|e| matches!(&e.body, Some(Body::ToolCall(c)) if c.call_id == "call_s"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    agents
        .cancel(authed(pb::CancelRequest {
            agent_id: agent_id.clone(),
        }))
        .await
        .unwrap();
    for _ in 0..200 {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.clone(),
                after_seq: entries.last().unwrap().seq,
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        if let Some(Body::TurnEnd(end)) = page.entries.last().and_then(|e| e.body.as_ref()) {
            assert!(!end.ok);
            assert_eq!(end.error, "cancelled");
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("cancel did not end the turn");
}

#[tokio::test]
async fn a_pending_take_over_leaves_quiesce_free_and_the_turn_resumes_after_it() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call(
                    "call_t",
                    "request_take_over",
                    &json!({"reason": "sign in to GitHub"}),
                ),
            ),
            (
                "chat/completions",
                chat_tool_call("call_s", "shell", &json!({"command": "touch after"})),
            ),
            ("chat/completions", chat_text("Signed in.")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;
    send(&mut agents, &agent_id, "log in", "").await;
    let approval_id = loop {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.clone(),
                after_seq: 0,
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        if let Some(id) = page.entries.iter().find_map(|e| match &e.body {
            Some(Body::Approval(a)) if a.kind == "take_over" => Some(a.approval_id.clone()),
            _ => None,
        }) {
            break id;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let started = std::time::Instant::now();
    plan(&h, pb::Consistency::Quiesced, "").await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the quiesced plan waited {:?} on a pending take-over",
        started.elapsed()
    );

    let saving = h.session_save.with_extension("started");
    write_script(
        &h.session_save,
        &format!("touch {}; sleep 2", saving.display()),
    );
    let quiesced = {
        let mut state = state_client(&h).await;
        tokio::spawn(async move {
            state
                .plan(authed(pb::PlanRequest {
                    key: Some(state_key()),
                    consistency: pb::Consistency::Quiesced.into(),
                    parent: String::new(),
                }))
                .await
        })
    };
    for _ in 0..200 {
        if saving.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saving.exists(), "the quiesce never started");
    agents
        .respond(authed(pb::RespondRequest {
            agent_id: agent_id.clone(),
            approval_id,
            approve: true,
            text: String::new(),
        }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !h.workspace.join("after").exists(),
        "a tool ran during the quiesce"
    );
    quiesced.await.unwrap().unwrap();
    let entries = wait_turn_end(&mut agents, &agent_id).await;
    assert!(h.workspace.join("after").exists());
    let Some(Body::TurnEnd(end)) = entries.last().and_then(|e| e.body.as_ref()) else {
        panic!("no turn end: {entries:?}");
    };
    assert!(end.ok, "{end:?}");
}

#[tokio::test]
async fn connector_tools_wait_for_the_users_approval_and_follow_list_changed() {
    let door_log = Log::default();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let door = mock_door(Arc::clone(&door_log), events.clone(), 0).await;
    let h = Harness::with_connect(
        vec![
            (
                "chat/completions",
                chat_tool_call("c1", "mock_list_items", &json!({})),
            ),
            (
                "chat/completions",
                chat_tool_call("c2", "mock_create_item", &json!({"title": "x"})),
            ),
            ("chat/completions", chat_text("Created.")),
            (
                "chat/completions",
                chat_tool_call("c3", "mock_create_item", &json!({"title": "y"})),
            ),
            ("chat/completions", chat_text("Declined.")),
            ("chat/completions", chat_text("Listed.")),
        ],
        door,
    )
    .await;
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;

    send(&mut agents, &agent_id, "make x", "").await;
    let asked = connector_approval(&mut agents, &agent_id, 0).await;
    assert_eq!(asked.summary, "Mock: create_item (title=\"x\")");
    let listed = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        listed.agents[0].status,
        i32::from(pb::AgentStatus::WaitingForUser)
    );
    agents
        .respond(authed(pb::RespondRequest {
            agent_id: agent_id.clone(),
            approval_id: asked.approval_id.clone(),
            approve: true,
            text: String::new(),
        }))
        .await
        .unwrap();
    let entries = turn_ends(&mut agents, &agent_id, 1).await;
    let result = |id: &str| {
        entries.iter().find_map(|e| match &e.body {
            Some(Body::ToolResult(r)) if r.call_id == id => Some(r.clone()),
            _ => None,
        })
    };
    assert_eq!(result("c1").unwrap().text, "items: a");
    let created = result("c2").unwrap();
    assert!(!created.is_error);
    assert_eq!(
        created.text,
        format!("created \"x\" with {}", asked.approval_id)
    );

    send(&mut agents, &agent_id, "make y", "").await;
    let asked = connector_approval(&mut agents, &agent_id, entries.last().unwrap().seq).await;
    agents
        .respond(authed(pb::RespondRequest {
            agent_id: agent_id.clone(),
            approval_id: asked.approval_id,
            approve: false,
            text: String::new(),
        }))
        .await
        .unwrap();
    let entries = turn_ends(&mut agents, &agent_id, 2).await;
    let declined = entries
        .iter()
        .find_map(|e| match &e.body {
            Some(Body::ToolResult(r)) if r.call_id == "c3" => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert!(declined.is_error);
    assert_eq!(declined.text, "The user declined the call.");
    let approved_calls = door_log
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, v)| {
            m == "tools/call" && v["params"]["_meta"]["cocoonbox/approval_id"].is_string()
        })
        .count();
    assert_eq!(approved_calls, 1, "a declined call ran again");

    for _ in 0..200 {
        if door_log.lock().unwrap().iter().any(|(m, _)| m == "GET") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    events
        .send(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#.to_owned())
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(&mut agents, &agent_id, "list", "").await;
    turn_ends(&mut agents, &agent_id, 3).await;
    let names = |body: &Value| -> Vec<String> {
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let bodies = gw_bodies(&h.gw_log, "/v1/chat/completions");
    let first = names(&bodies[0]);
    assert!(
        first.contains(&"mock_list_items".to_owned())
            && first.contains(&"mock_create_item".to_owned())
    );
    assert_eq!(
        first.iter().filter(|n| *n == "shell").count(),
        1,
        "{first:?}"
    );
    assert!(!first.contains(&"mock_new".to_owned()));
    assert!(names(bodies.last().unwrap()).contains(&"mock_new".to_owned()));
    assert!(
        door_log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m != "initialize")
            .all(|(_, v)| v["session"] == "s1")
    );
}

#[tokio::test]
async fn an_ended_door_stream_is_followed_again_within_a_second_each_time() {
    let door_log = Log::default();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let door = mock_door(Arc::clone(&door_log), events.clone(), 0).await;
    let _h = Harness::with_connect(vec![], door).await;
    for readers in 1..=4 {
        let since = tokio::time::Instant::now();
        while door_log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "GET")
            .count()
            < readers
        {
            assert!(
                since.elapsed() < Duration::from_millis(2500),
                "reader {readers} did not come within 2.5 s"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        events.send("close".to_owned()).unwrap();
    }
}

#[tokio::test]
async fn a_door_that_keeps_refusing_is_tried_again_at_most_every_five_seconds() {
    let door_log = Log::default();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let door = mock_door(Arc::clone(&door_log), events.clone(), usize::MAX).await;
    let _h = Harness::with_connect(vec![], door).await;
    let refusals = || -> Vec<u64> {
        door_log
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "GET refused")
            .map(|(_, v)| v["ms"].as_u64().unwrap())
            .collect()
    };
    let since = tokio::time::Instant::now();
    while refusals().len() < 6 {
        assert!(
            since.elapsed() < Duration::from_secs(25),
            "{:?}",
            refusals()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let gaps: Vec<u64> = refusals().windows(2).map(|w| w[1] - w[0]).collect();
    assert!(gaps.iter().all(|&g| g < 5500), "{gaps:?}");
    assert!(gaps[3..].iter().all(|&g| g >= 4500), "{gaps:?}");
}

#[tokio::test]
async fn a_connector_approval_answered_in_the_console_resumes_the_turn_from_the_door() {
    let door_log = Log::default();
    let (events, _) = tokio::sync::broadcast::channel(8);
    let door = mock_door(Arc::clone(&door_log), events.clone(), 1).await;
    let h = Harness::with_connect(
        vec![
            (
                "chat/completions",
                chat_tool_call("c1", "mock_create_item", &json!({"title": "x"})),
            ),
            ("chat/completions", chat_text("Created.")),
        ],
        door,
    )
    .await;
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;
    send(&mut agents, &agent_id, "make x", "").await;
    let asked = connector_approval(&mut agents, &agent_id, 0).await;
    let refused_at = tokio::time::Instant::now();
    while !door_log.lock().unwrap().iter().any(|(m, _)| m == "GET") {
        assert!(
            refused_at.elapsed() < Duration::from_secs(5),
            "no reader came back after the refused one"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    events
        .send(
            json!({"jsonrpc": "2.0", "method": "notifications/cocoonbox/approval_answered",
                   "params": {"approval_id": asked.approval_id, "decision": "once"}})
            .to_string(),
        )
        .unwrap();
    let entries = turn_ends(&mut agents, &agent_id, 1).await;
    let created = entries
        .iter()
        .find_map(|e| match &e.body {
            Some(Body::ToolResult(r)) if r.call_id == "c1" => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert!(!created.is_error, "{created:?}");
    assert_eq!(
        created.text,
        format!("created \"x\" with {}", asked.approval_id)
    );
    agents
        .respond(authed(pb::RespondRequest {
            agent_id: agent_id.clone(),
            approval_id: asked.approval_id,
            approve: true,
            text: String::new(),
        }))
        .await
        .unwrap();
}

async fn connector_approval(
    agents: &mut AgentsClient<Channel>,
    agent_id: &str,
    after_seq: u64,
) -> pb::ApprovalRequest {
    for _ in 0..500 {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.into(),
                after_seq,
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        if let Some(a) = page.entries.iter().find_map(|e| match &e.body {
            Some(Body::Approval(a)) if a.kind == "connector" => Some(a.clone()),
            _ => None,
        }) {
            return a;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("no connector approval");
}

async fn turn_ends(
    agents: &mut AgentsClient<Channel>,
    agent_id: &str,
    n: usize,
) -> Vec<pb::TranscriptEntry> {
    for _ in 0..500 {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.into(),
                after_seq: 0,
                limit: 1000,
            }))
            .await
            .unwrap()
            .into_inner();
        let ends = page
            .entries
            .iter()
            .filter(|e| matches!(e.body, Some(Body::TurnEnd(_))))
            .count();
        if ends >= n {
            return page.entries;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("fewer than {n} turns ended");
}

#[tokio::test]
async fn delete_cancels_a_running_turn_and_removes_the_agent() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call(
                    "call_s",
                    "shell",
                    &json!({"command": "sleep 30 & echo $! > sleep.pid; wait"}),
                ),
            ),
            ("chat/completions", chat_text("kept")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let mut session = h.session().await;
    let doomed = create_agent(&mut agents).await;
    let kept = create_agent(&mut agents).await;
    let mut events = session
        .events(authed(pb::EventsRequest { from_offset: 0 }))
        .await
        .unwrap()
        .into_inner();
    send(&mut agents, &doomed, "sleep", "n1").await;
    send(&mut agents, &doomed, "queued behind the sleep", "n2").await;
    let sleep_pid = h.workspace.join("sleep.pid");
    loop {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: doomed.clone(),
                after_seq: 0,
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        if page
            .entries
            .iter()
            .any(|e| matches!(&e.body, Some(Body::ToolCall(c)) if c.call_id == "call_s"))
            && std::fs::read_to_string(&sleep_pid).is_ok_and(|p| p.ends_with('\n'))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = std::time::Instant::now();
    agents
        .delete(authed(pb::DeleteAgentRequest {
            agent_id: doomed.clone(),
        }))
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        wait_gone(&sleep_pid).await,
        "the cancelled command survived the delete"
    );

    let listed: Vec<String> = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner()
        .agents
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert_eq!(listed, [kept.as_str()]);
    let page = agents
        .transcript(authed(pb::TranscriptRequest {
            agent_id: doomed.clone(),
            after_seq: 0,
            limit: 0,
        }))
        .await
        .unwrap()
        .into_inner();
    assert!(page.entries.is_empty(), "{:?}", page.entries);
    let again = agents
        .delete(authed(pb::DeleteAgentRequest {
            agent_id: doomed.clone(),
        }))
        .await
        .unwrap_err();
    assert_eq!(again.code(), Code::NotFound);
    let send_after = agents
        .send(authed(pb::SendRequest {
            agent_id: doomed.clone(),
            text: "hello?".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(send_after.code(), Code::NotFound);
    let recreated = agents
        .create(authed(pb::CreateAgentRequest {
            name: "fresh".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_ne!(recreated.id, doomed);

    let deleted = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Some(event::Body::AgentDeleted(d)) = ev.body {
            break d.agent_id;
        }
    };
    assert_eq!(deleted, doomed);

    send(&mut agents, &kept, "still here", "").await;
    let entries = wait_turn_end(&mut agents, &kept).await;
    assert!(
        entries
            .iter()
            .any(|e| matches!(&e.body, Some(Body::Text(t)) if t.text == "kept"))
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    assert_eq!(
        chats.len(),
        2,
        "the queued turn of the deleted agent never ran"
    );
}

#[tokio::test]
async fn delete_ends_the_agents_shell_and_its_background_jobs() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call(
                    "call_bg",
                    "shell",
                    &json!({"command": "echo $$ > shell.pid; sleep 300 & echo $! > job.pid"}),
                ),
            ),
            ("chat/completions", chat_text("started")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let doomed = create_agent(&mut agents).await;
    send(&mut agents, &doomed, "start a job", "").await;
    wait_turn_end(&mut agents, &doomed).await;
    let pids = [h.workspace.join("shell.pid"), h.workspace.join("job.pid")];
    for pid in &pids {
        assert!(
            alive(pid),
            "{} not running before the delete",
            pid.display()
        );
    }
    agents
        .delete(authed(pb::DeleteAgentRequest {
            agent_id: doomed.clone(),
        }))
        .await
        .unwrap();
    for pid in &pids {
        assert!(
            wait_gone(pid).await,
            "{} survived the delete",
            pid.display()
        );
    }
}

#[tokio::test]
async fn untitled_agents_take_their_first_line_and_a_rename_wins() {
    let h = Harness::start(
        vec![
            ("chat/completions", chat_text("好的")),
            ("chat/completions", chat_text("ok")),
            ("chat/completions", chat_text("ok")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let mut session = h.session().await;
    let mut events = session
        .events(authed(pb::EventsRequest { from_offset: 0 }))
        .await
        .unwrap()
        .into_inner();
    let untitled = agents
        .create(authed(pb::CreateAgentRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(untitled.name, "agent");
    send(
        &mut agents,
        &untitled.id,
        "\n  帮我整理一下桌面上的下载文件夹，并按照文件类型分类放好  \n谢谢",
        "",
    )
    .await;
    wait_turn_end(&mut agents, &untitled.id).await;
    let name = |list: &pb::ListAgentsResponse, id: &str| {
        list.agents
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.name.clone())
            .unwrap()
    };
    let list = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        name(&list, &untitled.id),
        "帮我整理一下桌面上的下载文件夹，并按照文件类型分"
    );
    let titled = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Some(event::Body::Agent(a)) = ev.body
            && a.id == untitled.id
            && a.name != "agent"
        {
            break a.name;
        }
    };
    assert_eq!(titled, "帮我整理一下桌面上的下载文件夹，并按照文件类型分");

    let renamed = agents
        .rename(authed(pb::RenameAgentRequest {
            agent_id: untitled.id.clone(),
            name: "  下载整理  ".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(renamed.name, "下载整理");
    let named_first = agents
        .create(authed(pb::CreateAgentRequest::default()))
        .await
        .unwrap()
        .into_inner();
    agents
        .rename(authed(pb::RenameAgentRequest {
            agent_id: named_first.id.clone(),
            name: "mine".into(),
        }))
        .await
        .unwrap();
    send(
        &mut agents,
        &named_first.id,
        "a message that would title it",
        "",
    )
    .await;
    wait_turn_end(&mut agents, &named_first.id).await;
    let list = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(name(&list, &untitled.id), "下载整理");
    assert_eq!(name(&list, &named_first.id), "mine");

    let blank = agents
        .rename(authed(pb::RenameAgentRequest {
            agent_id: untitled.id.clone(),
            name: " ".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(blank.code(), Code::InvalidArgument);
    let unknown = agents
        .rename(authed(pb::RenameAgentRequest {
            agent_id: "ag-nope".into(),
            name: "x".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), Code::NotFound);
}

#[tokio::test]
async fn screen_relays_raw_rfb_bytes() {
    let h = Harness::start(Vec::new(), false).await;
    let mut session = h.session().await;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(pb::ScreenClientFrame {
        body: Some(screen_client_frame::Body::Open(pb::ScreenOpen {})),
    })
    .await
    .unwrap();
    let mut frames = session
        .screen(authed(tokio_stream_from(rx)))
        .await
        .unwrap()
        .into_inner();
    let banner = frames.message().await.unwrap().unwrap();
    assert_eq!(&banner.data[..], b"RFB 003.008\n");
    tx.send(pb::ScreenClientFrame {
        body: Some(screen_client_frame::Body::Data(bytes::Bytes::from_static(
            b"RFB 003.008\n",
        ))),
    })
    .await
    .unwrap();
    let echo = frames.message().await.unwrap().unwrap();
    assert_eq!(&echo.data[..], b"RFB 003.008\n");
}

fn tokio_stream_from<T>(
    rx: tokio::sync::mpsc::Receiver<T>,
) -> tokio_stream::wrappers::ReceiverStream<T> {
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

fn exec_start(command: &str) -> pb::ExecClientFrame {
    pb::ExecClientFrame {
        body: Some(exec_client_frame::Body::Start(pb::ExecStart {
            command: command.into(),
            tty: None,
        })),
    }
}

fn exec_frame(body: exec_client_frame::Body) -> pb::ExecClientFrame {
    pb::ExecClientFrame { body: Some(body) }
}

fn exec_stdin(data: &[u8]) -> pb::ExecClientFrame {
    pb::ExecClientFrame {
        body: Some(exec_client_frame::Body::Stdin(
            bytes::Bytes::copy_from_slice(data),
        )),
    }
}

/// Reads an Exec stream to its end: stdout, stderr and the exit code of the last frame.
async fn exec_output(
    mut frames: tonic::Streaming<pb::ExecServerFrame>,
) -> (Vec<u8>, Vec<u8>, Option<i32>) {
    let (mut out, mut err, mut code) = (Vec::new(), Vec::new(), None);
    while let Some(frame) = frames.message().await.unwrap() {
        assert!(code.is_none(), "a frame after the exit code");
        match frame.body.unwrap() {
            exec_server_frame::Body::Stdout(d) => out.extend_from_slice(&d),
            exec_server_frame::Body::Stderr(d) => err.extend_from_slice(&d),
            exec_server_frame::Body::ExitCode(c) => code = Some(c),
        }
    }
    (out, err, code)
}

/// Runs `command` through Session.Exec, sends `stdin` in 64 KiB frames and half-closes.
async fn exec(h: &Harness, command: &str, stdin: Vec<u8>) -> (Vec<u8>, Vec<u8>, Option<i32>) {
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(exec_start(command)).await.unwrap();
    tokio::spawn(async move {
        for chunk in stdin.chunks(64 << 10) {
            if tx.send(exec_stdin(chunk)).await.is_err() {
                return;
            }
        }
    });
    let frames = h
        .session()
        .await
        .exec(authed(tokio_stream_from(rx)))
        .await
        .unwrap()
        .into_inner();
    exec_output(frames).await
}

#[tokio::test]
async fn exec_carries_binary_stdio_until_stdin_ends_and_returns_the_exit_code() {
    let h = Harness::start(Vec::new(), false).await;
    let data = noise(3 << 20, 7);
    let (out, err, code) = exec(&h, "pwd >&2; echo \"$TERM\" >&2; cat; exit 3", data.clone()).await;
    assert!(
        out == data,
        "stdout differs: {} of {} bytes",
        out.len(),
        data.len()
    );
    let cwd = h.workspace.canonicalize().unwrap();
    assert_eq!(
        String::from_utf8(err).unwrap(),
        format!("{}\ndumb\n", cwd.display())
    );
    assert_eq!(code, Some(3));

    assert_eq!(
        exec(&h, "false", Vec::new()).await,
        (Vec::new(), Vec::new(), Some(1))
    );
    let (_, _, code) = exec(&h, "kill -TERM $$", Vec::new()).await;
    assert_eq!(code, Some(128 + 15));
    let (out, _, code) = exec(&h, "wc -c", b"four".to_vec()).await;
    assert_eq!(
        (String::from_utf8(out).unwrap().trim(), code),
        ("4", Some(0))
    );
}

#[tokio::test]
async fn exec_runs_a_login_shell_and_forwards_signals_after_stdin_ends() {
    let h = Harness::start(Vec::new(), false).await;
    let (out, _, code) = exec(&h, "shopt -q login_shell && echo login", Vec::new()).await;
    assert_eq!((out, code), (b"login\n".to_vec(), Some(0)));

    for (signal, want_out, want_code) in [
        (pb::ExecSignal::Int, "ready\ngot-int\n", 9),
        (pb::ExecSignal::Term, "ready\n", 128 + 15),
    ] {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(exec_start(
            "trap 'echo got-int; exit 9' INT; echo ready; while :; do sleep 0.1; done",
        ))
        .await
        .unwrap();
        tx.send(exec_frame(exec_client_frame::Body::Eof(pb::ExecEof {})))
            .await
            .unwrap();
        let mut frames = h
            .session()
            .await
            .exec(authed(tokio_stream_from(rx)))
            .await
            .unwrap()
            .into_inner();
        let first = frames.message().await.unwrap().unwrap();
        assert_eq!(
            first.body,
            Some(exec_server_frame::Body::Stdout(bytes::Bytes::from_static(
                b"ready\n"
            )))
        );
        tx.send(exec_frame(exec_client_frame::Body::Signal(signal.into())))
            .await
            .unwrap();
        let (out, _, code) = exec_output(frames).await;
        assert_eq!(
            (format!("ready\n{}", String::from_utf8(out).unwrap()), code),
            (want_out.to_owned(), Some(want_code)),
            "{signal:?}"
        );
    }
}

#[tokio::test]
async fn exec_with_tty_runs_on_a_terminal_of_the_given_size() {
    let h = Harness::start(Vec::new(), false).await;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(pb::ExecClientFrame {
        body: Some(exec_client_frame::Body::Start(pb::ExecStart {
            command: "tty; echo \"term=$TERM\"; stty size; read x; stty size; exit 4".into(),
            tty: Some(pb::ExecTty {
                term: "vt220".into(),
                window: Some(pb::ExecWindow {
                    rows: 30,
                    cols: 100,
                }),
                terminfo: String::new(),
            }),
        })),
    })
    .await
    .unwrap();
    let mut frames = h
        .session()
        .await
        .exec(authed(tokio_stream_from(rx)))
        .await
        .unwrap()
        .into_inner();
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("30 100\r\n") {
        match frames.message().await.unwrap().unwrap().body.unwrap() {
            exec_server_frame::Body::Stdout(d) => seen.extend_from_slice(&d),
            other => panic!("unexpected frame before the size: {other:?}"),
        }
    }
    tx.send(exec_frame(exec_client_frame::Body::Resize(
        pb::ExecWindow {
            rows: 40,
            cols: 120,
        },
    )))
    .await
    .unwrap();
    tx.send(exec_stdin(b"go\n")).await.unwrap();
    let (out, err, code) = exec_output(frames).await;
    seen.extend_from_slice(&out);
    let text = String::from_utf8_lossy(&seen);
    assert!(
        text.starts_with("/dev/") && !text.contains("not a tty"),
        "{text}"
    );
    assert!(text.contains("term=vt220\r\n"), "{text}");
    assert!(text.contains("go\r\n40 120\r\n"), "{text}");
    assert!(err.is_empty());
    assert_eq!(code, Some(4));
}

/// Runs `command` on a terminal of type `term` with `terminfo` sent along and stdin ended.
async fn exec_tty(h: &Harness, command: &str, term: &str, terminfo: &str) -> (String, String) {
    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tx.send(pb::ExecClientFrame {
        body: Some(exec_client_frame::Body::Start(pb::ExecStart {
            command: command.into(),
            tty: Some(pb::ExecTty {
                term: term.into(),
                window: Some(pb::ExecWindow { rows: 24, cols: 80 }),
                terminfo: terminfo.into(),
            }),
        })),
    })
    .await
    .unwrap();
    drop(tx);
    let frames = h
        .session()
        .await
        .exec(authed(tokio_stream_from(rx)))
        .await
        .unwrap()
        .into_inner();
    let (out, err, code) = exec_output(frames).await;
    assert_eq!(code, Some(0));
    (
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

const TEST_TERMINFO: &str =
    "cb-test-term|cocoonbox test terminal,\n\tam, cols#80, lines#24, clear=\\E[H\\E[2J, Cbx,\n";

#[tokio::test]
async fn exec_tty_installs_the_terminfo_sent_for_a_term_the_box_lacks() {
    let h = Harness::start(Vec::new(), false).await;
    let show = "echo \"term=$TERM\"; infocmp -x | grep -c Cbx";
    let (out, err) = exec_tty(&h, show, "cb-test-term", TEST_TERMINFO).await;
    assert_eq!(out, "term=cb-test-term\r\n1\r\n");
    assert_eq!(err, "");
    let dir = h.home.join(".terminfo");
    assert!(
        ["c", "63"]
            .iter()
            .any(|d| dir.join(d).join("cb-test-term").is_file())
    );

    let (out, err) = exec_tty(&h, show, "cb-test-term", "").await;
    assert_eq!(
        (out.as_str(), err.as_str()),
        ("term=cb-test-term\r\n1\r\n", "")
    );
}

#[tokio::test]
async fn exec_tty_falls_back_to_xterm_256color_without_a_usable_terminfo() {
    let h = Harness::start(Vec::new(), false).await;
    for (term, terminfo, why) in [
        ("cb-missing-term", "", "the client sent no terminfo"),
        (
            "cb-missing-term",
            TEST_TERMINFO,
            "the terminfo sent does not define it",
        ),
        (
            "cb-broken-term",
            "cb-broken-term|broken terminal,\n\tuse=cb-no-such-term,\n",
            "tic: ",
        ),
    ] {
        let (out, err) = exec_tty(&h, "echo \"term=$TERM\"", term, terminfo).await;
        assert_eq!(out, "term=xterm-256color\r\n", "{term} {why}");
        assert!(
            err.starts_with(&format!(
                "cocoonbox: no terminfo for {term} in the box ({why}"
            )) && err.ends_with("; using TERM=xterm-256color\r\n"),
            "{err}"
        );
    }
}

#[tokio::test]
async fn exec_refuses_an_oversized_terminfo() {
    let h = Harness::start(Vec::new(), false).await;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(pb::ExecClientFrame {
        body: Some(exec_client_frame::Body::Start(pb::ExecStart {
            command: "true".into(),
            tty: Some(pb::ExecTty {
                term: "cb-huge-term".into(),
                window: None,
                terminfo: "#".repeat((256 << 10) + 1),
            }),
        })),
    })
    .await
    .unwrap();
    let err = h
        .session()
        .await
        .exec(authed(tokio_stream_from(rx)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(!h.home.join(".terminfo").exists());
}

#[tokio::test]
async fn exec_is_for_the_owner_only() {
    let h = Harness::start(Vec::new(), false).await;
    let edge = Edge::provision(&h);
    for principal in ["share:sh-1:write", "share:sh-1:read:u-1"] {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(exec_start("touch shared")).await.unwrap();
        let err = h
            .session()
            .await
            .exec(signed(tokio_stream_from(rx), &edge.share(principal)))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Code::PermissionDenied, "{principal}");
    }
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(exec_start("touch owned")).await.unwrap();
    drop(tx);
    let frames = h
        .session()
        .await
        .exec(signed(tokio_stream_from(rx), &edge.owner()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(exec_output(frames).await.2, Some(0));
    assert!(h.workspace.join("owned").exists());
    assert!(!h.workspace.join("shared").exists());
}

#[tokio::test]
async fn cancelling_exec_kills_the_command() {
    let h = Harness::start(Vec::new(), false).await;
    for (name, fill) in [("idle", false), ("stdin-blocked", true)] {
        let pid = h.workspace.join(format!("{name}.pid"));
        let (addr, link) = severable(&h.endpoint).await;
        let channel = Channel::from_shared(addr).unwrap().connect().await.unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(exec_start(&format!("echo $$ > {name}.pid; exec sleep 300")))
            .await
            .unwrap();
        let _frames = SessionClient::new(channel)
            .exec(authed(tokio_stream_from(rx)))
            .await
            .unwrap();
        for _ in 0..100 {
            if std::fs::read_to_string(&pid).is_ok_and(|p| !p.trim().is_empty()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(alive(&pid), "{name}: not running");
        if fill {
            let chunk = noise(64 << 10, 3);
            let mut sent = 0;
            while tokio::time::timeout(Duration::from_millis(300), tx.send(exec_stdin(&chunk)))
                .await
                .is_ok()
            {
                sent += chunk.len();
            }
            assert!(
                sent > 64 << 10,
                "{name}: only {sent} bytes went out before the stream stalled"
            );
        }
        link.abort();
        assert!(
            wait_gone(&pid).await,
            "{name}: the command survived the cancel"
        );
    }
}

#[tokio::test]
async fn an_exec_with_a_full_stdin_leaves_the_connection_to_other_calls() {
    let h = Harness::start(Vec::new(), false).await;
    let channel = h.channel().await;
    let mut session = SessionClient::new(channel);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(exec_start("exec sleep 300")).await.unwrap();
    let _frames = session.exec(authed(tokio_stream_from(rx))).await.unwrap();
    let chunk = noise(64 << 10, 5);
    while tokio::time::timeout(Duration::from_millis(300), tx.send(exec_stdin(&chunk)))
        .await
        .is_ok()
    {}
    let hello = tokio::time::timeout(
        Duration::from_secs(3),
        session.hello(authed(pb::HelloRequest::default())),
    )
    .await;
    assert!(
        hello.is_ok_and(|r| r.is_ok()),
        "Hello on the same connection stalled behind the exec"
    );
    let (tx2, rx2) = tokio::sync::mpsc::channel(4);
    tx2.send(exec_start("cat")).await.unwrap();
    tx2.send(exec_stdin(&chunk)).await.unwrap();
    drop(tx2);
    let frames = tokio::time::timeout(
        Duration::from_secs(3),
        session.exec(authed(tokio_stream_from(rx2))),
    )
    .await
    .expect("a second exec stalled")
    .unwrap()
    .into_inner();
    let (out, _, code) = tokio::time::timeout(Duration::from_secs(3), exec_output(frames))
        .await
        .expect("a second exec's stdin stalled");
    assert_eq!((out.len(), code), (chunk.len(), Some(0)));
}

/// A TCP forwarder to `endpoint`; aborting it closes every connection, as a client that dies does.
async fn severable(endpoint: &str) -> (String, tokio::task::JoinHandle<()>) {
    let target = endpoint.trim_start_matches("http://").to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut conns = tokio::task::JoinSet::new();
        loop {
            let (mut down, _) = listener.accept().await.unwrap();
            let target = target.clone();
            conns.spawn(async move {
                let mut up = tokio::net::TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
            });
        }
    });
    (addr, task)
}

#[tokio::test]
async fn exec_needs_start_first() {
    let h = Harness::start(Vec::new(), false).await;
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(exec_stdin(b"x")).await.unwrap();
    let err = h
        .session()
        .await
        .exec(authed(tokio_stream_from(rx)))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn cancel_during_computer_task_ends_the_driver_session() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call("call_d", "delegate_computer", &json!({"task": "wait"})),
            ),
            (
                "chat/completions",
                chat_tool_call(
                    "toolu_w",
                    "computer_act",
                    &json!({"action": "wait", "seconds": 10}),
                ),
            ),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;
    send(&mut agents, &agent_id, "wait", "").await;
    for _ in 0..200 {
        let page = agents
            .transcript(authed(pb::TranscriptRequest {
                agent_id: agent_id.clone(),
                after_seq: 0,
                limit: 0,
            }))
            .await
            .unwrap()
            .into_inner();
        if page
            .entries
            .iter()
            .any(|e| matches!(&e.body, Some(Body::ToolCall(c)) if c.call_id == "toolu_w"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    agents
        .cancel(authed(pb::CancelRequest {
            agent_id: agent_id.clone(),
        }))
        .await
        .unwrap();
    let entries = wait_turn_end(&mut agents, &agent_id).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.error == "cancelled")
    );
    for _ in 0..200 {
        if h.cua_log.lock().unwrap().iter().any(|(n, a)| {
            n == "end_session" && a["session"].as_str().is_some_and(|s| s.starts_with("cu-"))
        }) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "cancel did not end the driver session: {:?}",
        h.cua_log.lock().unwrap()
    );
}

#[tokio::test]
async fn vision_main_model_gets_a_screenshot_tool() {
    let h = Harness::start(
        vec![
            (
                "chat/completions",
                chat_tool_call("call_s", "screenshot", &json!({})),
            ),
            ("chat/completions", chat_text("I see the desktop.")),
        ],
        false,
    )
    .await;
    let mut agents = h.agents().await;
    let agent_id = agents
        .create(authed(pb::CreateAgentRequest {
            name: "v".into(),
            main_model: "anthropic/claude-sonnet-5".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .id;
    send(&mut agents, &agent_id, "what is on screen", "").await;
    let entries = wait_turn_end(&mut agents, &agent_id).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    assert!(
        chats[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "screenshot")
    );
    let last = chats[1]["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["role"], "user");
    assert!(
        last["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    assert!(
        !chats[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["function"]["name"] == "script_run")
    );
    let system = chats[0]["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(!system.contains("script_run"), "{system}");
    assert!(gw_bodies(&h.gw_log, "decisions").is_empty());
    let cua = h.cua_log.lock().unwrap().clone();
    let names: Vec<&str> = cua.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["get_desktop_state", "end_session"]);
    assert!(
        cua.iter()
            .all(|(_, a)| a["session"].as_str().unwrap().starts_with("shot-"))
    );
}

struct McpClient {
    http: reqwest::Client,
    url: String,
    session: String,
    next_id: u64,
    /// The edge credential each request carries; `None` sends the owner's.
    credential: Option<String>,
}

impl McpClient {
    async fn connect(h: &Harness) -> Self {
        Self::connect_as(h, None).await
    }

    async fn connect_as(h: &Harness, credential: Option<String>) -> Self {
        let mut c = Self {
            http: reqwest::Client::new(),
            url: format!("{}/mcp", h.endpoint),
            session: String::new(),
            next_id: 0,
            credential,
        };
        let resp = c
            .post(
                json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "test-client", "version": "1.0"}
                }}),
            )
            .await;
        assert_eq!(resp.status(), 200);
        c.session = resp.headers()["mcp-session-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let init: Value = resp.json().await.unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "cocoonbox-agent");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        let note = c
            .post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert_eq!(note.status(), 202);
        c
    }

    async fn post(&self, body: Value) -> reqwest::Response {
        let req = self
            .http
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        self.authorize(req).send().await.unwrap()
    }

    fn authorize(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req = match &self.credential {
            Some(c) => req.header("x-cocoonbox-credential", c),
            None => req.header("x-cocoonbox-credential", owner()),
        };
        if !self.session.is_empty() {
            req = req.header("mcp-session-id", &self.session);
        }
        req
    }

    fn tool_names(listed: &Value) -> Vec<String> {
        let mut names: Vec<String> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        names.sort_unstable();
        names
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let resp = self
            .post(json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}))
            .await;
        assert_eq!(resp.status(), 200);
        let reply: Value = resp.json().await.unwrap();
        assert_eq!(reply["id"], self.next_id);
        reply
    }

    async fn call(&mut self, name: &str, arguments: Value) -> Value {
        let reply = self
            .rpc("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        reply["result"].clone()
    }
}

fn driver_calls(h: &Harness, name: &str) -> Vec<Value> {
    h.cua_log
        .lock()
        .unwrap()
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, a)| a.clone())
        .collect()
}

#[tokio::test]
async fn mcp_owner_sessions_drive_every_tool_untracked() {
    let h = Harness::start(Vec::new(), false).await;
    let http = reqwest::Client::new();
    let denied = http
        .post(format!("{}/mcp", h.endpoint))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);

    let mut session = h.session().await;
    let mut events = session
        .events(authed(pb::EventsRequest { from_offset: 0 }))
        .await
        .unwrap()
        .into_inner();
    let mut mcp = McpClient::connect(&h).await;

    let listed = mcp.rpc("tools/list", json!({})).await;
    assert_eq!(
        McpClient::tool_names(&listed),
        [
            "browser_click",
            "browser_navigate",
            "browser_read",
            "browser_snapshot",
            "browser_type",
            "computer_act",
            "computer_state",
            "edit_file",
            "list_dir",
            "read_file",
            "screenshot",
            "shell",
            "write_file"
        ]
    );
    assert!(listed["result"]["tools"][0]["inputSchema"]["type"] == "object");

    let shell = mcp
        .call(
            "shell",
            json!({"command": "echo from-mcp > m.txt; cat m.txt; sleep 300 & echo $! > owner-job.pid"}),
        )
        .await;
    assert_eq!(shell["isError"], false);
    assert!(
        shell["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("from-mcp"),
        "{shell}"
    );
    assert!(h.workspace.join("m.txt").exists());

    let shot = mcp.call("screenshot", json!({"max_edge": 640})).await;
    assert_eq!(shot["content"][1]["type"], "image");
    assert_eq!(shot["content"][1]["mimeType"], "image/png");
    assert_eq!(shot["content"][1]["data"], PNG);
    let captures = driver_calls(&h, "get_desktop_state");
    assert_eq!(captures[0]["max_image_dimension"], 640);
    let label = captures[0]["session"].as_str().unwrap().to_owned();
    assert!(label.starts_with("cu-"), "{label}");

    let act = mcp
        .call("computer_act", json!({"action": "click", "x": 10, "y": 20}))
        .await;
    assert_eq!(act["isError"], false, "{act}");
    assert_eq!(driver_calls(&h, "click")[0]["session"], label.as_str());

    let unknown = mcp
        .rpc(
            "tools/call",
            json!({"name": "delegate_computer", "arguments": {}}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], -32602);

    let mut agents = h.agents().await;
    let listed = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.agents.is_empty(), "{:?}", listed.agents);

    let mut saw_action = false;
    while let Ok(Some(Ok(ev))) = tokio::time::timeout(Duration::from_millis(300), events.message())
        .await
        .map(Result::transpose)
    {
        match ev.body {
            Some(event::Body::ComputerAction(a)) => {
                assert_eq!((a.action.as_str(), a.x, a.y), ("click", 10, 20));
                saw_action = true;
            }
            Some(event::Body::Transcript(e)) => panic!("an owner session recorded {e:?}"),
            Some(event::Body::Agent(a)) => panic!("an owner session made agent {a:?}"),
            _ => {}
        }
    }
    assert!(saw_action);

    let discover = reqwest::Client::new()
        .post(format!("{}/mcp", h.endpoint))
        .header("x-cocoonbox-credential", owner())
        .json(&json!({"jsonrpc": "2.0", "id": 8, "method": "server/discover", "params": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(discover.status(), 200);
    let discover: Value = discover.json().await.unwrap();
    assert_eq!(discover["error"]["code"], -32601);
    assert_eq!(discover["id"], 8);
    let unknown_in_session = mcp.rpc("resources/list", json!({})).await;
    assert_eq!(unknown_in_session["error"]["code"], -32601);

    let no_session = reqwest::Client::new()
        .post(format!("{}/mcp", h.endpoint))
        .header("x-cocoonbox-credential", owner())
        .json(&json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_session.status(), 400);

    let job = h.workspace.join("owner-job.pid");
    assert!(alive(&job));
    let deleted = http
        .delete(format!("{}/mcp", h.endpoint))
        .header("x-cocoonbox-credential", owner())
        .header("mcp-session-id", &mcp.session)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    let ended = driver_calls(&h, "end_session");
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0]["session"], label.as_str());
    assert!(
        wait_gone(&job).await,
        "the closed session's shell job survived"
    );
    let gone = mcp
        .post(json!({"jsonrpc": "2.0", "id": 10, "method": "tools/list"}))
        .await;
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn mcp_surface_of_a_plain_box_matches_the_cbdesk_agent_byte_for_byte() {
    let h = Harness::start(Vec::new(), false).await;
    let http = reqwest::Client::new();
    let url = format!("{}/mcp", h.endpoint);
    let post = |body: Value, session: Option<String>| {
        let mut req = http
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .header("x-cocoonbox-credential", owner())
            .json(&body);
        if let Some(s) = session {
            req = req.header("mcp-session-id", s);
        }
        req.send()
    };
    let init = post(
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "golden", "version": "1"}}}),
        None,
    )
    .await
    .unwrap();
    let session = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let mut got = vec![("mcp-initialize.json", init.bytes().await.unwrap())];
    for (file, method) in [
        ("mcp-tools-list.json", "tools/list"),
        ("mcp-resources-list.json", "resources/list"),
    ] {
        let reply = post(
            json!({"jsonrpc": "2.0", "id": 1, "method": method}),
            Some(session.clone()),
        )
        .await
        .unwrap();
        got.push((file, reply.bytes().await.unwrap()));
    }
    for (file, body) in got {
        let want = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden")
                .join(file),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            String::from_utf8_lossy(&want),
            "{file} differs from the golden surface"
        );
    }
}

#[tokio::test]
async fn mcp_script_run_reports_result_logs_and_a_screenshot() {
    reports_result_logs_and_a_screenshot(&Harness::with_desk(Vec::new()).await, "script_run").await;
}

/// Runs scripts through `tool`.
async fn reports_result_logs_and_a_screenshot(h: &Harness, tool: &str) {
    let mut mcp = McpClient::connect(h).await;

    let ok = mcp
        .call(
            tool,
            json!({"code": "export function main(p) { return p }", "params": {"text": "hi"}}),
        )
        .await;
    assert_eq!(ok["isError"], false, "{ok}");
    let report: Value = serde_json::from_str(ok["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(report["result"], json!({"params": {"text": "hi"}}));
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["snapshot"], "outline");
    assert!(
        report["logs"].as_str().unwrap().starts_with("ran "),
        "{report}"
    );
    assert!(report.get("error").is_none(), "{report}");
    let shot = PathBuf::from(report["screenshot_path"].as_str().unwrap());
    assert!(shot.exists(), "{}", shot.display());
    assert_eq!(ok["content"][1]["type"], "image");

    std::fs::write(h.workspace.join("flow.mjs"), "boom").unwrap();
    let failed = mcp
        .call(tool, json!({"script": "flow.mjs", "screenshot": false}))
        .await;
    assert_eq!(failed["isError"], true, "{failed}");
    let report: Value =
        serde_json::from_str(failed["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        report["error"],
        json!({"kind": "exception", "message": "Error: boom"})
    );
    assert_eq!(report["exit_code"], 1);
    assert!(report.get("screenshot_path").is_none(), "{report}");
    assert_eq!(failed["content"].as_array().unwrap().len(), 1);

    let refused = mcp
        .call(tool, json!({"code": "1", "script": "flow.mjs"}))
        .await;
    assert_eq!(refused["isError"], true, "{refused}");
    let missing = mcp.call(tool, json!({"script": "nope.mjs"})).await;
    assert!(
        missing["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("does not exist"),
        "{missing}"
    );
}

#[tokio::test]
async fn mcp_script_run_stops_a_run_past_its_timeout_with_its_children() {
    stops_a_run_past_its_timeout(&Harness::with_desk(Vec::new()).await, "script_run").await;
}

#[tokio::test]
async fn mcp_plugin_runs_its_scripts_with_its_own_program() {
    let h = Harness::with_plugin(Vec::new()).await;
    reports_result_logs_and_a_screenshot(&h, "demo_run").await;
    let ran = std::fs::read_to_string(h.workspace.join("plugin.log")).unwrap();
    assert_eq!(ran.lines().count(), 2, "{ran}");
    assert!(ran.lines().all(|l| l.ends_with("entry.mjs")), "{ran}");
}

#[tokio::test]
async fn mcp_plugin_stops_a_run_past_its_timeout_with_its_children() {
    stops_a_run_past_its_timeout(&Harness::with_plugin(Vec::new()).await, "demo_run").await;
}

#[tokio::test]
async fn mcp_lists_a_plugin_after_the_script_tool_with_its_resource() {
    let h = Harness::with_plugin(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;

    let listed = mcp.rpc("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    let at = |name: &str| tools.iter().position(|t| t["name"] == name).unwrap();
    assert_eq!(at("demo_run"), at("script_run") + 1);
    let (plugin, script) = (&tools[at("demo_run")], &tools[at("script_run")]);
    assert_eq!(plugin["description"], "Run a demo script.");
    assert_eq!(plugin["inputSchema"], script["inputSchema"]);
    assert_eq!(plugin["_meta"]["anthropic/alwaysLoad"], true);

    let listed = mcp.rpc("resources/list", json!({})).await;
    let uris: Vec<&str> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert_eq!(
        uris,
        ["cocoonbox://desk/AGENTS.md", "cocoonbox://demo/DEMO.md"]
    );
    let read = mcp
        .rpc("resources/read", json!({"uri": "cocoonbox://demo/DEMO.md"}))
        .await;
    assert_eq!(
        read["result"]["contents"][0]["text"], "demo guide",
        "{read}"
    );
}

#[tokio::test]
async fn a_box_without_the_desk_ignores_its_plugins() {
    let day = Duration::from_secs(24 * 3600);
    for automation in [Automation::Plain, Automation::Android] {
        let h = Harness::boot(
            Vec::new(),
            false,
            Duration::from_secs(600),
            day,
            None,
            None,
            automation,
            true,
        )
        .await;
        let mut mcp = McpClient::connect(&h).await;

        let listed = mcp.rpc("tools/list", json!({})).await;
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"demo_run"), "{automation:?}: {names:?}");
        let resources = mcp.rpc("resources/list", json!({})).await;
        assert!(
            !resources.to_string().contains("cocoonbox://demo/"),
            "{automation:?}: {resources}"
        );
        let out = mcp
            .rpc(
                "tools/call",
                json!({"name": "demo_run", "arguments": {"code": "1"}}),
            )
            .await;
        assert_eq!(
            out["error"]["message"], "unknown tool: demo_run",
            "{automation:?}: {out}"
        );
        assert!(!h.workspace.join("plugin.log").exists(), "{automation:?}");
    }
}

#[tokio::test]
async fn an_android_box_keeps_its_mcp_texts_within_the_caps() {
    let h = Harness::with_android().await;
    let init: Value = reqwest::Client::new()
        .post(format!("{}/mcp", h.endpoint))
        .header("accept", "application/json, text/event-stream")
        .header("x-cocoonbox-credential", owner())
        .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.chars().count() <= 1600,
        "{} chars: the bridge prefix must keep the total under Claude Code's 2048 cap",
        instructions.chars().count()
    );
    let mut mcp = McpClient::connect(&h).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    for tool in listed["result"]["tools"].as_array().unwrap() {
        assert!(
            tool["description"].as_str().unwrap().chars().count() <= 2048,
            "{}: Claude Code cuts a description at 2048 characters",
            tool["name"]
        );
    }
}

#[tokio::test]
async fn hello_names_the_box_automation_and_its_screen() {
    for (h, automation, screen) in [
        (
            Harness::start(Vec::new(), false).await,
            pb::Automation::Plain,
            (1280, 800),
        ),
        (
            Harness::with_desk(Vec::new()).await,
            pb::Automation::Desk,
            (1280, 800),
        ),
        (
            Harness::with_android().await,
            pb::Automation::Android,
            (720, 1280),
        ),
    ] {
        let hello = h
            .session()
            .await
            .hello(authed(pb::HelloRequest::default()))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(hello.automation(), automation);
        assert_eq!(
            (hello.screen_width, hello.screen_height),
            screen,
            "{automation:?}"
        );
    }
}

#[tokio::test]
async fn mcp_on_an_android_box_drives_the_phone_through_adb() {
    let h = Harness::with_android().await;
    let init: Value = reqwest::Client::new()
        .post(format!("{}/mcp", h.endpoint))
        .header("accept", "application/json, text/event-stream")
        .header("x-cocoonbox-credential", owner())
        .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains("Android 16 phone"), "{instructions}");
    assert!(!instructions.contains("XFCE"), "{instructions}");

    let mut mcp = McpClient::connect(&h).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    assert_eq!(
        McpClient::tool_names(&listed),
        [
            "computer_act",
            "computer_state",
            "edit_file",
            "gui_plan",
            "list_dir",
            "read_file",
            "screenshot",
            "script_run",
            "shell",
            "write_file"
        ]
    );

    let shot = mcp.call("screenshot", json!({})).await;
    assert_eq!(shot["isError"], false, "{shot}");
    assert_eq!(shot["content"][1]["data"], B64.encode("phone-png"));

    let state = mcp.call("computer_state", json!({})).await;
    assert_eq!(
        state["content"][0]["text"],
        "[0] button \"Settings\" at 500,440"
    );
    let stale = mcp
        .call(
            "computer_act",
            json!({"action": "type", "text": "x", "element": 9}),
        )
        .await;
    assert_eq!(stale["isError"], true, "{stale}");
    let tap = mcp
        .call("computer_act", json!({"action": "tap", "element": 0}))
        .await;
    assert_eq!(tap["isError"], false, "{tap}");
    assert_eq!(tap["content"][1]["data"], B64.encode("phone-png"));
    let back = mcp
        .call("computer_act", json!({"action": "key", "key": "back"}))
        .await;
    assert_eq!(back["content"][0]["text"], "pressed back");

    let log = std::fs::read_to_string(h._dir.path().join("adb.log")).unwrap();
    assert!(log.contains("shell input tap 500 440\n"), "{log}");
    assert!(log.contains("shell input keyevent 4\n"), "{log}");
    assert!(!log.contains("cb-scrcpy.jar"), "{log}");
}

#[tokio::test]
async fn a_plugin_box_keeps_its_mcp_instructions_within_the_cap() {
    let h = Harness::with_plugin(Vec::new()).await;
    let init: Value = reqwest::Client::new()
        .post(format!("{}/mcp", h.endpoint))
        .header("accept", "application/json, text/event-stream")
        .header("x-cocoonbox-credential", owner())
        .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    let extra = format!("\n\n{DEMO_ROUTING}");
    let base = instructions.strip_suffix(&extra).unwrap_or(instructions);
    assert_eq!(
        instructions.ends_with(&extra),
        base.chars().count() + extra.chars().count() <= 1600,
        "{instructions}"
    );
    assert!(instructions.chars().count() <= 1600, "{instructions}");
    assert!(
        base.chars().count() + 150 <= 1600,
        "{} chars leave under 150 for routing.d",
        base.chars().count()
    );
}

#[tokio::test]
async fn a_plugin_box_gives_its_loop_the_plugin_and_the_routing_extra() {
    let run = json!({"code": "export function main(p) { return p }", "params": {"n": 2}, "screenshot": false});
    let h = Harness::with_plugin(vec![
        ("decisions", decision("plan", 0.93)),
        (
            "chat/completions",
            chat_tool_call("call_p", "demo_run", &run),
        ),
        ("chat/completions", chat_text("Done.")),
    ])
    .await;
    let routing = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../image/desk/routing.md"),
    )
    .unwrap();
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;
    send(&mut agents, &agent_id, "run the demo loop", "").await;
    let entries = wait_turn_end(&mut agents, &agent_id).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    let offered: Vec<&str> = chats[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        offered[..3],
        ["gui_plan", "script_run", "demo_run"],
        "{offered:?}"
    );
    let system = chats[0]["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.ends_with(&format!("{routing}\n\n{DEMO_ROUTING}")),
        "{system}"
    );
    let result = chats[1]["messages"].as_array().unwrap().last().unwrap();
    let report: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(report["result"], json!({"params": {"n": 2}}), "{report}");
    assert!(h.workspace.join("plugin.log").exists());
}

async fn stops_a_run_past_its_timeout(h: &Harness, tool: &str) {
    let mut mcp = McpClient::connect(h).await;

    let started = std::time::Instant::now();
    let out = mcp
        .call(
            tool,
            json!({"code": "hang", "timeout_s": 1, "screenshot": false}),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(out["isError"], true, "{out}");
    let report: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(report["error"]["kind"], "timeout", "{report}");
    assert!(!h.workspace.join("released.log").exists());
    let pid = std::fs::read_to_string(h.workspace.join("grandchild.pid")).unwrap();
    let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
    let mut alive = true;
    for _ in 0..50 {
        let state = std::fs::read_to_string(proc.join("stat")).unwrap_or_default();
        alive = !state.is_empty() && !state.contains(") Z ");
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "the script's child survived the timeout");
}

#[tokio::test]
async fn mcp_script_run_returns_once_the_script_exits_and_kills_a_child_that_holds_its_output() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;

    let started = std::time::Instant::now();
    let out = mcp
        .call(
            "script_run",
            json!({"code": "leftover", "timeout_s": 20, "screenshot": false}),
        )
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(out["isError"], false, "{out}");
    let report: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(report["result"], 7, "{report}");
    assert_eq!(report["logs"], "left", "{report}");
    assert_eq!(report["exit_code"], 0, "{report}");
    let pid = std::fs::read_to_string(h.workspace.join("leftover.pid")).unwrap();
    let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
    let mut alive = true;
    for _ in 0..50 {
        let state = std::fs::read_to_string(proc.join("stat")).unwrap_or_default();
        alive = !state.is_empty() && !state.contains(") Z ");
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "the child holding the script's output survived");
}

#[tokio::test]
async fn mcp_serves_the_desk_guide_as_a_resource() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    let names = McpClient::tool_names(&listed);
    assert!(names.contains(&"desk".to_owned()), "{names:?}");
    assert!(names.contains(&"browser_tabs".to_owned()), "{names:?}");
    for tool in listed["result"]["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        assert!(
            tool["description"].as_str().unwrap().chars().count() <= 2048,
            "{name}: Claude Code cuts a description at 2048 characters"
        );
        assert_eq!(
            tool["inputSchema"]["properties"].get("tab").is_some(),
            [
                "browser_navigate",
                "browser_snapshot",
                "browser_click",
                "browser_type",
                "browser_read"
            ]
            .contains(&name),
            "{tool}"
        );
        let always = tool["_meta"]["anthropic/alwaysLoad"] == true;
        assert_eq!(
            always,
            [
                "script_run",
                "gui_plan",
                "shell",
                "browser_navigate",
                "browser_read",
                "browser_fetch"
            ]
            .contains(&tool["name"].as_str().unwrap()),
            "{tool}"
        );
    }
    let listed = mcp.rpc("resources/list", json!({})).await;
    assert_eq!(
        listed["result"]["resources"][0]["uri"], "cocoonbox://desk/AGENTS.md",
        "{listed}"
    );
    assert_eq!(listed["result"]["resources"].as_array().unwrap().len(), 1);
    let unknown = mcp
        .rpc("resources/read", json!({"uri": "cocoonbox://desk/nope"}))
        .await;
    assert_eq!(unknown["error"]["code"], -32002, "{unknown}");
}

#[test]
fn the_shell_of_a_box_that_runs_gui_plans_names_its_time_budget() {
    use cocoonbox_agent::tools::{main_tools, remote_tools};
    for (tools, budget) in [
        (main_tools(false, Automation::Desk), true),
        (remote_tools(Automation::Desk), true),
        (main_tools(false, Automation::Plain), false),
        (remote_tools(Automation::Plain), false),
    ] {
        let shell = tools.iter().find(|t| t.name == "shell").unwrap();
        assert_eq!(
            shell
                .description
                .ends_with("Keep each command's wait well under the time left; save the required output early and improve it after."),
            budget,
            "{}",
            shell.description
        );
    }
}

#[test]
fn screenshots_of_a_box_that_runs_gui_plans_name_the_default_scale_instead_of_the_screen_size() {
    let shots = ["screenshot", "computer_state"];
    let desk = cocoonbox_agent::tools::main_tools(true, Automation::Desk)
        .into_iter()
        .chain(cocoonbox_agent::tools::computer_tools(Automation::Desk))
        .filter(|t| shots.contains(&t.name.as_ref()));
    let mut named = Vec::new();
    for t in desk {
        assert!(
            !t.description.contains("1280x800"),
            "{}: {}",
            t.name,
            t.description
        );
        assert!(
            t.description.contains("1024 pixels"),
            "{}: {}",
            t.name,
            t.description
        );
        named.push(t.name);
    }
    assert_eq!(named, shots);
    let plain = cocoonbox_agent::tools::computer_tools(Automation::Plain);
    assert!(
        plain[0].description.contains("whole 1280x800 screen"),
        "{}",
        plain[0].description
    );
}

#[tokio::test]
async fn mcp_gui_plan_keeps_a_static_description_and_lists_saved_flows_on_request() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    let plan = |listed: &Value| {
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "gui_plan")
            .unwrap()["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let bare = plan(&mcp.rpc("tools/list", json!({})).await);
    assert!(bare.contains("{flows: true}"), "{bare}");
    let none = mcp.call("gui_plan", json!({"flows": true})).await;
    assert_eq!(none["content"][0]["text"], "no saved flows yet", "{none}");

    std::fs::create_dir_all(h.workspace.join("flows")).unwrap();
    std::fs::write(
        h.workspace.join("flows/INDEX"),
        "mousepad-font-size(size): Edit > Preferences, set font size\n",
    )
    .unwrap();
    assert_eq!(plan(&mcp.rpc("tools/list", json!({})).await), bare);
    let listed = mcp.call("gui_plan", json!({"flows": true})).await;
    assert_eq!(
        listed["content"][0]["text"],
        "mousepad-font-size(size): Edit > Preferences, set font size\n",
        "{listed}"
    );

    let out = mcp
        .call("gui_plan", json!({"steps": [{"app": "mousepad"}]}))
        .await;
    assert_eq!(out["isError"], true, "{out}");
    assert!(
        out["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("/opt/desk/lib/gui_plan.mts does not exist"),
        "{out}"
    );
}

#[tokio::test]
async fn a_desk_box_gives_its_loop_and_its_mcp_clients_script_run_and_the_routing() {
    let run = json!({"code": "export function main(p) { return p }", "params": {"n": 1}, "screenshot": false});
    let h = Harness::with_desk(vec![
        ("decisions", decision("plan", 0.93)),
        (
            "chat/completions",
            chat_tool_call("call_r", "script_run", &run),
        ),
        ("chat/completions", chat_text("Saved.")),
    ])
    .await;
    let routing = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../image/desk/routing.md"),
    )
    .unwrap();
    let mut agents = h.agents().await;
    let agent_id = create_agent(&mut agents).await;
    send(&mut agents, &agent_id, "save a note in mousepad", "").await;
    let entries = wait_turn_end(&mut agents, &agent_id).await;
    assert!(
        matches!(entries.last().and_then(|e| e.body.as_ref()), Some(Body::TurnEnd(end)) if end.ok)
    );
    let chats = gw_bodies(&h.gw_log, "chat/completions");
    let offered: Vec<&str> = chats[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(offered[..2], ["gui_plan", "script_run"], "{offered:?}");
    assert!(offered.contains(&"delegate_computer"), "{offered:?}");
    assert!(!offered.contains(&"cua_route"), "{offered:?}");
    let system = chats[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.ends_with(&routing), "{system}");
    let asked = gw_bodies(&h.gw_log, "decisions");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["state"]["task"], "save a note in mousepad");
    assert_eq!(asked[0]["model"], "typesafe/jev-1.14");
    let request = &chats[0]["messages"][1]["content"];
    assert_eq!(
        request[0]["text"],
        "Router: this task is tier plan (0.93): run desktop-app GUI work as one gui_plan call with the whole plan, also a batch of files moved, copied or renamed in a file manager (select them by pattern, not one by one); write one script on the desk API for script_run only for logic a plan cannot hold."
    );
    assert_eq!(request[1]["text"], "save a note in mousepad");
    assert!(entries.iter().any(|e| e.role == i32::from(pb::Role::System)
        && matches!(&e.body, Some(Body::Text(t)) if t.text.starts_with("Router: this task is tier plan"))));
    let result = chats[1]["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(result["role"], "tool");
    let report: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(report["result"], json!({"params": {"n": 1}}), "{report}");
    let Some(Body::TurnEnd(end)) = entries.last().and_then(|e| e.body.as_ref()) else {
        panic!("want turn end, got {:?}", entries.last());
    };
    let usage = end.usage.as_ref().unwrap();
    let mut models: Vec<(&str, u64, u64, u64, u64)> = usage
        .models
        .iter()
        .map(|m| {
            (
                m.model.as_str(),
                m.calls,
                m.input_tokens,
                m.output_tokens,
                m.cached_input_tokens,
            )
        })
        .collect();
    models.sort_unstable();
    let mut want = vec![
        (chats[0]["model"].as_str().unwrap(), 2, 250, 15, 100),
        ("typesafe/jev-1.14", 1, 1000, 50, 0),
    ];
    want.sort_unstable();
    assert_eq!(models, want);
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.cached_input_tokens
        ),
        (1250, 65, 100)
    );

    let golden: Value = serde_json::from_slice(
        &std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/mcp-initialize.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let init: Value = reqwest::Client::new()
        .post(format!("{}/mcp", h.endpoint))
        .header("accept", "application/json, text/event-stream")
        .header("x-cocoonbox-credential", owner())
        .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(
        instructions.ends_with(&format!("\n\n{routing}")),
        "{instructions}"
    );
    assert_ne!(
        instructions.lines().next(),
        golden["result"]["instructions"]
            .as_str()
            .unwrap()
            .lines()
            .next()
    );
    let version = std::fs::read_to_string("/etc/os-release")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VERSION_ID="))
        .unwrap()
        .trim_matches('"')
        .to_owned();
    let machine = instructions.lines().next().unwrap();
    assert!(
        machine.starts_with("These tools act on a cloud computer: ") && machine.contains(&version),
        "{machine}"
    );
    assert!(
        instructions.chars().count() <= 1600,
        "{} chars: the bridge prefix must keep the total under Claude Code's 2048 cap",
        instructions.chars().count()
    );
}

#[tokio::test]
async fn a_desk_box_serves_no_cua_route_over_mcp() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    let tools = McpClient::tool_names(&mcp.rpc("tools/list", json!({})).await);
    assert!(tools.contains(&"script_run".to_owned()), "{tools:?}");
    assert!(!tools.contains(&"cua_route".to_owned()), "{tools:?}");
    let refused = mcp
        .rpc(
            "tools/call",
            json!({"name": "cua_route", "arguments": {"task": "x"}}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(gw_bodies(&h.gw_log, "decisions").is_empty());
}

#[tokio::test]
async fn a_desk_box_points_a_streak_of_computer_acts_to_script_run_once() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    let act = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "computer_act")
        .unwrap();
    assert!(
        act["description"].as_str().unwrap().contains("script_run"),
        "{act}"
    );
    let press = json!({"action": "key", "key": "a"});
    let mut nudged = Vec::new();
    for _ in 0..3 {
        let out = mcp.call("computer_act", press.clone()).await;
        nudged.push(
            out["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("GUI steps in a row"),
        );
    }
    mcp.call("list_dir", json!({})).await;
    for _ in 0..2 {
        let out = mcp.call("computer_act", press.clone()).await;
        nudged.push(
            out["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("GUI steps in a row"),
        );
    }
    assert_eq!(nudged, [false, true, false, false, true]);

    let launch = json!({"action": "launch_app", "app": "mousepad"});
    let click = json!({"action": "click", "x": 10, "y": 10});
    let mut nudged = Vec::new();
    mcp.call("list_dir", json!({})).await;
    for args in [&launch, &click, &launch, &click] {
        let out = mcp.call("computer_act", args.clone()).await;
        nudged.push(
            out["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("GUI steps in a row"),
        );
    }
    assert_eq!(nudged, [false, false, false, true]);
}

#[tokio::test]
async fn a_desk_box_runs_a_computer_act_list_until_its_first_failure() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    let act = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "computer_act")
        .unwrap();
    let schema = &act["inputSchema"];
    assert!(schema.get("required").is_none(), "{act}");
    assert_eq!(
        schema["properties"]["actions"]["items"]["required"],
        json!(["action"])
    );
    assert!(
        schema["properties"]["actions"]["items"]["properties"]
            .get("max_edge")
            .is_none()
    );

    let out = mcp
        .call(
            "computer_act",
            json!({"actions": [
                {"action": "click", "x": 10, "y": 20},
                {"action": "click"},
                {"action": "key", "key": "a"}
            ]}),
        )
        .await;
    assert_eq!(out["isError"], true, "{out}");
    assert_eq!(
        out["content"][0]["text"],
        "1. click: ✅ click done\n2. click failed: click needs element or x and y\nthe remaining 1 actions did not run"
    );
    assert_eq!(out["content"][1]["type"], "image");
    assert_eq!(out["content"].as_array().unwrap().len(), 2);
    assert!(driver_calls(&h, "press_key").is_empty());
    let probes: Vec<String> = driver_calls(&h, "end_session")
        .iter()
        .map(|a| a["session"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert!(probes[0].starts_with("still-"), "{probes:?}");
    let probed = driver_calls(&h, "get_desktop_state")
        .iter()
        .filter(|a| a["session"] == probes[0].as_str())
        .count();
    assert!(probed >= 2, "{probed}");
}

#[tokio::test]
async fn a_desk_box_maps_clicks_by_the_scale_of_the_frame_it_returned_last() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    let click = json!({"action": "click", "x": 10, "y": 20});

    mcp.call("screenshot", json!({"max_edge": 640})).await;
    mcp.call("computer_act", click.clone()).await;
    mcp.call("computer_act", click.clone()).await;

    let log = h.cua_log.lock().unwrap().clone();
    let session = log
        .iter()
        .find(|(n, a)| n == "get_desktop_state" && a["max_image_dimension"] == 640)
        .map(|(_, a)| a["session"].clone())
        .unwrap();
    let mine: Vec<&(String, Value)> = log
        .iter()
        .filter(|(_, a)| a["session"] == session)
        .collect();
    let clicks: Vec<(f64, f64)> = mine
        .iter()
        .filter(|(n, _)| n == "click")
        .map(|(_, a)| (a["x"].as_f64().unwrap(), a["y"].as_f64().unwrap()))
        .collect();
    assert_eq!(clicks, [(20.0, 40.0), (12.5, 25.0)], "{mine:?}");
    for (i, (name, _)) in mine.iter().enumerate().filter(|(_, (n, _))| n == "click") {
        let capture = mine[..i]
            .iter()
            .rev()
            .find(|(n, _)| n == "get_desktop_state")
            .unwrap();
        assert!(
            capture.1.get("max_image_dimension").is_none(),
            "{name} after {capture:?}"
        );
    }
}

#[tokio::test]
async fn a_desk_box_scales_screenshots_to_1024_by_default_and_weighs_them() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    let listed = mcp.rpc("tools/list", json!({})).await;
    let schema = |name: &str| {
        listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["inputSchema"]
            .clone()
    };
    assert_eq!(
        schema("screenshot")["properties"]["region"]["required"],
        json!(["x", "y", "w", "h"])
    );
    for name in ["screenshot", "computer_state", "computer_act", "script_run"] {
        let edge = &schema(name)["properties"]["max_edge"]["description"];
        assert!(
            edge.as_str().unwrap().contains("default 1024"),
            "{name}: {edge}"
        );
    }

    let shot = mcp.call("screenshot", json!({})).await;
    assert_eq!(shot["content"][1]["type"], "image");
    assert_eq!(
        shot["content"][0]["text"],
        "screenshot of the whole screen: 1 KB"
    );
    mcp.call("screenshot", json!({"max_edge": 0})).await;
    let state = mcp.call("computer_state", json!({})).await;
    assert!(
        state["content"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("\nScreenshot: 1 KB"),
        "{state}"
    );
    let edges: Vec<Value> = driver_calls(&h, "get_desktop_state")
        .iter()
        .map(|a| a["max_image_dimension"].clone())
        .collect();
    assert_eq!(edges[0], 1024, "{edges:?}");
    assert_eq!(edges.iter().filter(|e| **e == 1024).count(), 2, "{edges:?}");
    assert!(edges.contains(&json!(0)), "{edges:?}");
}

#[tokio::test]
async fn a_desk_box_takes_triple_click_and_left_click_as_clicks() {
    let h = Harness::with_desk(Vec::new()).await;
    let mut mcp = McpClient::connect(&h).await;
    for action in ["triple_click", "left_click"] {
        let out = mcp
            .call("computer_act", json!({"action": action, "x": 10, "y": 20}))
            .await;
        assert_eq!(out["isError"], false, "{action}: {out}");
    }
    let clicks = driver_calls(&h, "click");
    assert_eq!(clicks.len(), 2, "{clicks:?}");
    assert_eq!(clicks[0]["count"], 3, "{clicks:?}");
    assert!(clicks[1].get("count").is_none(), "{clicks:?}");
    let at: Vec<(f64, f64)> = clicks
        .iter()
        .map(|c| (c["x"].as_f64().unwrap(), c["y"].as_f64().unwrap()))
        .collect();
    assert_eq!(at, [(10.0, 20.0), (12.5, 25.0)], "{clicks:?}");
}

#[tokio::test]
async fn a_cbdesk_box_never_nudges_a_computer_act_streak() {
    let h = Harness::start(Vec::new(), false).await;
    let mut mcp = McpClient::connect(&h).await;
    for _ in 0..5 {
        let out = mcp
            .call("computer_act", json!({"action": "key", "key": "a"}))
            .await;
        assert_eq!(out["isError"], false, "{out}");
        assert!(
            !out["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("script_run"),
            "{out}"
        );
    }
}

#[tokio::test]
async fn mcp_share_sessions_are_filtered_recorded_and_bound_to_their_share() {
    let h = Harness::start(Vec::new(), false).await;
    let edge = Edge::provision(&h);
    let reader = edge.share("share:sh-1:read");
    let mut mcp = McpClient::connect_as(&h, Some(reader.clone())).await;

    let listed = mcp.rpc("tools/list", json!({})).await;
    let mut read_only: Vec<&str> = cocoonbox_agent::tools::READ_ONLY.to_vec();
    read_only.sort_unstable();
    assert_eq!(McpClient::tool_names(&listed), read_only);
    let refused = mcp
        .rpc(
            "tools/call",
            json!({"name": "shell", "arguments": {"command": "touch nope"}}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");
    assert!(!h.workspace.join("nope").exists());
    let dir = mcp.call("list_dir", json!({})).await;
    assert_eq!(dir["isError"], false, "{dir}");

    let mut agents = h.agents().await;
    let owner = edge.owner();
    let listed = agents
        .list(signed(pb::ListAgentsRequest {}, &owner))
        .await
        .unwrap()
        .into_inner();
    let names: Vec<&str> = listed.agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["mcp:test-client@sh-1"]);
    let agent_id = listed.agents[0].id.clone();
    let page = agents
        .transcript(signed(
            pb::TranscriptRequest {
                agent_id: agent_id.clone(),
                after_seq: 0,
                limit: 0,
            },
            &owner,
        ))
        .await
        .unwrap()
        .into_inner();
    let calls: Vec<&str> = page
        .entries
        .iter()
        .filter_map(|e| match &e.body {
            Some(Body::ToolCall(c)) => Some(c.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["list_dir"]);

    let mut stream = mcp
        .authorize(mcp.http.get(&mcp.url).header("accept", "text/event-stream"))
        .send()
        .await
        .unwrap();
    assert!(stream.chunk().await.unwrap().is_some());
    mcp.credential = Some(edge.share("share:sh-1:write"));
    mcp.rpc("ping", json!({})).await;
    let changed = tokio::time::timeout(Duration::from_secs(5), stream.chunk())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&changed).contains("notifications/tools/list_changed"),
        "{changed:?}"
    );
    drop(stream);
    assert_eq!(
        McpClient::tool_names(&mcp.rpc("tools/list", json!({})).await).len(),
        13
    );
    let shell = mcp
        .call("shell", json!({"command": "echo guest > g.txt"}))
        .await;
    assert_eq!(shell["isError"], false, "{shell}");
    assert!(h.workspace.join("g.txt").exists());
    mcp.credential = Some(reader.clone());
    let refused = mcp
        .rpc(
            "tools/call",
            json!({"name": "write_file", "arguments": {"path": "x", "content": ""}}),
        )
        .await;
    assert_eq!(refused["error"]["code"], -32602, "{refused}");

    for other in [
        edge.owner(),
        edge.share("share:sh-2:write"),
        edge.share("share:sh-1:read:u-1"),
    ] {
        let stranger = McpClient {
            http: reqwest::Client::new(),
            url: mcp.url.clone(),
            session: mcp.session.clone(),
            next_id: 0,
            credential: Some(other),
        };
        let resp = stranger
            .post(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await;
        assert_eq!(resp.status(), 404, "another principal used the session");
    }

    let shared = signed(pb::ListAgentsRequest {}, &reader);
    assert_eq!(
        agents.list(shared).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    let mut screen = h.session().await;
    assert_eq!(
        screen
            .hello(signed(pb::HelloRequest::default(), &reader))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    let plan = state_client(&h)
        .await
        .plan(signed(pb::PlanRequest::default(), &reader))
        .await;
    assert_eq!(plan.unwrap_err().code(), Code::PermissionDenied);
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(pb::ScreenClientFrame {
        body: Some(screen_client_frame::Body::Open(pb::ScreenOpen {})),
    })
    .await
    .unwrap();
    let mut frames = screen
        .screen(signed(tokio_stream_from(rx), &reader))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        &frames.message().await.unwrap().unwrap().data[..],
        b"RFB 003.008\n"
    );

    let end = |target| {
        signed(
            pb::EndRequest {
                target: Some(target),
            },
            &owner,
        )
    };
    agents
        .end(end(pb::end_request::Target::AgentId(agent_id.clone())))
        .await
        .unwrap();
    let gone = mcp
        .post(json!({"jsonrpc": "2.0", "id": 99, "method": "tools/list"}))
        .await;
    assert_eq!(gone.status(), 404);
    assert_eq!(
        agents
            .end(end(pb::end_request::Target::AgentId(agent_id.clone())))
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    let second = McpClient::connect_as(&h, Some(reader.clone())).await;
    let other = McpClient::connect_as(&h, Some(edge.share("share:sh-2:read"))).await;
    agents
        .end(end(pb::end_request::Target::ShareId("sh-1".into())))
        .await
        .unwrap();
    let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    assert_eq!(second.post(ping.clone()).await.status(), 404);
    assert_eq!(
        other.post(ping).await.status(),
        200,
        "ending sh-1 ended a session of sh-2"
    );
    let statuses: Vec<(String, i32)> = agents
        .list(signed(pb::ListAgentsRequest {}, &owner))
        .await
        .unwrap()
        .into_inner()
        .agents
        .into_iter()
        .map(|a| (a.name, a.status))
        .collect();
    let idle = i32::from(pb::AgentStatus::Idle);
    assert_eq!(
        statuses,
        [
            ("mcp:test-client@sh-1".to_owned(), idle),
            ("mcp:test-client@sh-1".to_owned(), idle),
            (
                "mcp:test-client@sh-2".to_owned(),
                i32::from(pb::AgentStatus::Running)
            ),
        ]
    );
}

#[tokio::test]
async fn a_share_principal_holds_at_most_four_open_sessions() {
    let h = Harness::start(Vec::new(), false).await;
    let edge = Edge::provision(&h);
    let reader = edge.share("share:sh-1:read");
    let mut open = Vec::new();
    for _ in 0..4 {
        open.push(McpClient::connect_as(&h, Some(reader.clone())).await);
    }
    let fifth = McpClient {
        http: reqwest::Client::new(),
        url: open[0].url.clone(),
        session: String::new(),
        next_id: 0,
        credential: Some(reader.clone()),
    };
    let init = json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "test-client", "version": "1.0"}
    }});
    let refused = fifth.post(init).await;
    assert_eq!(refused.status(), 429);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32000, "{body}");
    let listed = h
        .agents()
        .await
        .list(signed(pb::ListAgentsRequest {}, &edge.owner()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        listed.agents.len(),
        4,
        "the refused initialize created an agent"
    );

    McpClient::connect_as(&h, Some(edge.share("share:sh-1:read:u-1"))).await;
    for _ in 0..5 {
        McpClient::connect_as(&h, Some(edge.owner())).await;
    }
    let closed = open[0]
        .authorize(reqwest::Client::new().delete(&open[0].url))
        .send()
        .await
        .unwrap();
    assert_eq!(closed.status(), 204);
    McpClient::connect_as(&h, Some(reader)).await;
}

#[tokio::test]
async fn a_read_share_of_a_desk_or_android_box_does_not_read_its_saved_flows() {
    let h = Harness::with_desk(Vec::new()).await;
    let edge = Edge::provision(&h);
    std::fs::create_dir_all(h.workspace.join("flows")).unwrap();
    std::fs::write(h.workspace.join("flows/save.json"), "{\"typed\":\"x\"}").unwrap();
    std::fs::write(h.workspace.join("notes.txt"), "shared\n").unwrap();
    let text = |out: &Value| out["content"][0]["text"].as_str().unwrap().to_owned();

    let mut reader = McpClient::connect_as(&h, Some(edge.share("share:sh-1:read"))).await;
    for (tool, args) in [
        ("read_file", json!({"path": "flows/save.json"})),
        ("list_dir", json!({"path": "flows"})),
    ] {
        let out = reader.call(tool, args).await;
        assert_eq!(out["isError"], true, "{tool}: {out}");
        assert!(
            text(&out).contains("does not read saved flows"),
            "{tool}: {out}"
        );
    }
    let out = reader.call("read_file", json!({"path": "notes.txt"})).await;
    assert!(text(&out).contains("shared"), "{out}");

    let mut owner = McpClient::connect_as(&h, Some(edge.owner())).await;
    let out = owner
        .call("read_file", json!({"path": "flows/save.json"}))
        .await;
    assert!(text(&out).contains("typed"), "{out}");

    let android = Harness::with_android().await;
    let edge = Edge::provision(&android);
    std::fs::create_dir_all(android.workspace.join("flows")).unwrap();
    std::fs::write(android.workspace.join("flows/save.json"), "{}").unwrap();
    let mut reader = McpClient::connect_as(&android, Some(edge.share("share:sh-1:read"))).await;
    let out = reader
        .call("read_file", json!({"path": "flows/save.json"}))
        .await;
    assert!(
        text(&out).contains("does not read saved flows"),
        "an Android read share: {out}"
    );

    let desk = Harness::start(Vec::new(), false).await;
    let edge = Edge::provision(&desk);
    std::fs::create_dir_all(desk.workspace.join("flows")).unwrap();
    std::fs::write(desk.workspace.join("flows/save.json"), "{}").unwrap();
    let mut reader = McpClient::connect_as(&desk, Some(edge.share("share:sh-1:read"))).await;
    let out = reader
        .call("read_file", json!({"path": "flows/save.json"}))
        .await;
    assert_eq!(
        out["isError"], false,
        "a cbdesk read share is unchanged: {out}"
    );
}

#[tokio::test]
async fn a_read_share_reads_files_only_under_the_workspace() {
    let h = Harness::start(Vec::new(), false).await;
    let edge = Edge::provision(&h);
    std::fs::create_dir_all(h.workspace.join("docs")).unwrap();
    std::fs::write(h.workspace.join("docs/x.txt"), "shared\n").unwrap();
    let claude = h.home.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("x"), "secret\n").unwrap();
    std::os::unix::fs::symlink(&claude, h.workspace.join("abs")).unwrap();
    std::os::unix::fs::symlink("../home/.claude", h.workspace.join("rel")).unwrap();
    let ws = h.workspace.display().to_string();
    let secret = claude.join("x").display().to_string();
    let text = |out: &Value| out["content"][0]["text"].as_str().unwrap().to_owned();

    let mut reader = McpClient::connect_as(&h, Some(edge.share("share:sh-1:read"))).await;
    for path in [
        "docs/x.txt".to_owned(),
        format!("{ws}/docs/x.txt"),
        format!("{ws}/docs/../docs/x.txt"),
    ] {
        let out = reader.call("read_file", json!({"path": path})).await;
        assert_eq!(out["isError"], false, "{path}: {out}");
        assert!(text(&out).contains("shared"), "{path}: {out}");
    }
    for path in [json!({}), json!({"path": ws}), json!({"path": "docs"})] {
        let out = reader.call("list_dir", path.clone()).await;
        assert_eq!(out["isError"], false, "{path}: {out}");
    }
    assert!(text(&reader.call("list_dir", json!({})).await).contains("docs/"));
    let escapes = [
        (secret.clone(), claude.display().to_string()),
        (
            format!("{ws}/../home/.claude/x"),
            format!("{ws}/../home/.claude"),
        ),
        ("../home/.claude/x".to_owned(), "..".to_owned()),
        (
            "docs/../../home/.claude/x".to_owned(),
            "docs/../..".to_owned(),
        ),
        ("abs/x".to_owned(), "abs".to_owned()),
        ("rel/x".to_owned(), "rel".to_owned()),
    ];
    for (file, dir) in &escapes {
        for (tool, path) in [("read_file", file), ("list_dir", dir)] {
            for args in [
                json!({"path": path}),
                json!({"path": path, "workspace_only": false}),
            ] {
                let out = reader.call(tool, args).await;
                assert_eq!(out["isError"], true, "{tool} {path}: {out}");
                assert!(
                    text(&out).contains("is refused: a read-only share reads files only under"),
                    "{tool} {path}: {out}"
                );
                assert!(!text(&out).contains("secret"), "{tool} {path}: {out}");
            }
        }
    }

    let mut owner = McpClient::connect_as(&h, Some(edge.owner())).await;
    let mut writer = McpClient::connect_as(&h, Some(edge.share("share:sh-1:write"))).await;
    for client in [&mut owner, &mut writer] {
        for (file, dir) in &escapes {
            let out = client.call("read_file", json!({"path": file})).await;
            assert!(text(&out).contains("secret"), "{file}: {out}");
            let out = client.call("list_dir", json!({"path": dir})).await;
            assert_eq!(out["isError"], false, "{dir}: {out}");
        }
    }
}

#[tokio::test]
async fn only_a_box_that_runs_gui_plans_says_that_shell_capped_its_timeout() {
    for (h, note) in [
        (Harness::start(Vec::new(), false).await, false),
        (Harness::with_desk(Vec::new()).await, true),
    ] {
        let mut mcp = McpClient::connect(&h).await;
        let out = mcp
            .call("shell", json!({"command": "echo hi", "timeout_s": 2400}))
            .await;
        assert_eq!(out["isError"], false, "{out}");
        let text = out["content"][0]["text"].as_str().unwrap();
        assert_eq!(
            text.ends_with(
                "[exit status 0]\n[timeout_s 2400 is over the 600 s maximum; ran with 600 s]"
            ),
            note,
            "{text}"
        );
        assert_eq!(text.ends_with("[exit status 0]"), !note, "{text}");
    }
}

#[tokio::test]
async fn a_quiesced_plan_waits_for_a_running_mcp_call() {
    let h = Harness::start(Vec::new(), false).await;
    let mut mcp = McpClient::connect(&h).await;
    let call = tokio::spawn(async move {
        mcp.call("shell", json!({"command": "sleep 2; echo slept"}))
            .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    plan(&h, pb::Consistency::Quiesced, "").await;
    let waited = started.elapsed();
    let out = call.await.unwrap();
    assert_eq!(out["isError"], false, "{out}");
    assert!(
        waited >= Duration::from_millis(1500),
        "the quiesced plan took {waited:?}, so it did not wait for the running MCP call"
    );
}

#[tokio::test]
async fn mcp_ends_the_driver_session_on_stream_close_and_idle() {
    let day = Duration::from_secs(24 * 3600);
    let h = Harness::start_with(Vec::new(), false, Duration::from_millis(400), day).await;
    let mut mcp = McpClient::connect(&h).await;
    mcp.call("screenshot", json!({})).await;
    let first = driver_calls(&h, "get_desktop_state")[0]["session"].clone();

    let mut stream = mcp
        .http
        .get(&mcp.url)
        .header("x-cocoonbox-credential", owner())
        .header("mcp-session-id", &mcp.session)
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    assert!(stream.chunk().await.unwrap().is_some());
    drop(stream);
    for _ in 0..100 {
        if !driver_calls(&h, "end_session").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(driver_calls(&h, "end_session")[0]["session"], first);

    mcp.call("screenshot", json!({})).await;
    let second = driver_calls(&h, "get_desktop_state")[1]["session"].clone();
    assert_ne!(first, second);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let ended = driver_calls(&h, "end_session");
    assert_eq!(ended.len(), 2, "{ended:?}");
    assert_eq!(ended[1]["session"], second);
    let gone = mcp
        .post(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"}))
        .await;
    assert_eq!(gone.status(), 404);
}

#[tokio::test]
async fn concurrent_computer_acts_do_not_interleave() {
    let h = Harness::start(Vec::new(), false).await;
    let mut a = McpClient::connect(&h).await;
    let mut b = McpClient::connect(&h).await;
    let (ra, rb) = tokio::join!(
        a.call("computer_act", json!({"action": "click", "x": 1, "y": 1})),
        b.call("computer_act", json!({"action": "click", "x": 2, "y": 2}))
    );
    assert_eq!(
        (ra["isError"].clone(), rb["isError"].clone()),
        (json!(false), json!(false))
    );
    let log: Vec<(String, String)> = h
        .cua_log
        .lock()
        .unwrap()
        .iter()
        .map(|(n, a)| {
            (
                n.clone(),
                a["session"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert_eq!(log.len(), 4, "{log:?}");
    assert_eq!(
        (log[0].0.as_str(), log[1].0.as_str()),
        ("click", "get_desktop_state")
    );
    assert_eq!(
        (log[2].0.as_str(), log[3].0.as_str()),
        ("click", "get_desktop_state")
    );
    assert_eq!(log[0].1, log[1].1);
    assert_eq!(log[2].1, log[3].1);
    assert_ne!(log[0].1, log[2].1);

    let raw = a
        .post(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2099-01-01",
                "clientInfo": {"name": "future"}
            }}),
        )
        .await;
    let init: Value = raw.json().await.unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
}

#[tokio::test]
async fn pixel_double_and_right_clicks_are_desktop_clicks() {
    let h = Harness::start(Vec::new(), false).await;
    let mut mcp = McpClient::connect(&h).await;
    for action in ["double_click", "right_click"] {
        let act = mcp
            .call("computer_act", json!({"action": action, "x": 30, "y": 40}))
            .await;
        assert_eq!(act["isError"], false, "{act}");
    }
    let clicks = driver_calls(&h, "click");
    let desktop = json!({"kind": "desktop", "display_id": "primary"});
    assert_eq!(
        (&clicks[0]["count"], &clicks[0]["target"]),
        (&json!(2), &desktop)
    );
    assert_eq!(
        (&clicks[1]["button"], &clicks[1]["target"]),
        (&json!("right"), &desktop)
    );

    mcp.call("computer_state", json!({})).await;
    for action in ["double_click", "right_click"] {
        let act = mcp
            .call(
                "computer_act",
                json!({"action": action, "element": 0, "button": "left"}),
            )
            .await;
        assert_eq!(act["isError"], false, "{act}");
        let call = &driver_calls(&h, action)[0];
        assert_eq!(
            (&call["element_token"], &call["pid"]),
            (&json!("tok-0"), &json!(42))
        );
        assert!(call.get("button").is_none(), "{call}");
    }
}

#[tokio::test]
async fn non_ascii_text_is_typed_in_calls_of_at_most_eight_distinct_characters() {
    let h = Harness::start(Vec::new(), false).await;
    let mut mcp = McpClient::connect(&h).await;
    let out = mcp
        .call(
            "computer_act",
            json!({"action": "type", "text": "hi — 你好 🙂 一二三四五六七"}),
        )
        .await;
    assert_eq!(out["isError"], false, "{out}");
    let typed: Vec<(String, Value, Value)> = driver_calls(&h, "type_text")
        .into_iter()
        .map(|a| {
            (
                a["text"].as_str().unwrap().to_owned(),
                a["pid"].clone(),
                a["window_id"].clone(),
            )
        })
        .collect();
    let texts: Vec<&str> = typed.iter().map(|t| t.0.as_str()).collect();
    assert_eq!(texts, ["hi — 你好 🙂 一二三四", "五六七"]);
    assert!(typed.iter().all(|t| t.1 == 42 && t.2 == 7), "{typed:?}");
    let ascii = mcp
        .call(
            "computer_act",
            json!({"action": "type", "text": "plain ascii"}),
        )
        .await;
    assert_eq!(ascii["isError"], false);
    assert_eq!(driver_calls(&h, "type_text").len(), 3);
}

#[tokio::test]
async fn a_quiesce_interrupts_a_guests_long_call_but_not_the_owners() {
    let h = Harness::start(Vec::new(), false).await;
    let edge = Edge::provision(&h);
    let owner = edge.owner();
    let quiesced = |credential: &str| {
        signed(
            pb::PlanRequest {
                key: Some(state_key()),
                consistency: pb::Consistency::Quiesced.into(),
                parent: String::new(),
            },
            credential,
        )
    };

    let mut guest = McpClient::connect_as(&h, Some(edge.share("share:sh-1:write"))).await;
    let call = tokio::spawn(async move {
        guest
            .call(
                "shell",
                json!({"command": "sleep 100 & echo $! > guest.pid; wait", "timeout_s": 600}),
            )
            .await
    });
    let pid = h.workspace.join("guest.pid");
    for _ in 0..100 {
        if pid.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let started = std::time::Instant::now();
    state_client(&h).await.plan(quiesced(&owner)).await.unwrap();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(4),
        "the plan waited {took:?} on a guest"
    );
    let out = call.await.unwrap();
    assert_eq!(out["isError"], true, "{out}");
    assert!(
        out["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("interrupted"),
        "{out}"
    );
    assert!(
        wait_gone(&pid).await,
        "the guest's sleep outlived the quiesce"
    );

    let mut mine = McpClient::connect_as(&h, Some(owner.clone())).await;
    let call = tokio::spawn(async move { mine.call("shell", json!({"command": "sleep 8"})).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let busy = state_client(&h).await.plan(quiesced(&owner)).await;
    assert_eq!(busy.unwrap_err().code(), Code::Unavailable);
    assert_eq!(call.await.unwrap()["isError"], false);
}

#[tokio::test]
async fn closed_mcp_agents_are_pruned_after_the_prune_age() {
    let h = Harness::start_with(
        Vec::new(),
        false,
        Duration::from_millis(400),
        Duration::from_millis(300),
    )
    .await;
    let mut agents = h.agents().await;
    let loop_agent = create_agent(&mut agents).await;
    let edge = Edge::provision(&h);
    let guest = edge.share("share:sh-1:write");
    let mut closed = McpClient::connect_as(&h, Some(guest.clone())).await;
    let job = closed
        .call(
            "shell",
            json!({"command": "sleep 300 & echo $! > mcp-job.pid"}),
        )
        .await;
    assert_eq!(job["isError"], false, "{job}");
    let job = h.workspace.join("mcp-job.pid");
    assert!(alive(&job));
    let deleted = closed
        .authorize(reqwest::Client::new().delete(format!("{}/mcp", h.endpoint)))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 204);
    let mut open = McpClient::connect_as(&h, Some(guest)).await;
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        open.rpc("ping", json!({})).await;
    }
    let names: Vec<(String, String)> = agents
        .list(signed(pb::ListAgentsRequest {}, &edge.owner()))
        .await
        .unwrap()
        .into_inner()
        .agents
        .into_iter()
        .map(|a| (a.id, a.name))
        .collect();
    let mcp_count = names
        .iter()
        .filter(|(_, n)| n == "mcp:test-client@sh-1")
        .count();
    assert_eq!(mcp_count, 1, "{names:?}");
    assert!(names.iter().any(|(id, _)| *id == loop_agent), "{names:?}");
    assert!(wait_gone(&job).await, "the pruned agent's job survived");
}

#[tokio::test]
async fn mcp_agents_are_deleted_only_after_their_session_closes() {
    let h = Harness::start(Vec::new(), false).await;
    let edge = Edge::provision(&h);
    let owner = edge.owner();
    let mcp = McpClient::connect_as(&h, Some(edge.share("share:sh-1:read:u-2"))).await;
    let mut agents = h.agents().await;
    let agent_id = agents
        .list(signed(pb::ListAgentsRequest {}, &owner))
        .await
        .unwrap()
        .into_inner()
        .agents
        .into_iter()
        .find(|a| a.name == "mcp:test-client@sh-1:u-2")
        .unwrap()
        .id;
    let refused = agents
        .delete(signed(
            pb::DeleteAgentRequest {
                agent_id: agent_id.clone(),
            },
            &owner,
        ))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(
        mcp.post(json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}))
            .await
            .status(),
        200
    );
    let closed = mcp
        .authorize(reqwest::Client::new().delete(format!("{}/mcp", h.endpoint)))
        .send()
        .await
        .unwrap();
    assert_eq!(closed.status(), 204);
    agents
        .delete(signed(
            pb::DeleteAgentRequest {
                agent_id: agent_id.clone(),
            },
            &owner,
        ))
        .await
        .unwrap();
    let listed = agents
        .list(signed(pb::ListAgentsRequest {}, &owner))
        .await
        .unwrap()
        .into_inner();
    assert!(listed.agents.is_empty(), "{:?}", listed.agents);
}

#[tokio::test]
async fn each_mcp_session_types_into_its_own_chrome_window() {
    let Some(chrome) = test_chrome() else {
        return;
    };
    let site = field_pages().await;
    let url = |path: &str| format!("http://{site}{path}");
    let site_b = format!("http://{}", site.replace("127.0.0.1", "localhost"));
    let url_b = |path: &str| format!("{site_b}{path}");
    let h = Harness::with_chrome(Vec::new(), &chrome).await;
    let mut a = McpClient::connect(&h).await;
    let mut b = McpClient::connect(&h).await;
    let (na, nb) = tokio::join!(
        a.call("browser_navigate", json!({"url": url("/a")})),
        b.call("browser_navigate", json!({"url": url_b("/b")}))
    );
    for (nav, field) in [(&na, "field /a"), (&nb, "field /b")] {
        let text = nav["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains(&format!("textbox \"{field}\" [ref=e1]")),
            "{text}"
        );
    }

    let (text_a, text_b) = ("typed by a; ".repeat(50), "typed by b; ".repeat(50));
    let (ta, tb) = tokio::join!(
        a.call("browser_type", json!({"ref": "e1", "text": text_a})),
        b.call("browser_type", json!({"ref": "e1", "text": text_b}))
    );
    assert_eq!(
        (&ta["isError"], &tb["isError"]),
        (&json!(false), &json!(false))
    );
    let open = tabs(h.cdp_port).await;
    let tab = |u: String| open.iter().find(|t| t.1 == u).cloned().unwrap();
    let (tab_a, tab_b) = (tab(url("/a")), tab(url_b("/b")));
    assert_ne!(tab_a.0, tab_b.0);
    let value = "document.querySelector('input').value";
    assert_eq!(tab_eval(&tab_a.2, value).await, text_a.as_str());
    assert_eq!(tab_eval(&tab_b.2, value).await, text_b.as_str());

    reqwest::get(format!(
        "http://127.0.0.1:{}/json/close/{}",
        h.cdp_port, tab_a.0
    ))
    .await
    .unwrap();
    assert!(tab_gone(h.cdp_port, &url("/a")).await);
    let reopened = a.call("browser_navigate", json!({"url": url("/a2")})).await;
    assert_eq!(reopened["isError"], false, "{reopened}");
    let tab_a2 = tabs(h.cdp_port)
        .await
        .into_iter()
        .find(|t| t.1 == url("/a2"))
        .unwrap();
    assert!(tab_a2.0 != tab_a.0 && tab_a2.0 != tab_b.0, "{tab_a2:?}");
    assert_eq!(tab_eval(&tab_b.2, value).await, text_b.as_str());

    let mut session = h.session().await;
    for (client, origin) in [(&mut b, site_b.clone()), (&mut a, format!("http://{site}"))] {
        client.call("browser_snapshot", json!({})).await;
        let active = session
            .active_origin(authed(pb::ActiveOriginRequest {}))
            .await
            .unwrap()
            .into_inner()
            .origin;
        assert_eq!(
            active, origin,
            "ActiveOrigin follows the window last acted on"
        );
    }

    let closed = reqwest::Client::new()
        .delete(format!("{}/mcp", h.endpoint))
        .header("x-cocoonbox-credential", owner())
        .header("mcp-session-id", &b.session)
        .send()
        .await
        .unwrap();
    assert_eq!(closed.status(), 204);
    assert!(
        tab_gone(h.cdp_port, &url_b("/b")).await,
        "the closed session's window stayed open"
    );
    assert!(tabs(h.cdp_port).await.iter().any(|t| t.0 == tab_a2.0));
}

#[tokio::test]
async fn a_desk_box_acts_on_a_user_tab_only_when_a_call_names_it() {
    let Some(chrome) = test_chrome() else {
        return;
    };
    let site = field_pages().await;
    let url = |path: &str| format!("http://{site}{path}");
    let day = Duration::from_secs(24 * 3600);
    let h = Harness::boot(
        Vec::new(),
        false,
        Duration::from_secs(600),
        day,
        Some(&chrome),
        None,
        Automation::Desk,
        false,
    )
    .await;
    let mut mcp = McpClient::connect(&h).await;
    let own = mcp
        .call("browser_navigate", json!({"url": url("/agent")}))
        .await;
    assert_eq!(own["isError"], false, "{own}");
    let user: Value = reqwest::Client::new()
        .put(format!(
            "http://127.0.0.1:{}/json/new?about:blank",
            h.cdp_port
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = user["id"].as_str().unwrap();
    let text = |out: &Value| out["content"][0]["text"].as_str().unwrap().to_owned();

    let nav = mcp
        .call("browser_navigate", json!({"url": url("/user"), "tab": id}))
        .await;
    assert!(
        text(&nav).contains("textbox \"field /user\" [ref=e1]"),
        "{nav}"
    );
    let listed = text(&mcp.call("browser_tabs", json!({})).await);
    assert!(
        listed.contains(&format!("- tab {id} \"/user\" ({})", url("/user"))),
        "{listed}"
    );
    assert!(!listed.contains(&url("/agent")), "{listed}");
    let typed = mcp
        .call(
            "browser_type",
            json!({"ref": "e1", "text": "for the user", "tab": id}),
        )
        .await;
    assert_eq!(typed["isError"], false, "{typed}");
    let value = "document.querySelector('input').value";
    let open = tabs(h.cdp_port).await;
    let ws = |u: String| open.iter().find(|t| t.1 == u).unwrap().2.clone();
    assert_eq!(tab_eval(&ws(url("/user")), value).await, "for the user");
    assert_eq!(tab_eval(&ws(url("/agent")), value).await, "");
    let own = mcp.call("browser_snapshot", json!({})).await;
    assert!(text(&own).contains("field /agent"), "{own}");
}

#[tokio::test]
async fn a_desk_box_waits_for_the_content_a_page_script_loads() {
    let Some(chrome) = test_chrome() else {
        return;
    };
    let page = format!("http://{}/spa", feed_pages().await);
    let day = Duration::from_secs(24 * 3600);
    let plain = Harness::with_chrome(Vec::new(), &chrome).await;
    let desk = Harness::boot(
        Vec::new(),
        false,
        Duration::from_secs(600),
        day,
        Some(&chrome),
        None,
        Automation::Desk,
        false,
    )
    .await;
    let text = |out: &Value| out["content"][0]["text"].as_str().unwrap().to_owned();
    for (h, tool, want) in [
        (&plain, "browser_navigate", "Loading your feed"),
        (&desk, "browser_navigate", "feed item one"),
        (&desk, "browser_fetch", "feed item one"),
    ] {
        let mut mcp = McpClient::connect(h).await;
        let out = text(&mcp.call(tool, json!({"url": page})).await);
        assert!(out.contains(want), "{tool}: {out}");
    }
}

#[tokio::test]
async fn a_share_session_closes_its_chrome_window() {
    let Some(chrome) = test_chrome() else {
        return;
    };
    let site = field_pages().await;
    let page = format!("http://{site}/guest");
    let h = Harness::with_chrome(Vec::new(), &chrome).await;
    let edge = Edge::provision(&h);
    let mut guest = McpClient::connect_as(&h, Some(edge.share("share:sh-1:write"))).await;
    let nav = guest.call("browser_navigate", json!({"url": page})).await;
    assert_eq!(nav["isError"], false, "{nav}");
    assert!(tabs(h.cdp_port).await.iter().any(|t| t.1 == page));
    let closed = guest
        .authorize(reqwest::Client::new().delete(format!("{}/mcp", h.endpoint)))
        .send()
        .await
        .unwrap();
    assert_eq!(closed.status(), 204);
    assert!(
        tab_gone(h.cdp_port, &page).await,
        "the closed share session's window stayed open"
    );
}

#[tokio::test]
async fn delete_closes_the_agents_chrome_window() {
    let Some(chrome) = test_chrome() else {
        return;
    };
    let site = field_pages().await;
    let page = format!("http://{site}/doomed");
    let h = Harness::with_chrome(
        vec![
            (
                "chat/completions",
                chat_tool_call("call_nav", "browser_navigate", &json!({"url": page})),
            ),
            ("chat/completions", chat_text("opened")),
        ],
        &chrome,
    )
    .await;
    let mut agents = h.agents().await;
    let doomed = create_agent(&mut agents).await;
    send(&mut agents, &doomed, "open the page", "").await;
    wait_turn_end(&mut agents, &doomed).await;
    assert!(tabs(h.cdp_port).await.iter().any(|t| t.1 == page));
    agents
        .delete(authed(pb::DeleteAgentRequest { agent_id: doomed }))
        .await
        .unwrap();
    assert!(
        tab_gone(h.cdp_port, &page).await,
        "the deleted agent's window stayed open"
    );
}

#[tokio::test]
async fn launch_app_reuses_a_running_apps_window() {
    let h = Harness::start(Vec::new(), false).await;
    let mut mcp = McpClient::connect(&h).await;
    let running = mcp
        .call(
            "computer_act",
            json!({"action": "launch_app", "app": "mousepad"}),
        )
        .await;
    assert_eq!(running["isError"], false, "{running}");
    assert!(
        running["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("already running"),
        "{running}"
    );
    let raised = driver_calls(&h, "bring_to_front");
    assert_eq!(
        (raised[0]["pid"].clone(), raised[0]["window_id"].clone()),
        (json!(42), json!(7))
    );
    assert!(driver_calls(&h, "launch_app").is_empty());

    let fresh = mcp
        .call(
            "computer_act",
            json!({"action": "launch_app", "app": "gedit"}),
        )
        .await;
    assert_eq!(fresh["isError"], false, "{fresh}");
    assert_eq!(driver_calls(&h, "launch_app")[0]["name"], "gedit");
}

type Objects = Arc<Mutex<HashMap<String, Vec<u8>>>>;

async fn fake_store() -> (String, Objects) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/bucket", listener.local_addr().unwrap());
    let objects = Objects::default();
    let shared = Arc::clone(&objects);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let objects = Arc::clone(&shared);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_owned();
                    let name = parts
                        .next()
                        .unwrap_or_default()
                        .rsplit('/')
                        .next()
                        .unwrap()
                        .to_owned();
                    let mut len = 0;
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).await.unwrap();
                        if h == "\r\n" {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0u8; len];
                    reader.read_exact(&mut body).await.unwrap();
                    let (status, reply) = if method == "PUT" {
                        objects.lock().unwrap().insert(name, body);
                        ("200 OK", Vec::new())
                    } else {
                        match objects.lock().unwrap().get(&name) {
                            Some(b) => ("200 OK", b.clone()),
                            None => ("404 Not Found", Vec::new()),
                        }
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n",
                        reply.len()
                    );
                    let stream = reader.get_mut();
                    stream.write_all(head.as_bytes()).await.unwrap();
                    stream.write_all(&reply).await.unwrap();
                }
            });
        }
    });
    (base, objects)
}

fn state_key() -> pb::StateKey {
    pb::StateKey {
        key: vec![42u8; 32].into(),
        key_id: "k1".into(),
    }
}

async fn state_client(h: &Harness) -> cocoonbox_agent::pb::state_client::StateClient<Channel> {
    cocoonbox_agent::pb::state_client::StateClient::new(h.channel().await)
}

async fn plan(h: &Harness, consistency: pb::Consistency, parent: &str) -> pb::PlanResponse {
    state_client(h)
        .await
        .plan(authed(pb::PlanRequest {
            key: Some(state_key()),
            consistency: consistency.into(),
            parent: parent.into(),
        }))
        .await
        .unwrap()
        .into_inner()
}

async fn upload_missing(
    h: &Harness,
    base: &str,
    objects: &Objects,
    planned: &pb::PlanResponse,
) -> usize {
    let missing: Vec<String> = {
        let have = objects.lock().unwrap();
        planned
            .chunks
            .iter()
            .filter(|c| !have.contains_key(&c.id))
            .map(|c| c.id.clone())
            .collect()
    };
    let put = |id: &str| pb::PresignedPut {
        chunk_id: id.into(),
        url: format!("{base}/{id}"),
        headers: HashMap::from([("x-amz-meta-test".to_owned(), "1".to_owned())]),
    };
    let mut stream = state_client(h)
        .await
        .upload(authed(pb::UploadRequest {
            snapshot_id: planned.snapshot_id.clone(),
            key: Some(state_key()),
            puts: missing.iter().map(|id| put(id)).collect(),
            manifest: Some(put(&planned.snapshot_id)),
        }))
        .await
        .unwrap()
        .into_inner();
    let mut last = None;
    while let Some(p) = stream.message().await.unwrap() {
        last = Some(p);
    }
    let last = last.unwrap();
    assert!(last.finished);
    assert_eq!(last.manifest_id, planned.snapshot_id);
    assert_eq!(last.done_chunks as usize, missing.len() + 1);
    missing.len()
}

async fn restore(
    h: &Harness,
    base: &str,
    objects: &Objects,
    manifest_id: &str,
) -> Result<pb::TransferProgress, tonic::Status> {
    let gets = objects
        .lock()
        .unwrap()
        .keys()
        .filter(|k| *k != manifest_id)
        .map(|k| pb::PresignedGet {
            chunk_id: k.clone(),
            url: format!("{base}/{k}"),
        })
        .collect();
    restore_pages(h, base, manifest_id, gets).await
}

async fn restore_pages(
    h: &Harness,
    base: &str,
    manifest_id: &str,
    gets: Vec<pb::PresignedGet>,
) -> Result<pb::TransferProgress, tonic::Status> {
    let mut reqs = vec![pb::RestoreRequest {
        key: Some(state_key()),
        manifest: Some(pb::PresignedGet {
            chunk_id: manifest_id.into(),
            url: format!("{base}/{manifest_id}"),
        }),
        gets: Vec::new(),
        scope: None,
    }];
    reqs.extend(gets.chunks(2).map(|page| pb::RestoreRequest {
        gets: page.to_vec(),
        ..Default::default()
    }));
    let mut stream = state_client(h)
        .await
        .restore(authed(tokio_stream::iter(reqs)))
        .await?
        .into_inner();
    let mut last = None;
    while let Some(p) = stream.message().await? {
        last = Some(p);
    }
    Ok(last.unwrap())
}

fn manifest_of(objects: &Objects, id: &str) -> Value {
    let keys = cocoonbox_agent::state::crypto::Keys::derive(&state_key().key).unwrap();
    let sealed = objects.lock().unwrap()[id].clone();
    serde_json::from_slice(&keys.open(id, &sealed, 1 << 30).unwrap()).unwrap()
}

fn known_files(h: &Harness) -> Value {
    let path = h
        .home
        .parent()
        .unwrap()
        .join("state/state/known-chunks.json");
    serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap()["files"].take()
}

fn chunks_of(m: &Value, path: &str) -> Value {
    m["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == path)
        .unwrap()["chunks"]
        .clone()
}

fn write_script(path: &Path, body: &str) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[tokio::test]
async fn state_round_trip_is_incremental_and_restores_files_and_agent_db() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    let big = noise(3 << 20, 7);
    std::fs::create_dir_all(h.home.join(".config/app")).unwrap();
    std::fs::create_dir_all(h.home.join(".config/chrome/Default/Code Cache")).unwrap();
    std::fs::create_dir_all(h.home.join(".cache/fontconfig")).unwrap();
    std::fs::create_dir_all(h.home.join("Documents")).unwrap();
    std::fs::write(h.home.join(".config/app/settings.json"), b"{\"a\":1}").unwrap();
    std::fs::write(
        h.home.join(".config/chrome/Default/Code Cache/js"),
        b"cache",
    )
    .unwrap();
    std::fs::write(h.home.join(".config/chrome/SingletonLock"), b"lock").unwrap();
    std::fs::write(h.home.join(".cache/fontconfig/c"), b"junk").unwrap();
    std::fs::write(h.home.join("Documents/big.bin"), &big).unwrap();
    std::os::unix::fs::symlink("Documents/big.bin", h.home.join("big-link")).unwrap();
    std::fs::write(h.workspace.join("notes.txt"), b"first").unwrap();
    let db = h.home.join(".config/app/data.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE t(v); INSERT INTO t VALUES ('kept');",
    )
    .unwrap();
    assert!(h.home.join(".config/app/data.db-wal").exists());
    let mut agents = h.agents().await;
    let keeper = create_agent(&mut agents).await;

    let first = plan(&h, pb::Consistency::Live, "").await;
    assert!(first.files >= 4, "{first:?}");
    assert!(first.chunks.len() >= 4, "{}", first.chunks.len());
    let sent_first = upload_missing(&h, &base, &objects, &first).await;
    assert_eq!(sent_first, first.chunks.len());
    let m = manifest_of(&objects, &first.snapshot_id);
    let paths: Vec<String> = m["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            format!(
                "{}/{}",
                f["root"].as_str().unwrap(),
                f["path"].as_str().unwrap()
            )
        })
        .collect();
    assert!(
        paths.contains(&"home/.config/app/data.db".to_owned()),
        "{paths:?}"
    );
    for gone in [
        "data.db-wal",
        "data.db-shm",
        "Code Cache",
        "SingletonLock",
        ".cache",
    ] {
        assert!(
            !paths.iter().any(|p| p.contains(gone)),
            "{gone} in {paths:?}"
        );
    }
    assert_eq!(m["consistency"], "live");
    assert!(!m["agent_db"]["chunks"].as_array().unwrap().is_empty());

    std::fs::write(h.workspace.join("notes.txt"), b"second").unwrap();
    let second = plan(&h, pb::Consistency::Quiesced, &first.snapshot_id).await;
    let sent_second = upload_missing(&h, &base, &objects, &second).await;
    assert!(sent_second <= 2, "incremental sent {sent_second} chunks");
    assert_eq!(
        manifest_of(&objects, &second.snapshot_id)["consistency"],
        "quiesced"
    );
    assert_eq!(
        manifest_of(&objects, &second.snapshot_id)["parent"],
        first.snapshot_id.as_str()
    );

    drop(conn);
    std::fs::remove_dir_all(h.home.join("Documents")).unwrap();
    std::fs::write(h.home.join("stray.txt"), b"not in the snapshot").unwrap();
    std::fs::write(h.workspace.join("notes.txt"), b"third").unwrap();
    let intruder = agents
        .create(authed(pb::CreateAgentRequest {
            name: "intruder".into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner()
        .id;
    let done = restore(&h, &base, &objects, &second.snapshot_id)
        .await
        .unwrap();
    assert!(done.finished);
    assert_eq!(
        std::fs::read(h.home.join("Documents/big.bin")).unwrap(),
        big
    );
    assert_eq!(
        std::fs::read_link(h.home.join("big-link")).unwrap(),
        PathBuf::from("Documents/big.bin")
    );
    assert_eq!(
        std::fs::read(h.workspace.join("notes.txt")).unwrap(),
        b"second"
    );
    assert!(!h.home.join("stray.txt").exists());
    assert!(
        h.home.join(".cache/fontconfig/c").exists(),
        "excluded top-level dirs are kept"
    );
    let restored = rusqlite::Connection::open(&db).unwrap();
    let v: String = restored
        .query_row("SELECT v FROM t", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, "kept");
    let ids: Vec<String> = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner()
        .agents
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert!(ids.contains(&keeper));
    assert!(
        !ids.contains(&intruder),
        "agent.db was not restored: {ids:?}"
    );
}

#[tokio::test]
async fn a_plan_leaves_out_the_offload_excludes_under_the_workspace_only() {
    let h = Harness::start(Vec::new(), false).await;
    let repo = h.workspace.join("gron");
    std::fs::create_dir_all(repo.join("target/debug")).unwrap();
    std::fs::write(repo.join("main.go"), b"package main").unwrap();
    std::fs::write(repo.join("target/debug/big"), noise(10_000, 61)).unwrap();
    std::fs::write(repo.join("target/x"), b"x").unwrap();
    std::fs::create_dir_all(h.home.join("docs")).unwrap();
    std::fs::write(h.home.join("docs/a.txt"), b"a").unwrap();
    let before = plan(&h, pb::Consistency::Live, "").await;
    let listed = h.home.join(offload::Layout::excludes_rel());
    std::fs::create_dir_all(listed.parent().unwrap()).unwrap();
    std::fs::write(
        &listed,
        format!(
            "{}/\n{}\n{}/../gron\nnot-absolute\n",
            repo.join("target").display(),
            h.home.join("docs").display(),
            h.workspace.display()
        ),
    )
    .unwrap();
    let after = plan(&h, pb::Consistency::Live, "").await;
    assert_eq!(
        after.files,
        before.files - 2 + 1,
        "target's two files leave and the excludes file joins; lines outside /workspace are ignored"
    );
}

#[tokio::test]
async fn an_offload_post_reaches_session_events_without_its_body() {
    let h = Harness::start(Vec::new(), false).await;
    let tmux = h._dir.path().join("fake-tmux");
    write_script(&tmux, "cat >/dev/null; exit 0");
    let layout = offload::Layout {
        state: h.home.join(".local/share/cocoonbox/offload"),
        socket: h._dir.path().join("offload.sock"),
        secrets: h._dir.path().join("secrets"),
        workspace: h.workspace.clone(),
        home: h.home.clone(),
        tmux,
    };
    let mut index = offload::Index::new();
    index.insert(
        "0123456789ab".to_owned(),
        offload::Copy {
            name: "gron".into(),
            dir: h.workspace.clone(),
            offload_id: "a1b2c3d4e5f6".into(),
            unreturned: false,
        },
    );
    std::fs::create_dir_all(&layout.state).unwrap();
    std::fs::write(
        layout.state.join("index.json"),
        serde_json::to_vec(&index).unwrap(),
    )
    .unwrap();
    let listener = UnixListener::bind(&layout.socket).unwrap();
    let inbox = offload::inbox::Inbox::open(layout.clone()).await.unwrap();
    tokio::spawn(inbox.run(listener, tokio_util::sync::CancellationToken::new()));
    let started = offload::request(
        &layout.socket,
        &json!({"op": "send", "repo": "0123456789ab", "cli": "claude",
                "offload_id": "a1b2c3d4e5f6", "body": "go", "start": true}),
    )
    .await
    .unwrap();
    assert_eq!(started["delivery"], "started", "{started}");
    let mut events = h
        .session()
        .await
        .events(authed(pb::EventsRequest { from_offset: 0 }))
        .await
        .unwrap()
        .into_inner();
    for n in 0..40 {
        let body = format!("SECRET-{n}");
        offload::request(
            &layout.socket,
            &json!({"op": "post", "repo": "0123456789ab", "kind": "question", "body": body}),
        )
        .await
        .unwrap();
        let Ok(Some(Ok(ev))) =
            tokio::time::timeout(Duration::from_millis(300), events.next()).await
        else {
            continue;
        };
        let Some(event::Body::Offload(update)) = &ev.body else {
            continue;
        };
        assert_eq!(update.repo, "0123456789ab");
        assert_eq!(update.name, "gron");
        assert_eq!(update.cli, "claude");
        assert_eq!(update.offload_id, "a1b2c3d4e5f6");
        assert_eq!(update.entry, pb::OffloadEntry::Question as i32);
        assert!((1..=n + 1).contains(&update.seq), "{}", update.seq);
        let encoded = prost::Message::encode_to_vec(&ev);
        assert!(!encoded.windows(7).any(|w| w == b"SECRET-"));
        return;
    }
    panic!("no offload event reached Session.Events");
}

#[tokio::test]
async fn quiesced_plan_saves_the_desktop_session_and_survives_a_failed_save() {
    let h = Harness::start(Vec::new(), false).await;
    let log = h.session_save.with_extension("log");
    std::fs::write(h.workspace.join("notes.txt"), b"x").unwrap();

    plan(&h, pb::Consistency::Live, "").await;
    assert!(!log.exists(), "a live plan saved the desktop session");
    plan(&h, pb::Consistency::Quiesced, "").await;
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "saved\n");

    write_script(&h.session_save, "echo no session manager >&2; exit 1");
    let p = plan(&h, pb::Consistency::Quiesced, "").await;
    assert!(p.files >= 1, "{p:?}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "saved\n");
}

#[tokio::test]
async fn an_android_box_fails_a_quiesced_plan_when_its_session_save_fails() {
    let h = Harness::with_android().await;
    write_script(&h.session_save, "echo tar failed >&2; exit 1");
    let err = state_client(&h)
        .await
        .plan(authed(pb::PlanRequest {
            key: Some(state_key()),
            consistency: pb::Consistency::Quiesced.into(),
            parent: String::new(),
        }))
        .await
        .unwrap_err();
    assert!(err.message().contains("tar failed"), "{err:?}");
}

#[tokio::test]
async fn a_restore_tells_the_offload_inbox_to_rescan() {
    let h = Harness::start(Vec::new(), false).await;
    let inbox = UnixListener::bind(h.home.parent().unwrap().join("offload.sock")).unwrap();
    let (base, objects) = fake_store().await;
    std::fs::write(h.workspace.join("a.txt"), b"x").unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;
    restore(&h, &base, &objects, &planned.snapshot_id)
        .await
        .unwrap();
    let rescan = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (conn, _) = inbox.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(conn).read_line(&mut line).await.unwrap();
            if line.contains("rescan") {
                return line;
            }
        }
    })
    .await
    .expect("no rescan reached the inbox");
    assert_eq!(rescan, "{\"op\":\"rescan\"}\n");
}

#[tokio::test]
async fn restore_rejects_tampered_chunks_before_touching_files() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    std::fs::write(h.workspace.join("a.txt"), noise(10_000, 3)).unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;
    std::fs::write(h.workspace.join("a.txt"), b"current").unwrap();
    {
        let mut store = objects.lock().unwrap();
        let victim = store
            .keys()
            .find(|k| **k != planned.snapshot_id)
            .unwrap()
            .clone();
        let body = store.get_mut(&victim).unwrap();
        let last = body.len() - 1;
        body[last] ^= 0x01;
    }
    let err = restore(&h, &base, &objects, &planned.snapshot_id)
        .await
        .unwrap_err();
    assert!(err.message().contains("failed authentication"), "{err:?}");
    assert_eq!(
        std::fs::read(h.workspace.join("a.txt")).unwrap(),
        b"current"
    );

    let wrong_key = state_client(&h)
        .await
        .plan(authed(pb::PlanRequest {
            key: Some(pb::StateKey {
                key: vec![1u8; 8].into(),
                key_id: "short".into(),
            }),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(wrong_key.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn restore_rejects_a_manifest_whose_chunks_never_all_got_urls() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    std::fs::write(h.workspace.join("a.txt"), noise(10_000, 3)).unwrap();
    std::fs::write(h.workspace.join("b.txt"), noise(10_000, 4)).unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;
    std::fs::write(h.workspace.join("a.txt"), b"current").unwrap();
    let withheld = &planned.chunks[0].id;
    let gets = planned
        .chunks
        .iter()
        .filter(|c| c.id != *withheld)
        .map(|c| pb::PresignedGet {
            chunk_id: c.id.clone(),
            url: format!("{base}/{}", c.id),
        })
        .collect();
    let err = restore_pages(&h, &base, &planned.snapshot_id, gets)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Code::InvalidArgument, "{err:?}");
    assert!(
        err.message()
            .contains(&format!("no url for chunk {withheld}")),
        "{err:?}"
    );
    assert_eq!(
        std::fs::read(h.workspace.join("a.txt")).unwrap(),
        b"current"
    );
}

#[tokio::test]
async fn restore_fetches_a_page_before_the_stream_ends() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    std::fs::write(h.workspace.join("a.txt"), noise(10_000, 3)).unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;
    let get = |id: &str| pb::PresignedGet {
        chunk_id: id.into(),
        url: format!("{base}/{id}"),
    };
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    tx.send(pb::RestoreRequest {
        key: Some(state_key()),
        manifest: Some(get(&planned.snapshot_id)),
        gets: vec![get(&planned.chunks[0].id)],
        scope: None,
    })
    .await
    .unwrap();
    let mut stream = state_client(&h)
        .await
        .restore(authed(tokio_stream::wrappers::ReceiverStream::new(rx)))
        .await
        .unwrap()
        .into_inner();
    let early = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(early.done_chunks, 1, "{early:?}");
    assert_eq!(early.total_chunks as usize, planned.chunks.len());
    for c in &planned.chunks[1..] {
        tx.send(pb::RestoreRequest {
            gets: vec![get(&c.id)],
            ..Default::default()
        })
        .await
        .unwrap();
    }
    drop(tx);
    let mut last = early;
    while let Some(p) = stream.message().await.unwrap() {
        last = p;
    }
    assert!(last.finished, "{last:?}");
    assert_eq!(last.manifest_id, planned.snapshot_id);
}

#[tokio::test]
async fn live_snapshot_skips_a_file_that_keeps_changing() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    let growing = h.workspace.join("growing.bin");
    std::fs::write(&growing, noise(16 << 20, 5)).unwrap();
    std::fs::write(h.workspace.join("steady.txt"), b"steady").unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let stop = Arc::clone(&stop);
        let growing = growing.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(growing)
                .unwrap();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                f.write_all(b"x").unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let planned = plan(&h, pb::Consistency::Live, "").await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    upload_missing(&h, &base, &objects, &planned).await;
    let m = manifest_of(&objects, &planned.snapshot_id);
    assert_eq!(m["skipped"], json!(["workspace/growing.bin"]));
    let paths: Vec<&str> = m["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap())
        .collect();
    assert!(
        paths.contains(&"steady.txt") && !paths.contains(&"growing.bin"),
        "{paths:?}"
    );
}

#[tokio::test]
async fn upload_can_span_several_calls_with_the_manifest_last() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    std::fs::write(h.workspace.join("big.bin"), noise(6 << 20, 11)).unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    let ids: Vec<String> = planned.chunks.iter().map(|c| c.id.clone()).collect();
    assert!(ids.len() >= 3, "{}", ids.len());
    let put = |id: &str| pb::PresignedPut {
        chunk_id: id.into(),
        url: format!("{base}/{id}"),
        headers: HashMap::new(),
    };
    let (first, rest) = ids.split_at(ids.len() / 2);
    let call = |puts: Vec<pb::PresignedPut>, manifest: Option<pb::PresignedPut>| {
        let snapshot_id = planned.snapshot_id.clone();
        let h = &h;
        async move {
            let mut stream = state_client(h)
                .await
                .upload(authed(pb::UploadRequest {
                    snapshot_id,
                    key: Some(state_key()),
                    puts,
                    manifest,
                }))
                .await
                .unwrap()
                .into_inner();
            let mut last = None;
            while let Some(p) = stream.message().await.unwrap() {
                last = Some(p);
            }
            last.unwrap()
        }
    };
    let part = call(first.iter().map(|id| put(id)).collect(), None).await;
    assert!(!part.finished);
    assert_eq!(part.done_chunks as usize, first.len());
    let done = call(
        rest.iter().map(|id| put(id)).collect(),
        Some(put(&planned.snapshot_id)),
    )
    .await;
    assert!(done.finished);
    assert_eq!(done.manifest_id, planned.snapshot_id);
    assert_eq!(objects.lock().unwrap().len(), ids.len() + 1);
    let again = state_client(&h)
        .await
        .upload(authed(pb::UploadRequest {
            snapshot_id: planned.snapshot_id.clone(),
            key: Some(state_key()),
            puts: Vec::new(),
            manifest: Some(put(&planned.snapshot_id)),
        }))
        .await
        .unwrap()
        .into_inner()
        .message()
        .await
        .unwrap_err();
    assert_eq!(
        again.code(),
        Code::InvalidArgument,
        "the snapshot is unpinned after the manifest"
    );
}

#[tokio::test]
async fn plan_reuses_unchanged_files_and_rereads_a_rewrite_that_keeps_size_and_mtime() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    let a = h.workspace.join("a.txt");
    std::fs::write(&a, noise(10_000, 1)).unwrap();
    std::fs::write(h.workspace.join("b.txt"), noise(10_000, 2)).unwrap();
    std::fs::write(h.workspace.join("c.txt"), noise(10_000, 3)).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let first = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &first).await;
    let index = known_files(&h);
    for f in ["workspace/a.txt", "workspace/b.txt", "workspace/c.txt"] {
        assert!(index.get(f).is_some(), "{f} not indexed: {index}");
    }
    let m1 = manifest_of(&objects, &first.snapshot_id);
    assert!(
        m1["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f.get("ctime_ns").is_none())
    );

    let mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
    std::fs::write(&a, noise(10_000, 4)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&a)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert_eq!(std::fs::metadata(&a).unwrap().modified().unwrap(), mtime);
    let known = h
        .home
        .parent()
        .unwrap()
        .join("state/state/known-chunks.json");
    let mut forged: Value = serde_json::from_slice(&std::fs::read(&known).unwrap()).unwrap();
    forged["files"]["workspace/c.txt"]["chunks"] =
        forged["files"]["workspace/b.txt"]["chunks"].clone();
    std::fs::write(&known, serde_json::to_vec(&forged).unwrap()).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    std::fs::write(h.workspace.join("fresh.txt"), b"fresh").unwrap();

    let second = plan(&h, pb::Consistency::Quiesced, &first.snapshot_id).await;
    let sent = upload_missing(&h, &base, &objects, &second).await;
    let m2 = manifest_of(&objects, &second.snapshot_id);
    assert_ne!(
        chunks_of(&m2, "a.txt"),
        chunks_of(&m1, "a.txt"),
        "a rewrite with the same size and mtime changes ctime and is reread"
    );
    assert_eq!(chunks_of(&m2, "b.txt"), chunks_of(&m1, "b.txt"));
    assert_eq!(
        chunks_of(&m2, "c.txt"),
        chunks_of(&m1, "b.txt"),
        "missed by design: an unchanged size, mtime and ctime reuses the recorded chunks unread"
    );
    assert!(sent <= 3, "sent {sent} chunks");
    let index = known_files(&h);
    assert!(index.get("workspace/a.txt").is_some(), "{index}");
    assert_eq!(
        index["workspace/fresh.txt"]["compare_only"], true,
        "a file changed within a second of the walk is never reused: {index}"
    );

    let fresh_root = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &fresh_root).await;
    let m3 = manifest_of(&objects, &fresh_root.snapshot_id);
    assert_eq!(
        chunks_of(&m3, "c.txt"),
        chunks_of(&m1, "c.txt"),
        "a plan without a parent reads every file"
    );
    restore(&h, &base, &objects, &second.snapshot_id)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&a).unwrap(), noise(10_000, 4));
}

#[tokio::test]
async fn a_plan_right_after_a_restore_reads_no_file_the_restore_wrote() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    let names = ["a.txt", "b.txt", "c.txt", "d.bin"];
    for (i, name) in names.iter().enumerate() {
        let size = if i == 3 { 3 << 20 } else { 10_000 };
        std::fs::write(h.workspace.join(name), noise(size, 70 + i as u64)).unwrap();
    }
    let db = rusqlite::Connection::open(h.workspace.join("e.db")).unwrap();
    db.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES (1);")
        .unwrap();
    drop(db);
    let first = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &first).await;
    let m1 = manifest_of(&objects, &first.snapshot_id);
    let known = h
        .home
        .parent()
        .unwrap()
        .join("state/state/known-chunks.json");
    std::fs::remove_file(&known).unwrap();
    std::fs::remove_file(h.workspace.join("a.txt")).unwrap();

    restore(&h, &base, &objects, &first.snapshot_id)
        .await
        .unwrap();
    let index = known_files(&h);
    for name in names {
        let f = &index[format!("workspace/{name}")];
        assert_eq!(f["compare_only"], false, "{name}: {index}");
        let sizes: Vec<u64> = f["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c[1].as_u64().unwrap())
            .collect();
        assert_eq!(
            sizes.iter().sum::<u64>(),
            std::fs::metadata(h.workspace.join(name)).unwrap().len(),
            "{name}: {index}"
        );
    }
    assert_eq!(
        index["workspace/e.db"]["compare_only"], true,
        "an SQLite file is always read through its backup: {index}"
    );

    let mut forged: Value = serde_json::from_slice(&std::fs::read(&known).unwrap()).unwrap();
    for (i, name) in names.iter().enumerate() {
        let next = names[(i + 1) % names.len()];
        forged["files"][format!("workspace/{name}")]["chunks"] =
            index[format!("workspace/{next}")]["chunks"].clone();
    }
    std::fs::write(&known, serde_json::to_vec(&forged).unwrap()).unwrap();
    let second = plan(&h, pb::Consistency::Quiesced, &first.snapshot_id).await;
    assert_eq!(second.changed_files, Some(0), "{second:?}");
    let sent = upload_missing(&h, &base, &objects, &second).await;
    assert_eq!(sent, 0, "a plan right after a restore uploads nothing");
    let m2 = manifest_of(&objects, &second.snapshot_id);
    for (i, name) in names.iter().enumerate() {
        let next = names[(i + 1) % names.len()];
        assert_eq!(
            chunks_of(&m2, name),
            chunks_of(&m1, next),
            "{name} was reused unread"
        );
    }
    assert_eq!(chunks_of(&m2, "e.db"), chunks_of(&m1, "e.db"));
}

#[tokio::test]
async fn a_plan_after_a_restore_uploads_exactly_the_files_changed_since() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    for (name, seed) in [("a.txt", 81), ("b.txt", 82), ("c.txt", 83)] {
        std::fs::write(h.workspace.join(name), noise(10_000, seed)).unwrap();
    }
    let first = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &first).await;
    restore(&h, &base, &objects, &first.snapshot_id)
        .await
        .unwrap();

    std::fs::write(h.workspace.join("a.txt"), noise(10_000, 84)).unwrap();
    let b = h.workspace.join("b.txt");
    let mtime = std::fs::metadata(&b).unwrap().modified().unwrap();
    std::fs::write(&b, noise(10_000, 85)).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&b)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let second = plan(&h, pb::Consistency::Quiesced, &first.snapshot_id).await;
    assert_eq!(second.changed_files, Some(2), "{second:?}");
    let missing: HashSet<String> = {
        let have = objects.lock().unwrap();
        second
            .chunks
            .iter()
            .filter(|c| !have.contains_key(&c.id))
            .map(|c| c.id.clone())
            .collect()
    };
    upload_missing(&h, &base, &objects, &second).await;
    let m2 = manifest_of(&objects, &second.snapshot_id);
    let changed: HashSet<String> = ["a.txt", "b.txt"]
        .iter()
        .flat_map(|n| chunks_of(&m2, n).as_array().unwrap().clone())
        .map(|c| c.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(missing, changed);
    assert_eq!(
        chunks_of(&m2, "c.txt"),
        chunks_of(&manifest_of(&objects, &first.snapshot_id), "c.txt")
    );

    restore(&h, &base, &objects, &second.snapshot_id)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(h.workspace.join("a.txt")).unwrap(),
        noise(10_000, 84)
    );
    assert_eq!(std::fs::read(&b).unwrap(), noise(10_000, 85));
}

#[tokio::test]
async fn restore_fetches_a_chunk_shared_by_several_files_once() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    let big = noise(6 << 20, 21);
    let small = noise(5_000, 22);
    std::fs::create_dir_all(h.workspace.join("many")).unwrap();
    for name in ["big1.bin", "big2.bin"] {
        std::fs::write(h.workspace.join(name), &big).unwrap();
    }
    for i in 0..30 {
        std::fs::write(h.workspace.join(format!("many/s{i:02}")), &small).unwrap();
    }
    std::fs::write(h.workspace.join("own.txt"), noise(7_000, 23)).unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;
    let m = manifest_of(&objects, &planned.snapshot_id);
    let mut order: Vec<String> = Vec::new();
    for id in m["files"]
        .as_array()
        .unwrap()
        .iter()
        .chain([&m["agent_db"]])
        .flat_map(|f| f["chunks"].as_array().cloned().unwrap_or_default())
    {
        let id = id.as_str().unwrap().to_owned();
        if !order.contains(&id) {
            order.push(id);
        }
    }
    let planned_order: Vec<String> = planned.chunks.iter().map(|c| c.id.clone()).collect();
    assert_eq!(planned_order, order, "plan lists chunks in manifest order");
    assert!(chunks_of(&m, "big1.bin").as_array().unwrap().len() >= 3);

    std::fs::remove_dir_all(h.workspace.join("many")).unwrap();
    std::fs::write(h.workspace.join("big2.bin"), b"changed").unwrap();
    let done = restore(&h, &base, &objects, &planned.snapshot_id)
        .await
        .unwrap();
    assert!(done.finished);
    assert_eq!(done.total_chunks as usize, planned.chunks.len());
    assert_eq!(
        done.done_chunks, done.total_chunks,
        "every chunk is fetched exactly once"
    );
    for name in ["big1.bin", "big2.bin"] {
        assert_eq!(std::fs::read(h.workspace.join(name)).unwrap(), big);
    }
    for i in 0..30 {
        assert_eq!(
            std::fs::read(h.workspace.join(format!("many/s{i:02}"))).unwrap(),
            small
        );
    }
    assert_eq!(
        std::fs::read(h.workspace.join("own.txt")).unwrap(),
        noise(7_000, 23)
    );
    let work = h.home.parent().unwrap().join("state/state");
    let leftovers: Vec<_> = std::fs::read_dir(&work)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().starts_with("restore-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn mcp_moves_to_a_new_driver_session_when_the_driver_ended_the_old_one() {
    let h = Harness::start(Vec::new(), false).await;
    let mut mcp = McpClient::connect(&h).await;
    let ok = mcp.call("computer_state", json!({})).await;
    assert_eq!(ok["isError"], false, "{ok}");
    let first = driver_calls(&h, "list_windows")[0]["session"].clone();

    let toolhost =
        cocoonbox_agent::toolhost::rpc::Client::new(h.home.parent().unwrap().join("toolhost.sock"));
    let end = cocoonbox_agent::toolhost::rpc::Call {
        agent_id: String::new(),
        tool: "computer_end".into(),
        args: json!({"session": first}),
    };
    assert!(!toolhost.call(&end).await.unwrap().is_error);

    let again = mcp.call("computer_state", json!({})).await;
    assert_eq!(again["isError"], false, "{again}");
    let lists = driver_calls(&h, "list_windows");
    assert_eq!(lists.len(), 3, "{lists:?}");
    assert_eq!(lists[1]["session"], first);
    let second = lists[2]["session"].clone();
    assert_ne!(second, first);
    let ends: Vec<Value> = driver_calls(&h, "end_session")
        .into_iter()
        .filter(|a| a["session"] == first)
        .collect();
    assert_eq!(ends.len(), 2, "{ends:?}");

    let act = mcp
        .call("computer_act", json!({"action": "key", "key": "Return"}))
        .await;
    assert_eq!(act["isError"], false, "{act}");
    let shots = driver_calls(&h, "get_desktop_state");
    assert_eq!(shots.last().unwrap()["session"], second);
}

#[tokio::test]
async fn restore_writes_files_under_a_read_only_directory_and_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    let ro = h.home.join("go/pkg/mod/example.com/m@v1.0.0");
    std::fs::create_dir_all(ro.join("sub")).unwrap();
    std::fs::write(ro.join("go.mod"), b"module example.com/m").unwrap();
    std::fs::write(ro.join("sub/a.go"), noise(5_000, 31)).unwrap();
    let chmod = |p: &Path, mode| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap()
    };
    for d in [ro.join("sub"), ro.clone()] {
        chmod(&d, 0o555);
    }
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;
    for d in [ro.clone(), ro.join("sub")] {
        chmod(&d, 0o755);
    }
    std::fs::remove_dir_all(h.home.join("go")).unwrap();

    let done = restore(&h, &base, &objects, &planned.snapshot_id)
        .await
        .unwrap();
    assert!(done.finished);
    assert_eq!(
        std::fs::read(ro.join("go.mod")).unwrap(),
        b"module example.com/m"
    );
    assert_eq!(
        std::fs::read(ro.join("sub/a.go")).unwrap(),
        noise(5_000, 31)
    );
    for d in [ro.clone(), ro.join("sub")] {
        let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o555, "{}", d.display());
    }
    for d in [ro.clone(), ro.join("sub")] {
        chmod(&d, 0o755);
    }
}

#[tokio::test]
async fn a_failed_plan_removes_its_spool() {
    let h = Harness::start(Vec::new(), false).await;
    std::fs::write(h.workspace.join("a.txt"), noise(10_000, 41)).unwrap();
    let dir = h.home.parent().unwrap().to_path_buf();
    std::fs::remove_file(dir.join("toolhost.sock")).unwrap();
    let err = state_client(&h)
        .await
        .plan(authed(pb::PlanRequest {
            key: Some(state_key()),
            consistency: pb::Consistency::Live.into(),
            parent: String::new(),
        }))
        .await
        .unwrap_err();
    assert!(err.message().contains("toolhost"), "{err:?}");
    let spools: Vec<_> = std::fs::read_dir(dir.join("state/state"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().starts_with("spool-"))
        .collect();
    assert!(spools.is_empty(), "{spools:?}");
}

#[tokio::test]
async fn plan_counts_changed_files_against_the_last_upload() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    for (name, seed) in [("a.txt", 51), ("b.txt", 52), ("c.txt", 53)] {
        std::fs::write(h.workspace.join(name), noise(10_000, seed)).unwrap();
    }
    let db = rusqlite::Connection::open(h.workspace.join("e.db")).unwrap();
    db.execute_batch("CREATE TABLE t(v); INSERT INTO t VALUES (1);")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let first = plan(&h, pb::Consistency::Live, "").await;
    assert_eq!(
        first.changed_files, None,
        "a plan without a parent has no baseline"
    );
    upload_missing(&h, &base, &objects, &first).await;

    std::fs::write(h.workspace.join("a.txt"), noise(10_000, 54)).unwrap();
    std::fs::remove_file(h.workspace.join("b.txt")).unwrap();
    std::fs::write(h.workspace.join("d.txt"), noise(10_000, 55)).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let second = plan(&h, pb::Consistency::Live, &first.snapshot_id).await;
    assert_eq!(
        second.changed_files,
        Some(3),
        "a rewritten, b removed, d new"
    );
    upload_missing(&h, &base, &objects, &second).await;

    let third = plan(&h, pb::Consistency::Live, &second.snapshot_id).await;
    assert_eq!(
        third.changed_files,
        Some(0),
        "the SQLite file is read again through its backup but its chunks did not change"
    );
}

#[tokio::test]
async fn restore_paths_copies_a_subtree_into_restored_and_touches_nothing_else() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    let docs = h.home.join("Documents");
    std::fs::create_dir_all(docs.join("sub")).unwrap();
    std::fs::create_dir_all(h.workspace.join("app")).unwrap();
    std::fs::write(docs.join("a.txt"), b"saved").unwrap();
    std::fs::write(docs.join("sub/b.bin"), noise(300_000, 61)).unwrap();
    std::fs::set_permissions(docs.join("sub/b.bin"), PermissionsExt::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink("a.txt", docs.join("link")).unwrap();
    std::fs::write(h.workspace.join("app/main.go"), b"package main").unwrap();
    std::fs::write(h.workspace.join("app/other.go"), noise(200_000, 62)).unwrap();
    std::fs::set_permissions(docs.join("sub"), PermissionsExt::from_mode(0o555)).unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;

    std::fs::set_permissions(docs.join("sub"), PermissionsExt::from_mode(0o755)).unwrap();
    std::fs::write(docs.join("a.txt"), b"live").unwrap();
    std::fs::write(docs.join("sub/b.bin"), b"live").unwrap();
    std::fs::write(h.workspace.join("app/main.go"), b"package live").unwrap();
    std::fs::write(h.workspace.join("late.txt"), b"after the save").unwrap();
    let mut agents = h.agents().await;
    let late_agent = create_agent(&mut agents).await;
    let before = tree(&[&h.home, &h.workspace]);

    let m = manifest_of(&objects, &planned.snapshot_id);
    let mut needed: Vec<String> = Vec::new();
    for p in ["Documents/a.txt", "Documents/sub/b.bin", "app/main.go"] {
        for id in chunks_of(&m, p).as_array().unwrap() {
            needed.push(id.as_str().unwrap().to_owned());
        }
    }
    let paths = vec![
        format!("{}/Documents/", h.home.display()),
        format!("{}/app/main.go", h.workspace.display()),
    ];
    let done = restore_paths(
        &h,
        &base,
        &planned.snapshot_id,
        &paths,
        "2026-10-02T1608Z",
        &needed,
    )
    .await
    .unwrap();
    assert!(done.finished);
    assert_eq!(done.target, "2026-10-02T1608Z");
    let out = h.home.join("Restored/2026-10-02T1608Z");
    let at = |p: &Path| out.join(p.strip_prefix("/").unwrap());
    let rdocs = at(&docs);
    assert_eq!(std::fs::read(rdocs.join("a.txt")).unwrap(), b"saved");
    assert_eq!(
        std::fs::read(rdocs.join("sub/b.bin")).unwrap(),
        noise(300_000, 61)
    );
    assert_eq!(
        std::fs::read_link(rdocs.join("link")).unwrap(),
        PathBuf::from("a.txt")
    );
    assert_eq!(
        std::fs::metadata(rdocs.join("sub/b.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(rdocs.join("sub"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o555
    );
    assert_eq!(
        std::fs::read(at(&h.workspace).join("app/main.go")).unwrap(),
        b"package main"
    );
    assert!(
        !at(&h.workspace).join("app/other.go").exists(),
        "only the named paths are restored"
    );

    let after = tree(&[&h.home, &h.workspace]);
    let outside: Vec<_> = after
        .iter()
        .filter(|(p, _)| !p.starts_with(h.home.join("Restored")))
        .collect();
    assert_eq!(
        outside,
        before.iter().collect::<Vec<_>>(),
        "live files changed"
    );
    let ids: Vec<String> = agents
        .list(authed(pb::ListAgentsRequest {}))
        .await
        .unwrap()
        .into_inner()
        .agents
        .into_iter()
        .map(|a| a.id)
        .collect();
    assert!(ids.contains(&late_agent), "agent.db was replaced: {ids:?}");

    let again = restore_paths(
        &h,
        &base,
        &planned.snapshot_id,
        &paths[1..],
        "2026-10-02T1608Z",
        &needed,
    )
    .await
    .unwrap();
    assert_eq!(again.target, "2026-10-02T1608Z-2");
    assert_eq!(
        std::fs::read(
            h.home
                .join("Restored/2026-10-02T1608Z-2")
                .join(h.workspace.strip_prefix("/").unwrap())
                .join("app/main.go")
        )
        .unwrap(),
        b"package main"
    );
    let mut names: Vec<String> = std::fs::read_dir(h.home.join("Restored"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["2026-10-02T1608Z", "2026-10-02T1608Z-2"]);
    std::fs::set_permissions(rdocs.join("sub"), PermissionsExt::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn restore_paths_refuses_bad_scopes_before_writing() {
    let h = Harness::start(Vec::new(), false).await;
    let (base, objects) = fake_store().await;
    std::fs::write(h.workspace.join("a.txt"), b"a").unwrap();
    let planned = plan(&h, pb::Consistency::Live, "").await;
    upload_missing(&h, &base, &objects, &planned).await;
    let ws = h.workspace.display().to_string();
    let good = format!("{ws}/a.txt");
    for (paths, target) in [
        (vec![good.clone()], ""),
        (vec![good.clone()], "a/b"),
        (vec![good.clone()], ".."),
        (vec![good.clone()], ".cb-restore-x"),
        (vec![], "t"),
        (vec!["/etc/passwd".to_owned()], "t"),
        (vec![format!("{ws}/../etc")], "t"),
        (vec![format!("{ws}//a.txt")], "t"),
        (vec!["relative".to_owned()], "t"),
        (vec![format!("{ws}/missing")], "t"),
    ] {
        let err = restore_paths(&h, &base, &planned.snapshot_id, &paths, target, &[])
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            Code::InvalidArgument,
            "{paths:?} {target:?}: {err:?}"
        );
    }
    assert!(!h.home.join("Restored").exists());
}

async fn restore_paths(
    h: &Harness,
    base: &str,
    manifest_id: &str,
    paths: &[String],
    target: &str,
    chunks: &[String],
) -> Result<pb::TransferProgress, tonic::Status> {
    let mut reqs = vec![pb::RestoreRequest {
        key: Some(state_key()),
        manifest: Some(pb::PresignedGet {
            chunk_id: manifest_id.into(),
            url: format!("{base}/{manifest_id}"),
        }),
        gets: Vec::new(),
        scope: Some(pb::RestoreScope {
            paths: paths.to_vec(),
            target: target.into(),
        }),
    }];
    reqs.extend(chunks.chunks(2).map(|page| {
        pb::RestoreRequest {
            gets: page
                .iter()
                .map(|id| pb::PresignedGet {
                    chunk_id: id.clone(),
                    url: format!("{base}/{id}"),
                })
                .collect(),
            ..Default::default()
        }
    }));
    let mut stream = state_client(h)
        .await
        .restore_paths(authed(tokio_stream::iter(reqs)))
        .await?
        .into_inner();
    let mut last = None;
    while let Some(p) = stream.message().await? {
        last = Some(p);
    }
    Ok(last.unwrap())
}

/// Every entry under the roots with its kind, mode and content.
fn tree(roots: &[&Path]) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = roots.iter().map(|r| r.to_path_buf()).collect();
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let mode = meta.permissions().mode();
            let what = if meta.is_dir() {
                stack.push(path.clone());
                format!("dir {mode:o}")
            } else if meta.is_symlink() {
                format!("link {:?}", std::fs::read_link(&path).unwrap())
            } else {
                format!(
                    "file {mode:o} {:x}",
                    Sha256::digest(std::fs::read(&path).unwrap())
                )
            };
            out.push((path, what));
        }
    }
    out.sort();
    out
}
