#![cfg(test)]

mod common;

use std::process::Stdio;
use std::time::{Duration, Instant};

use cocoonbox_agent::pb::agents_client::AgentsClient;
use cocoonbox_agent::pb::session_client::SessionClient;
use cocoonbox_agent::pb::{self, event, screen_client_frame, transcript_entry::Body};
use cocoonbox_agent::store::Store;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tonic::Request;
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;

fn authed<T>(msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut().insert(
        "x-cocoonbox-credential",
        MetadataValue::from_static(common::owner()),
    );
    req
}

async fn hanging_gw() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    addr
}

async fn banner_vnc() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            stream.write_all(b"RFB 003.008\n").await.unwrap();
            held.push(stream);
        }
    });
    addr
}

async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn sigterm_with_open_streams_and_a_running_turn_exits_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let identity = dir.path().join("identity").join("identity");
    common::provision(&identity);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_cocoonbox-agent"))
        .arg("serve")
        .env("GW_BASE_URL", format!("http://{}/v1", hanging_gw().await))
        .env("GW_API_KEY", "k")
        .env("MODEL_MAIN", "deepseek/deepseek-v4.1-flash")
        .env("MODEL_COMPUTER", "anthropic/claude-sonnet-5")
        .env("AGENT_LISTEN", format!("127.0.0.1:{port}"))
        .env("CB_STATE_DIR", dir.path())
        .env("CB_IDENTITY_PATH", &identity)
        .env(
            "CB_IDENTITY_OWNER",
            std::os::unix::fs::MetadataExt::uid(&dir.path().metadata().unwrap()).to_string(),
        )
        .env("TOOLHOST_SOCKET", dir.path().join("none.sock"))
        .env("VNC_ADDR", banner_vnc().await)
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let channel = loop {
        if let Ok(ch) = Channel::from_shared(endpoint.clone())
            .unwrap()
            .connect()
            .await
        {
            break ch;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut agents = AgentsClient::new(channel.clone());
    let mut session = SessionClient::new(channel);

    let mut events = session
        .events(authed(pb::EventsRequest { from_offset: 0 }))
        .await
        .unwrap()
        .into_inner();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(pb::ScreenClientFrame {
        body: Some(screen_client_frame::Body::Open(pb::ScreenOpen {})),
    })
    .await
    .unwrap();
    let mut screen = session
        .screen(authed(tokio_stream::wrappers::ReceiverStream::new(rx)))
        .await
        .unwrap()
        .into_inner();
    assert!(screen.message().await.unwrap().is_some());

    let http = reqwest::Client::new();
    let init = http
        .post(format!("{endpoint}/mcp"))
        .header("x-cocoonbox-credential", common::owner())
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"clientInfo": {"name": "t"}}}))
        .send()
        .await
        .unwrap();
    let sid = init.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let mut mcp_stream = http
        .get(format!("{endpoint}/mcp"))
        .header("x-cocoonbox-credential", common::owner())
        .header("mcp-session-id", &sid)
        .send()
        .await
        .unwrap();
    assert!(mcp_stream.chunk().await.unwrap().is_some());

    let agent_id = agents
        .create(authed(pb::CreateAgentRequest::default()))
        .await
        .unwrap()
        .into_inner()
        .id;
    agents
        .send(authed(pb::SendRequest {
            agent_id: agent_id.clone(),
            text: "hang".into(),
            ..Default::default()
        }))
        .await
        .unwrap();
    loop {
        let ev = events.message().await.unwrap().unwrap();
        if let Some(event::Body::Agent(a)) = ev.body
            && a.status == i32::from(pb::AgentStatus::Running)
        {
            break;
        }
    }

    let pid = child.id().unwrap().to_string();
    let started = Instant::now();
    let kill = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(kill.success());
    let status = tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .expect("agent did not exit within 3s of SIGTERM")
        .unwrap();
    let took = started.elapsed();
    assert!(status.success(), "exit status {status}");
    assert!(took < Duration::from_secs(3), "took {took:?}");

    let ended = tokio::time::timeout(Duration::from_secs(1), async {
        while let Ok(Some(_)) = events.message().await {}
    })
    .await;
    assert!(ended.is_ok(), "events stream stayed open");

    let store = Store::open(&dir.path().join("agent.db")).unwrap();
    let page = store.transcript(agent_id, 0, 100).await.unwrap();
    let end = page
        .entries
        .iter()
        .find_map(|e| match &e.body {
            Some(Body::TurnEnd(end)) => Some(end.clone()),
            _ => None,
        })
        .expect("the running turn got no TurnEnd");
    assert!(!end.ok);
    assert_eq!(end.error, "interrupted: the agent is shutting down");
    let agents = store.list_agents().await.unwrap();
    assert!(
        agents.iter().all(|a| !a.name.starts_with("mcp:")),
        "the owner's MCP session made an agent: {agents:?}"
    );
    eprintln!("agent exited {took:?} after SIGTERM");
}
