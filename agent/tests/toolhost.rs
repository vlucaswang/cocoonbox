#![cfg(test)]

use std::path::Path;
use std::time::Duration;

use cocoonbox_agent::toolhost::rpc::{Call, Client, Output, read_frame, write_frame};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

const SCRIPT: &str = r"
const { createConnection } = require('node:net')
const body = Buffer.from(JSON.stringify({ agent_id: 'shell', tool: 'desktop_info', args: {} }))
const head = Buffer.alloc(4)
head.writeUInt32BE(body.length)
const s = createConnection(process.env.TOOLHOST_SOCKET)
let got = Buffer.alloc(0)
s.on('connect', () => s.write(Buffer.concat([head, body])))
s.on('data', (d) => {
  got = Buffer.concat([got, d])
  if (got.length >= 4 && got.length >= 4 + got.readUInt32BE(0)) { s.destroy(); console.log('reply ' + got.subarray(4).toString()) }
})
s.on('error', (e) => console.log('failed ' + e))
";

#[tokio::test]
async fn a_node_script_in_the_agent_shell_reaches_the_toolhost_it_runs_under() {
    let Some(node) = std::env::var_os("NODE_BIN") else {
        eprintln!("SKIP: no Node; set NODE_BIN to run");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (_toolhost, socket) = spawn_toolhost(dir.path(), None).await;
    std::fs::write(dir.path().join("probe.js"), SCRIPT).unwrap();
    let command = format!(
        "\"{}\" {}",
        node.to_string_lossy(),
        dir.path().join("probe.js").display()
    );
    let out = Client::new(socket)
        .call(&Call {
            agent_id: "t".into(),
            tool: "shell".into(),
            args: json!({"command": command}),
        })
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.contains("reply {") && out.text.contains("\"is_error\":false"),
        "{}",
        out.text
    );
}

#[tokio::test]
async fn with_shuttle_the_toolhost_relays_desk_model_and_run_calls_and_shapes_the_runs() {
    let dir = tempfile::tempdir().unwrap();
    let desk = dir.path().join("desk.sock");
    let (seen, mut calls) = mpsc::unbounded_channel::<(Call, bool)>();
    let listener = UnixListener::bind(&desk).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let call: Call = read_frame(&mut conn).await.unwrap();
                let out = match call.tool.as_str() {
                    "desk_windows" => Output {
                        text: "[]".into(),
                        is_error: false,
                        image_b64: Some("aW1n".into()),
                        secrets: vec![("number".into(), "4242".into())],
                    },
                    "script_run" => Output::text(
                        json!({"duration_ms": 3, "logs": "", "stderr": "", "exit_code": 1, "error": {"kind": "exception", "message": "boom"}, "result": null}).to_string(),
                    ),
                    "gui_plan" if call.args["hold"] == true => {
                        let mut probe = [0u8; 1];
                        let closed = conn.read(&mut probe).await.unwrap() == 0;
                        seen.send((call, closed)).unwrap();
                        return;
                    }
                    "gui_plan" => Output::text(r#"{"ok":true,"steps":1}"#),
                    _ => Output::text("{}"),
                };
                seen.send((call, false)).unwrap();
                write_frame(&mut conn, &out).await.unwrap();
            });
        }
    });
    std::fs::write(dir.path().join("s.mts"), "export function main() {}").unwrap();
    let (_toolhost, socket) = spawn_toolhost(dir.path(), Some(&desk)).await;
    let th = Client::new(socket.clone());
    let call = |tool: &str, args: Value| Call {
        agent_id: "ag".into(),
        tool: tool.into(),
        args,
    };

    let out = th.call(&call("desk_windows", json!({}))).await.unwrap();
    assert_eq!(
        (out.text.as_str(), out.image_b64.as_deref(), out.secrets),
        ("[]", Some("aW1n"), vec![("number".into(), "4242".into())])
    );
    let (got, _) = calls.recv().await.unwrap();
    assert_eq!(
        (got.agent_id.as_str(), got.tool.as_str()),
        ("ag", "desk_windows")
    );

    th.call(&call("desk", json!({"op": "click", "x": 1, "y": 2})))
        .await
        .unwrap();
    let (got, _) = calls.recv().await.unwrap();
    assert_eq!(
        (got.tool.as_str(), &got.args["x"]),
        ("desk_click", &json!(1))
    );

    th.call(&call("model_ask", json!({"prompt": "p"})))
        .await
        .unwrap();
    assert_eq!(calls.recv().await.unwrap().0.tool, "model_ask");

    let out = th
        .call(&call(
            "script_run",
            json!({"script": "s.mts", "screenshot": false, "timeout_s": 500}),
        ))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.text);
    assert!(out.text.contains("\n  \"error\": {"), "{}", out.text);
    let (got, _) = calls.recv().await.unwrap();
    assert_eq!(
        got.args["script"],
        dir.path().join("s.mts").to_string_lossy().as_ref()
    );
    assert_eq!(
        (&got.args["timeout_s"], &got.args["params"]),
        (&json!(110), &json!({}))
    );

    let out = th
        .call(&call(
            "gui_plan",
            json!({"steps": [{"app": "x"}], "screenshot": false}),
        ))
        .await
        .unwrap();
    assert_eq!(
        (out.is_error, out.text.as_str()),
        (false, r#"{"ok":true,"steps":1}"#)
    );
    let (got, _) = calls.recv().await.unwrap();
    assert_eq!(got.args, json!({"steps": [{"app": "x"}], "timeout_s": 90}));

    let held = call("gui_plan", json!({"hold": true}));
    let pending = tokio::spawn(async move { Client::new(socket).call(&held).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    pending.abort();
    let (got, closed) = tokio::time::timeout(Duration::from_secs(5), calls.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.tool, "gui_plan");
    assert!(closed, "a dropped call closes its shuttle connection");
}

async fn spawn_toolhost(
    dir: &Path,
    shuttle: Option<&Path>,
) -> (tokio::process::Child, std::path::PathBuf) {
    let socket = dir.join("toolhost.sock");
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_cocoonbox-agent"));
    cmd.arg("toolhost")
        .env("TOOLHOST_SOCKET", &socket)
        .env("TOOLHOST_SOCKET_GROUP", "no-such-group")
        .env("CB_WORKSPACE", dir)
        .env("HOME", dir)
        .kill_on_drop(true);
    if let Some(shuttle) = shuttle {
        cmd.env("SHUTTLE_SOCKET", shuttle);
    }
    let child = cmd.spawn().unwrap();
    for _ in 0..500 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (child, socket)
}

#[tokio::test]
async fn a_failed_run_shuttle_answers_as_an_error_keeps_its_report_and_shape() {
    let dir = tempfile::tempdir().unwrap();
    let desk = dir.path().join("desk.sock");
    let listener = UnixListener::bind(&desk).unwrap();
    tokio::spawn(async move {
        loop {
            let (mut conn, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let call: Call = read_frame(&mut conn).await.unwrap();
                let failed = json!({"duration_ms": 2, "logs": "", "stderr": "", "exit_code": 1, "error": {"kind": "exception", "message": "boom"}, "result": null}).to_string();
                let out = match (
                    call.tool.as_str(),
                    call.args["code"].as_str().or(call.args["case"].as_str()),
                ) {
                    (_, Some("fail")) => Output::error(failed),
                    (_, Some("busy")) => {
                        Output::error("desk busy: another run or action is still going")
                    }
                    (_, Some("null")) => Output::text(
                        json!({"duration_ms": 1, "error": null, "result": 3}).to_string(),
                    ),
                    _ => Output::text("{}"),
                };
                write_frame(&mut conn, &out).await.unwrap();
            });
        }
    });
    let (_toolhost, socket) = spawn_toolhost(dir.path(), Some(&desk)).await;
    let th = Client::new(socket);
    let call = |tool: &str, args: Value| Call {
        agent_id: "ag".into(),
        tool: tool.into(),
        args,
    };

    let out = th
        .call(&call(
            "script_run",
            json!({"code": "fail", "screenshot": false}),
        ))
        .await
        .unwrap();
    assert!(
        out.is_error && out.text.contains("\n  \"error\": {"),
        "{}",
        out.text
    );
    assert!(out.text.contains("\"message\": \"boom\""), "{}", out.text);

    let out = th
        .call(&call(
            "gui_plan",
            json!({"case": "fail", "screenshot": false}),
        ))
        .await
        .unwrap();
    assert!(out.is_error, "{}", out.text);
    let body: Value = serde_json::from_str(&out.text).unwrap();
    assert_eq!(body["error"]["kind"], "exception", "{body}");

    let out = th
        .call(&call("gui_plan", json!({"case": "fail"})))
        .await
        .unwrap();
    assert!(out.is_error && out.image_b64.is_none(), "{}", out.text);
    let body: Value = serde_json::from_str(&out.text).unwrap();
    assert_eq!(body["error"]["kind"], "exception", "{body}");
    assert!(body["screenshot_error"].is_string(), "{body}");

    let out = th
        .call(&call(
            "script_run",
            json!({"code": "busy", "screenshot": false}),
        ))
        .await
        .unwrap();
    assert_eq!(
        (out.is_error, out.text.as_str()),
        (true, "desk busy: another run or action is still going")
    );

    let out = th
        .call(&call(
            "script_run",
            json!({"code": "null", "screenshot": false}),
        ))
        .await
        .unwrap();
    assert!(!out.is_error, "{}", out.text);
}
