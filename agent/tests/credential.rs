#![cfg(test)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cocoonbox_agent::pb::session_client::SessionClient;
use cocoonbox_agent::pb::{self, fill_credential_request::Secret};
use cocoonbox_agent::toolhost::rpc::{Call, Client};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tonic::metadata::MetadataValue;
use tonic::transport::Channel;
use tonic::{Code, Request};

const LOGIN_PAGE: &str = r#"<!doctype html><title>login</title>
<form action="/login" method="post">
<input name="user" autocomplete="username" placeholder="Username">
<input name="pass" type="password" placeholder="Password">
<button>Sign in</button>
</form>"#;
const CODE_PAGE: &str = r#"<!doctype html><title>code</title>
<form action="/code" method="post">
<input name="otp" autocomplete="one-time-code" placeholder="Code">
<button>Verify</button>
</form>"#;

const SEARCH_PAGE: &str = r#"<!doctype html><title>search</title>
<form action="/search" method="post"><input name="q" placeholder="Search"><button>Search</button></form>"#;
const DIALOG_PAGE: &str = r#"<!doctype html><title>dialog</title>
<form><input placeholder="Account"><input type="password" placeholder="Password"></form>
<div id="go" style="cursor:pointer">Log in</div>
<script>
go.onclick = () => {
  const [user, pass] = document.querySelectorAll('input');
  fetch('/dialog', { method: 'POST', body: `user=${user.value}&pass=${pass.value}` });
};
</script>"#;
const CARD_PAGE: &str = r#"<!doctype html><title>checkout</title>
<form>
<input autocomplete="cc-number" placeholder="Number" value="4392027523501133">
<input name="cvc" placeholder="CVC" value="739">
<label>Security code <input value="1234"></label>
<input placeholder="Name on card" value="Ada Lovelace">
<input type="password" name="pin" value="pw-in-the-page">
<input type="button" value="Pay now">
</form>"#;

type Posts = Arc<Mutex<Vec<(String, String)>>>;

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

fn authed<T>(msg: T) -> Request<T> {
    let mut req = Request::new(msg);
    req.metadata_mut().insert(
        "x-cocoonbox-credential",
        MetadataValue::from_static(common::owner()),
    );
    req
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

async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Serves the sign-in pages and records every POST as (path, body).
async fn site(posts: Posts) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let posts = Arc::clone(&posts);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body_at) = loop {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (String::from_utf8_lossy(&buf[..i]).into_owned(), i + 4);
                    }
                };
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .unwrap_or(0);
                while buf.len() < body_at + len {
                    let n = s.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let mut words = head.split_whitespace();
                let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
                let page = match (method, path) {
                    ("POST", _) => {
                        let body = String::from_utf8_lossy(&buf[body_at..]).into_owned();
                        posts.lock().unwrap().push((path.to_owned(), body));
                        "<!doctype html><title>signed in</title>signed in"
                    }
                    (_, "/card") => CARD_PAGE,
                    (_, "/code") => CODE_PAGE,
                    (_, "/dialog") => DIALOG_PAGE,
                    (_, "/search") => SEARCH_PAGE,
                    (_, "/") => LOGIN_PAGE,
                    _ => "",
                };
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    port
}

/// Collects a child's stderr until it exits.
fn capture(child: &mut Child) -> tokio::task::JoinHandle<String> {
    let mut err = child.stderr.take().unwrap();
    tokio::spawn(async move {
        let mut out = Vec::new();
        let _ = err.read_to_end(&mut out).await;
        String::from_utf8_lossy(&out).into_owned()
    })
}

async fn tool(th: &Client, tool: &str, args: serde_json::Value) -> String {
    let out = th
        .call(&Call {
            agent_id: String::new(),
            tool: tool.to_owned(),
            args,
        })
        .await
        .unwrap();
    assert!(!out.is_error, "{tool}: {}", out.text);
    out.text
}

async fn wait_post(posts: &Posts, path: &str) -> String {
    for _ in 0..100 {
        if let Some((_, body)) = posts.lock().unwrap().iter().find(|(p, _)| p == path) {
            return body.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no POST {path} arrived");
}

fn files_containing(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            hits.extend(files_containing(&path, needle));
        } else if needle.is_empty()
            || std::fs::read(&path)
                .unwrap()
                .windows(needle.len())
                .any(|w| w == needle)
        {
            hits.push(path);
        }
    }
    hits
}

#[tokio::test]
async fn fill_credential_signs_in_only_on_the_matching_origin_and_logs_no_value() {
    let Some(chrome) = find_chrome() else {
        assert!(
            cfg!(target_os = "macos") || std::env::var_os("CB_NO_BROWSER").is_some(),
            "no Chrome found: set CHROME_BIN, or CB_NO_BROWSER=1 to skip"
        );
        eprintln!("SKIP: no Chrome; set CHROME_BIN to run");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let wrapper = dir.path().join("chrome");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec \"{}\" --headless=new --use-mock-keychain \"$@\"\n",
            chrome.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("workspace")).unwrap();
    let _chrome = ChromeGuard(dir.path().join("profile"));
    let socket = dir.path().join("toolhost.sock");
    let state = dir.path().join("state");
    let user = format!("user-{}", uuid::Uuid::new_v4().simple());
    let password = format!("pw-{}", uuid::Uuid::new_v4().simple());
    let code = format!("{}", 100_000 + uuid::Uuid::new_v4().as_u128() % 900_000);

    let mut toolhost = Command::new(env!("CARGO_BIN_EXE_cocoonbox-agent"))
        .arg("toolhost")
        .env("HOME", dir.path())
        .env("TOOLHOST_SOCKET", &socket)
        .env("TOOLHOST_SOCKET_GROUP", "no-such-group")
        .env("CB_WORKSPACE", dir.path().join("workspace"))
        .env("CHROME_BIN", &wrapper)
        .env("CHROME_PROFILE", dir.path().join("profile"))
        .env("CDP_PORT", free_port().await.to_string())
        .env("CUA_DRIVER_SOCKET", dir.path().join("cua.sock"))
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let toolhost_log = capture(&mut toolhost);
    let port = free_port().await;
    let identity = dir.path().join("identity").join("identity");
    common::provision(&identity);
    let mut serve = Command::new(env!("CARGO_BIN_EXE_cocoonbox-agent"))
        .arg("serve")
        .env("GW_BASE_URL", "http://127.0.0.1:9/v1")
        .env("GW_API_KEY", "k")
        .env("MODEL_MAIN", "deepseek/deepseek-v4.1-flash")
        .env("MODEL_COMPUTER", "anthropic/claude-sonnet-5")
        .env("AGENT_LISTEN", format!("127.0.0.1:{port}"))
        .env("CB_STATE_DIR", &state)
        .env("CB_IDENTITY_PATH", &identity)
        .env(
            "CB_IDENTITY_OWNER",
            std::os::unix::fs::MetadataExt::uid(&dir.path().metadata().unwrap()).to_string(),
        )
        .env("TOOLHOST_SOCKET", &socket)
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let serve_log = capture(&mut serve);
    let endpoint = format!("http://127.0.0.1:{port}");
    let channel = loop {
        if socket.exists()
            && let Ok(ch) = Channel::from_shared(endpoint.clone())
                .unwrap()
                .connect()
                .await
        {
            break ch;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let mut session = SessionClient::new(channel);
    let th = Client::new(socket.clone());

    let posts = Posts::default();
    let site_port = site(Arc::clone(&posts)).await;
    let origin = format!("http://127.0.0.1:{site_port}");
    tool(
        &th,
        "browser_navigate",
        json!({"url": format!("{origin}/")}),
    )
    .await;
    let active = session
        .active_origin(authed(pb::ActiveOriginRequest {}))
        .await
        .unwrap()
        .into_inner()
        .origin;
    assert_eq!(active, origin);

    let fill = |origin: &str, secret: Secret| pb::FillCredentialRequest {
        origin: origin.to_owned(),
        username: user.clone(),
        secret: Some(secret),
        submit: true,
    };
    let refused = session
        .fill_credential(authed(fill(
            "http://127.0.0.1:1",
            Secret::Password(password.clone()),
        )))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert!(
        refused
            .message()
            .contains(&format!("the active tab is on {origin}")),
        "{}",
        refused.message()
    );
    let snapshot = tool(&th, "browser_snapshot", json!({})).await;
    assert!(
        snapshot.contains("textbox \"Username\" [ref=e1]\n"),
        "{snapshot}"
    );
    assert!(posts.lock().unwrap().is_empty());
    eprintln!(
        "refused: {:?} {}; nothing posted",
        refused.code(),
        refused.message()
    );

    let done = session
        .fill_credential(authed(fill(&origin, Secret::Password(password.clone()))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.filled, ["username", "password"]);
    assert!(done.submitted);
    eprintln!("filled {:?}, submitted {}", done.filled, done.submitted);
    assert_eq!(
        wait_post(&posts, "/login").await,
        format!("user={user}&pass={password}")
    );

    tool(
        &th,
        "browser_navigate",
        json!({"url": format!("{origin}/search")}),
    )
    .await;
    let refused = session
        .fill_credential(authed(fill(&origin, Secret::Password(password.clone()))))
        .await
        .unwrap_err();
    assert!(
        refused.message().contains("no sign-in field on this page"),
        "{}",
        refused.message()
    );

    tool(
        &th,
        "browser_navigate",
        json!({"url": format!("{origin}/dialog")}),
    )
    .await;
    let done = session
        .fill_credential(authed(fill(&origin, Secret::Password(password.clone()))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.filled, ["username", "password"]);
    assert_eq!(
        wait_post(&posts, "/dialog").await,
        format!("user={user}&pass={password}")
    );

    tool(
        &th,
        "browser_navigate",
        json!({"url": format!("{origin}/code")}),
    )
    .await;
    let done = session
        .fill_credential(authed(fill(&origin, Secret::Code(code.clone()))))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(done.filled, ["code"]);
    assert_eq!(wait_post(&posts, "/code").await, format!("otp={code}"));
    eprintln!(
        "the site received all three POSTs with the filled values; filled {:?}",
        done.filled
    );

    tool(&th, "browser_close", json!({})).await;
    for child in [&mut serve, &mut toolhost] {
        child.start_kill().unwrap();
        child.wait().await.unwrap();
    }
    let logs = format!(
        "{}{}",
        toolhost_log.await.unwrap(),
        serve_log.await.unwrap()
    );
    assert!(logs.contains("toolhost listening on"), "{logs}");
    assert!(logs.contains("serve listening on"), "{logs}");
    let control = dir.path().join("control");
    std::fs::create_dir_all(&control).unwrap();
    std::fs::write(control.join("planted"), &password).unwrap();
    assert_eq!(files_containing(&control, password.as_bytes()).len(), 1);
    for value in [&user, &password, &code] {
        assert!(!logs.contains(value.as_str()), "a value is in the logs");
        assert_eq!(
            files_containing(&state, value.as_bytes()),
            Vec::<PathBuf>::new()
        );
    }
    eprintln!(
        "{} bytes of serve and toolhost stderr and {} files under the state dir hold none of the 3 values; the planted control file was found",
        logs.len(),
        files_containing(&state, b"").len()
    );
}

/// Starts a toolhost on a headless Chrome in `dir`; None when no Chrome is found where the test may skip.
async fn chrome_toolhost(dir: &Path) -> Option<(ChromeGuard, Child, Client)> {
    let Some(chrome) = find_chrome() else {
        assert!(
            cfg!(target_os = "macos") || std::env::var_os("CB_NO_BROWSER").is_some(),
            "no Chrome found: set CHROME_BIN, or CB_NO_BROWSER=1 to skip"
        );
        eprintln!("SKIP: no Chrome; set CHROME_BIN to run");
        return None;
    };
    let wrapper = dir.join("chrome");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec \"{}\" --headless=new --use-mock-keychain \"$@\"\n",
            chrome.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &wrapper,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("workspace")).unwrap();
    let guard = ChromeGuard(dir.join("profile"));
    let socket = dir.join("toolhost.sock");
    let toolhost = Command::new(env!("CARGO_BIN_EXE_cocoonbox-agent"))
        .arg("toolhost")
        .env("HOME", dir)
        .env("TOOLHOST_SOCKET", &socket)
        .env("TOOLHOST_SOCKET_GROUP", "no-such-group")
        .env("CB_WORKSPACE", dir.join("workspace"))
        .env("CHROME_BIN", &wrapper)
        .env("CHROME_PROFILE", dir.join("profile"))
        .env("CDP_PORT", free_port().await.to_string())
        .env("CUA_DRIVER_SOCKET", dir.join("cua.sock"))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Some((guard, toolhost, Client::new(socket)))
}

#[tokio::test]
async fn snapshots_mask_the_values_of_card_fields() {
    let dir = tempfile::tempdir().unwrap();
    let Some((_chrome, mut toolhost, th)) = chrome_toolhost(dir.path()).await else {
        return;
    };
    let site_port = site(Posts::default()).await;

    let snapshot = tool(
        &th,
        "browser_navigate",
        json!({"url": format!("http://127.0.0.1:{site_port}/card")}),
    )
    .await;
    let read = tool(&th, "browser_read", json!({})).await;
    eprintln!("{snapshot}\n{read}");
    assert_eq!(
        snapshot.matches("value=\"[masked]\"").count(),
        3,
        "{snapshot}"
    );
    assert!(
        snapshot.contains("textbox \"Name on card\" [ref=e4] value=\"Ada Lovelace\""),
        "{snapshot}"
    );
    let (_, fields) = snapshot.split_once('\n').unwrap();
    for value in ["4392027523501133", "739", "1234"] {
        assert!(!fields.contains(value) && !read.contains(value), "{value}");
    }
    assert!(read.contains("Security code"), "{read}");
    assert!(
        snapshot.contains("textbox \"\" [ref=e5] value=<redacted>"),
        "{snapshot}"
    );

    for (element, label) in [
        ("e1", "Number"),
        ("e2", "CVC"),
        ("e3", "INPUT"),
        ("e4", "Name on card"),
        ("e5", "pin"),
        ("e6", "Pay now"),
    ] {
        let read = tool(&th, "browser_read", json!({"ref": element})).await;
        let clicked = tool(&th, "browser_click", json!({"ref": element})).await;
        eprintln!("{clicked}");
        assert!(
            clicked.starts_with(&format!("clicked {element} \"{label}\";")),
            "{clicked}"
        );
        for value in [
            "4392027523501133",
            "739",
            "1234",
            "Ada Lovelace",
            "pw-in-the-page",
        ] {
            assert!(
                !clicked.contains(value) && !read.contains(value),
                "{value}: {clicked} {read}"
            );
        }
    }

    tool(&th, "browser_close", json!({})).await;
    toolhost.start_kill().unwrap();
    toolhost.wait().await.unwrap();
}

#[tokio::test]
async fn a_click_waits_for_the_page_it_opens_and_not_for_a_click_that_opens_none() {
    let dir = tempfile::tempdir().unwrap();
    let Some((_chrome, mut toolhost, th)) = chrome_toolhost(dir.path()).await else {
        return;
    };
    let posts = Posts::default();
    let origin = format!("http://127.0.0.1:{}", site(Arc::clone(&posts)).await);

    let page = tool(
        &th,
        "browser_navigate",
        json!({"url": format!("{origin}/search")}),
    )
    .await;
    assert!(page.contains("button \"Search\" [ref=e2]"), "{page}");
    let clicked = tool(&th, "browser_click", json!({"ref": "e2"})).await;
    assert!(
        clicked.ends_with(&format!("page is now signed in ({origin}/search)")),
        "{clicked}"
    );
    assert_eq!(wait_post(&posts, "/search").await, "q=");

    tool(
        &th,
        "browser_navigate",
        json!({"url": format!("{origin}/card")}),
    )
    .await;
    let asked = std::time::Instant::now();
    let clicked = tool(&th, "browser_click", json!({"ref": "e6"})).await;
    assert!(
        clicked.ends_with(&format!("page is now checkout ({origin}/card)")),
        "{clicked}"
    );
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "{:?}",
        asked.elapsed()
    );

    tool(&th, "browser_close", json!({})).await;
    toolhost.start_kill().unwrap();
    toolhost.wait().await.unwrap();
}
