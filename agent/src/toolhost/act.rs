//! `computer_act` on a box that runs shuttle: the toolhost runs a card field typed from a file and Chrome brought up
//! as the agent's window itself, and sends every other action to shuttle's `desk_computer_act`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::browser::Browser;
use super::rpc::{Call, Client, DESK_DEADLINE, Output};

const CARD_FILE_MAX: u64 = 64 * 1024;

/// Runs `computer_act` with `args` for agent `owner` on `shuttle`, the toolhost's own actions in `browser`.
pub async fn run(
    shuttle: &Client,
    browser: &Browser,
    owner: &str,
    args: Value,
) -> Result<Output, String> {
    let Some(actions) = args.get("actions").and_then(Value::as_array) else {
        return one(shuttle, browser, owner, args).await;
    };
    if !actions.iter().any(own) {
        let steps = u32::try_from(actions.len()).unwrap_or(u32::MAX).max(1);
        return call(shuttle, owner, "desk_computer_act", args, steps).await;
    }
    list(shuttle, browser, owner, args).await
}

/// Screen pixels per pixel of the latest screenshot of `session`, as shuttle maps x,y.
pub(super) async fn scale(shuttle: &Client, owner: &str, session: &Value) -> Result<f64, String> {
    let out = call(
        shuttle,
        owner,
        "desk_computer_scale",
        json!({"session": session}),
        1,
    )
    .await?;
    if out.is_error {
        return Err(out.text);
    }
    let reply: Value =
        serde_json::from_str(&out.text).map_err(|e| format!("decode the scale: {e}"))?;
    reply["scale"]
        .as_f64()
        .ok_or_else(|| format!("no scale in {}", out.text))
}

/// Reads `field` of the card in a link-cli `--output-file` JSON file under /tmp or $HOME; no error carries a value.
pub(super) async fn card_field(path: &str, field: &str) -> Result<String, String> {
    let real = tokio::fs::canonicalize(path)
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let allowed = real.starts_with("/tmp") || home.is_some_and(|h| real.starts_with(h));
    if !Path::new(path).is_absolute() || !allowed {
        return Err(format!("{path}: give an absolute path under /tmp or $HOME"));
    }
    let size = tokio::fs::metadata(&real)
        .await
        .map_err(|e| format!("{path}: {e}"))?
        .len();
    if size > CARD_FILE_MAX {
        return Err(format!("{path}: larger than {CARD_FILE_MAX} bytes"));
    }
    let raw = tokio::fs::read(&real)
        .await
        .map_err(|e| format!("{path}: {e}"))?;
    let doc: Value = serde_json::from_slice(&raw)
        .map_err(|_| format!("{path}: not a link-cli card file (no JSON)"))?;
    let card = &doc["card"];
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    };
    let month = || text(&card["exp_month"]).and_then(|m| m.parse::<u32>().ok());
    let value = match field {
        "exp_month" => month().map(|m| format!("{m:02}")),
        "exp" => month()
            .zip(text(&card["exp_year"]).and_then(|y| y.parse::<u32>().ok()))
            .map(|(m, y)| format!("{m:02}/{:02}", y % 100)),
        _ => text(field.split('.').fold(card, |v, k| &v[k])),
    };
    value
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{path}: the card has no {field}"))
}

/// Whether `launch_app` names Chrome or Chromium by command, display name or desktop file id.
pub(super) fn is_chrome(app: &str) -> bool {
    let app = app.to_lowercase();
    app.contains("chrome") || app.contains("chromium")
}

/// Runs the actions in order up to the first failure, each mapped by the screenshot the caller saw, as shuttle runs a list.
async fn list(
    shuttle: &Client,
    browser: &Browser,
    owner: &str,
    mut args: Value,
) -> Result<Output, String> {
    let session = args["session"].take();
    let max_edge = args["max_edge"].take();
    let scale = match args["scale"].as_f64() {
        Some(scale) => scale,
        None => scale(shuttle, owner, &session).await?,
    };
    let Value::Array(actions) = args["actions"].take() else {
        return Err("actions is a list".to_owned());
    };
    let total = actions.len();
    let mut lines = Vec::with_capacity(total);
    let mut out = Output::default();
    let shared = json!({"session": session, "max_edge": max_edge, "scale": scale});
    for (i, mut step) in actions.into_iter().enumerate() {
        carry(&mut step, &shared, &["session", "max_edge", "scale"]);
        let action = step["action"].as_str().unwrap_or_default().to_owned();
        let done = one(shuttle, browser, owner, step)
            .await
            .unwrap_or_else(Output::error);
        let failed = if done.is_error { " failed" } else { "" };
        lines.push(format!("{}. {action}{failed}: {}", i + 1, done.text));
        out.image_b64 = done.image_b64;
        out.secrets.extend(done.secrets);
        if done.is_error {
            out.is_error = true;
            if i + 1 < total {
                lines.push(format!(
                    "the remaining {} actions did not run",
                    total - i - 1
                ));
            }
            break;
        }
    }
    if out.image_b64.is_none() {
        out.image_b64 = look(shuttle, owner, &shared["session"], &shared["max_edge"]).await?;
    }
    out.text = lines.join("\n");
    Ok(out)
}

/// Runs one action: the toolhost's own, else shuttle's.
async fn one(
    shuttle: &Client,
    browser: &Browser,
    owner: &str,
    args: Value,
) -> Result<Output, String> {
    match args["action"].as_str() {
        Some("type_from_file") => type_from_file(shuttle, browser, owner, args).await,
        Some("launch_app") if own(&args) => chrome(shuttle, browser, owner, &args).await,
        _ => call(shuttle, owner, "desk_computer_act", args, 1).await,
    }
}

/// Clicks the target of `args`, types a card field into it while the focus is in a web page field (see
/// [`Browser::page_field`]), returns only its length and hands the field to the agent as a secret.
async fn type_from_file(
    shuttle: &Client,
    browser: &Browser,
    owner: &str,
    args: Value,
) -> Result<Output, String> {
    let field = required(&args, "field")?.to_owned();
    let value = card_field(required(&args, "path")?, &field).await?;
    let mut shot = None;
    if !args["element"].is_null() || (!args["x"].is_null() && !args["y"].is_null()) {
        let mut click = json!({"action": "click"});
        carry(
            &mut click,
            &args,
            &[
                "element",
                "x",
                "y",
                "session",
                "foreground",
                "max_edge",
                "scale",
            ],
        );
        let clicked = call(shuttle, owner, "desk_computer_act", click, 1).await?;
        if clicked.is_error {
            return Ok(Output {
                text: format!(
                    "clicking the {field} field failed; nothing was typed: {}",
                    clicked.text
                ),
                ..clicked
            });
        }
        shot = clicked.image_b64;
    }
    if let Err(e) = browser.page_field().await {
        if shot.is_none() {
            shot = look(shuttle, owner, &args["session"], &args["max_edge"]).await?;
        }
        return Ok(Output {
            text: e,
            is_error: true,
            image_b64: shot,
            ..Output::default()
        });
    }
    let typed = value.chars().count();
    let mut typing = json!({"action": "type", "text": value});
    carry(
        &mut typing,
        &args,
        &["session", "foreground", "max_edge", "scale"],
    );
    let mut out = call(shuttle, owner, "desk_computer_act", typing, 1)
        .await
        .unwrap_or_else(Output::error);
    out.text = if out.is_error {
        format!(
            "typing {field} failed: {}",
            out.text.replace(&value, "[card field]")
        )
    } else {
        format!("typed {typed} characters of {field}; verify on the screenshot")
    };
    out.secrets.push((field, value));
    Ok(out)
}

/// Brings this agent's window of the browser tools' Chrome to the front, so they and `type_from_file` see it.
async fn chrome(
    shuttle: &Client,
    browser: &Browser,
    owner: &str,
    args: &Value,
) -> Result<Output, String> {
    let shown = browser.run(owner, "browser_show", Value::Null).await;
    call(shuttle, owner, "desk_still", json!({}), 1).await?;
    let image_b64 = look(shuttle, owner, &args["session"], &args["max_edge"]).await?;
    Ok(match shown {
        Ok(title) => Output {
            text: format!(
                "brought this agent's window \"{title}\" of the box's Chrome to the front; the browser tools and type_from_file see it"
            ),
            image_b64,
            ..Output::default()
        },
        Err(e) => Output {
            text: format!("opening the box's Chrome failed: {e}"),
            is_error: true,
            image_b64,
            ..Output::default()
        },
    })
}

/// A screenshot of the whole screen in `session`, its long edge at most `max_edge`.
async fn look(
    shuttle: &Client,
    owner: &str,
    session: &Value,
    max_edge: &Value,
) -> Result<Option<String>, String> {
    let out = call(
        shuttle,
        owner,
        "desk_look",
        json!({"session": session, "max_edge": max_edge}),
        1,
    )
    .await?;
    if out.is_error {
        return Err(out.text);
    }
    Ok(out.image_b64)
}

/// Sends `tool` with `args` to shuttle as agent `owner`, waiting for each of `steps` actions as for one desk call.
async fn call(
    shuttle: &Client,
    owner: &str,
    tool: &str,
    args: Value,
    steps: u32,
) -> Result<Output, String> {
    let call = Call {
        agent_id: owner.to_owned(),
        tool: tool.to_owned(),
        args,
    };
    shuttle.desk(&call, DESK_DEADLINE * steps).await
}

/// Whether the toolhost runs action `a` itself: a card field typed from a file, or Chrome by any name.
fn own(a: &Value) -> bool {
    match a["action"].as_str() {
        Some("type_from_file") => true,
        Some("launch_app") => a["app"].as_str().is_some_and(is_chrome),
        _ => false,
    }
}

/// Copies the `keys` that `from` sets to `to`.
fn carry(to: &mut Value, from: &Value, keys: &[&str]) {
    for &key in keys {
        if !from[key].is_null() {
            to[key] = from[key].clone();
        }
    }
}

fn required<'a>(args: &'a Value, name: &str) -> Result<&'a str, String> {
    args[name].as_str().ok_or_else(|| format!("missing {name}"))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::net::UnixListener;

    use super::super::browser::tests::{slow_chrome, web_chrome};
    use super::super::rpc::{read_frame, write_frame};
    use super::*;

    const PAN: &str = "4000056655665556";

    type Calls = Arc<Mutex<Vec<(String, Value)>>>;

    fn card_file(dir: &Path) -> String {
        let path = dir.join("card.json");
        let card = json!({"spend_request_id": "lsrq_1", "card": {"number": PAN, "cvc": "321", "exp_month": 3, "exp_year": 2029, "brand": "visa", "billing_address": {"postal_code": "94107"}}});
        std::fs::write(&path, card.to_string()).unwrap();
        path.display().to_string()
    }

    /// A shuttle on `socket` that logs every call and types the first `typed` texts; acts answer with an image.
    fn fake_shuttle(socket: &Path, typed: usize) -> Calls {
        let listener = UnixListener::bind(socket).unwrap();
        let calls = Calls::default();
        let log = Arc::clone(&calls);
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let call: Call = read_frame(&mut conn).await.unwrap();
                let types = log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(tool, args)| tool == "desk_computer_act" && args["action"] == "type")
                    .count();
                let out = match call.tool.as_str() {
                    "desk_computer_act" if call.args["action"] == "type" && types >= typed => {
                        Output::error(format!("cannot type {}", call.args["text"]))
                    }
                    "desk_computer_act" => Output {
                        image_b64: Some("YWN0".into()),
                        ..Output::text(format!("{} done", call.args["action"]))
                    },
                    "desk_look" => Output {
                        image_b64: Some("bG9vaw==".into()),
                        ..Output::text("screenshot")
                    },
                    "desk_computer_scale" => Output::text(r#"{"scale": 2.0}"#),
                    _ => Output::text("{}"),
                };
                log.lock().unwrap().push((call.tool, call.args));
                write_frame(&mut conn, &out).await.unwrap();
            }
        });
        calls
    }

    fn tools(calls: &Calls) -> Vec<String> {
        calls
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }

    #[tokio::test]
    async fn an_action_or_a_list_without_an_own_step_goes_to_shuttle_as_it_came() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("desk.sock");
        let calls = fake_shuttle(&socket, usize::MAX);
        let shuttle = Client::new(socket);
        let browser = Browser::new("/nonexistent/chrome".into(), dir.path().into(), 1, true);
        let click = json!({"action": "click", "x": 3, "y": 4, "session": "cu-1"});
        let out = run(&shuttle, &browser, "ag", click.clone()).await.unwrap();
        assert_eq!(
            (out.text.as_str(), out.image_b64.as_deref()),
            ("\"click\" done", Some("YWN0"))
        );
        let listed = json!({"actions": [{"action": "key", "key": "ctrl+s"}, {"action": "launch_app", "app": "mousepad"}], "max_edge": 640});
        run(&shuttle, &browser, "ag", listed.clone()).await.unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            [
                ("desk_computer_act".to_owned(), click),
                ("desk_computer_act".to_owned(), listed)
            ]
        );
    }

    #[tokio::test]
    async fn type_from_file_clicks_types_only_into_a_page_field_and_returns_only_its_length() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = card_file(dir.path());
        let socket = dir.path().join("desk.sock");
        let calls = fake_shuttle(&socket, 1);
        let shuttle = Client::new(socket);
        let field = Browser::new(
            "/nonexistent/chrome".into(),
            dir.path().into(),
            web_chrome(&[("page", "https://shop.test/checkout", "field")]).await,
            true,
        );
        let act = json!({"action": "type_from_file", "path": path, "field": "number", "x": 10, "y": 10, "session": "cu-1"});
        let ok = run(&shuttle, &field, "ag", act.clone()).await.unwrap();
        assert!(!ok.is_error, "{}", ok.text);
        assert_eq!(
            ok.text,
            "typed 16 characters of number; verify on the screenshot"
        );
        assert_eq!(ok.secrets, [("number".to_owned(), PAN.to_owned())]);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                (
                    "desk_computer_act".to_owned(),
                    json!({"action": "click", "x": 10, "y": 10, "session": "cu-1"})
                ),
                (
                    "desk_computer_act".to_owned(),
                    json!({"action": "type", "text": PAN, "session": "cu-1"})
                )
            ]
        );
        let failed = run(&shuttle, &field, "ag", act.clone()).await.unwrap();
        assert!(
            failed.is_error && failed.text.contains("cannot type \"[card field]\""),
            "{}",
            failed.text
        );
        assert_eq!(failed.secrets, ok.secrets);

        let omnibox = Browser::new(
            "/nonexistent/chrome".into(),
            dir.path().into(),
            web_chrome(&[("page", "https://shop.test/checkout", "")]).await,
            true,
        );
        let refused = run(&shuttle, &omnibox, "ag", act).await.unwrap();
        assert!(
            refused.is_error
                && refused
                    .text
                    .starts_with("focus a card field on the checkout page first"),
            "{}",
            refused.text
        );
        assert!(refused.secrets.is_empty());
        assert_eq!(refused.image_b64.as_deref(), Some("YWN0"));
        let unreadable = run(
            &shuttle,
            &field,
            "ag",
            json!({"action": "type_from_file", "path": "/etc/passwd", "field": "number"}),
        )
        .await
        .unwrap_err();
        assert!(unreadable.contains("under /tmp or $HOME"), "{unreadable}");
        assert_eq!(tools(&calls).len(), 5, "{:?}", calls.lock().unwrap());
    }

    #[tokio::test]
    async fn chrome_by_any_name_brings_up_the_agent_window_of_the_browser_tools_chrome() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("desk.sock");
        let calls = fake_shuttle(&socket, usize::MAX);
        let shuttle = Client::new(socket);
        let port = slow_chrome(Duration::ZERO).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let browser = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        for app in [
            "google-chrome",
            "Google Chrome",
            "chromium",
            "com.google.Chrome.desktop",
        ] {
            let out = run(
                &shuttle,
                &browser,
                "ag",
                json!({"action": "launch_app", "app": app, "session": "cu-1"}),
            )
            .await
            .unwrap();
            assert!(
                !out.is_error
                    && out
                        .text
                        .contains("window \"Page: slow\" of the box's Chrome")
                    && out.image_b64.as_deref() == Some("bG9vaw=="),
                "{app}: {}",
                out.text
            );
        }
        assert_eq!(tools(&calls), ["desk_still", "desk_look"].repeat(4));
        assert_eq!(calls.lock().unwrap()[1].1["session"], "cu-1");
    }

    #[tokio::test]
    async fn a_list_with_an_own_step_runs_each_step_by_the_scale_the_caller_saw() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = card_file(dir.path());
        let socket = dir.path().join("desk.sock");
        let calls = fake_shuttle(&socket, usize::MAX);
        let shuttle = Client::new(socket);
        let browser = Browser::new(
            "/nonexistent/chrome".into(),
            dir.path().into(),
            web_chrome(&[("page", "https://shop.test/checkout", "field")]).await,
            true,
        );
        let list = json!({"session": "cu-1", "max_edge": 640, "actions": [
            {"action": "click", "x": 5, "y": 6},
            {"action": "type_from_file", "path": path, "field": "cvc"},
            {"action": "key", "key": "return"}
        ]});
        let out = run(&shuttle, &browser, "ag", list).await.unwrap();
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            out.text,
            "1. click: \"click\" done\n2. type_from_file: typed 3 characters of cvc; verify on the screenshot\n3. key: \"key\" done"
        );
        assert_eq!(out.secrets, [("cvc".to_owned(), "321".to_owned())]);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0],
            ("desk_computer_scale".to_owned(), json!({"session": "cu-1"}))
        );
        let acts: Vec<&Value> = calls[1..].iter().map(|(_, a)| a).collect();
        assert_eq!(acts.len(), 3, "{acts:?}");
        for a in acts {
            assert_eq!(
                (&a["session"], &a["max_edge"], &a["scale"]),
                (&json!("cu-1"), &json!(640), &json!(2.0)),
                "{a}"
            );
        }
    }

    #[tokio::test]
    async fn card_field_reads_a_link_cli_card_file_under_tmp_only() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let path = card_file(dir.path());
        for (field, want) in [
            ("number", PAN),
            ("cvc", "321"),
            ("exp_month", "03"),
            ("exp_year", "2029"),
            ("exp", "03/29"),
            ("billing_address.postal_code", "94107"),
        ] {
            assert_eq!(card_field(&path, field).await.unwrap(), want, "{field}");
        }
        let missing = card_field(&path, "billing_address.line1")
            .await
            .unwrap_err();
        assert!(
            missing.contains("no billing_address.line1") && !missing.contains(PAN),
            "{missing}"
        );
        let big = dir.path().join("big.json");
        std::fs::write(&big, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(
            card_field(big.to_str().unwrap(), "number")
                .await
                .unwrap_err()
                .contains("larger than")
        );
        assert!(
            card_field("/etc/passwd", "number")
                .await
                .unwrap_err()
                .contains("under /tmp or $HOME")
        );
        assert!(card_field("../../etc/passwd", "number").await.is_err());
    }
}
