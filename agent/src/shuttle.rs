//! serve on a box whose shuttle endpoint it can open: desk calls and runs go straight to shuttle, every other
//! tool to the toolhost.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::dispatch::{ToolOutput, Tools, tool_output};
use crate::runs::{self, Shot, Shots};
use crate::toolhost::rpc::{Call, Client, DESK_DEADLINE, Output};

/// The tools of serve with shuttle beside the toolhost.
pub struct Split {
    shuttle: Client,
    toolhost: Client,
    workspace: PathBuf,
    plugins: Vec<String>,
}

impl Split {
    /// Sends desk calls, `script_run`, `gui_plan` and the `plugins` to `shuttle`, and the rest to `toolhost`.
    pub fn new(
        shuttle: Client,
        toolhost: Client,
        workspace: PathBuf,
        plugins: Vec<String>,
    ) -> Self {
        Self {
            shuttle,
            toolhost,
            workspace,
            plugins,
        }
    }
}

#[tonic::async_trait]
impl Tools for Split {
    async fn run(&self, owner: &str, name: &str, args: Value) -> ToolOutput {
        let shots = ToolhostShots(&self.toolhost);
        let answer = match name {
            n if let Some(tool) = desk_verb(n, &args) => {
                let call = Call {
                    agent_id: owner.to_owned(),
                    tool,
                    args,
                };
                self.shuttle.desk(&call, DESK_DEADLINE).await
            }
            "gui_plan" if args["flows"] != true => Ok(runs::plan(&self.shuttle, &shots, args)
                .await
                .unwrap_or_else(Output::error)),
            n if n == "script_run" || self.plugins.iter().any(|p| p == n) => {
                Ok(
                    runs::script(&self.shuttle, &shots, &self.workspace, name, args)
                        .await
                        .unwrap_or_else(Output::error),
                )
            }
            "screenshot" => return self.toolhost.run(owner, "computer_screenshot", args).await,
            "gui_plan" => {
                let mut out = self.toolhost.run(owner, "gui_flows", json!({})).await;
                if !out.is_error && out.text.trim().is_empty() {
                    "no saved flows yet".clone_into(&mut out.text);
                }
                return out;
            }
            _ => return self.toolhost.run(owner, name, args).await,
        };
        tool_output(answer)
    }
}

/// The screenshots of runs on shuttle, which the toolhost takes and keeps as `user`.
struct ToolhostShots<'a>(&'a Client);

impl Shots for ToolhostShots<'_> {
    async fn shot(&self, max_edge: Option<u32>, keep: bool) -> Result<Option<Shot>, String> {
        let call = Call {
            agent_id: String::new(),
            tool: "run_screenshot".to_owned(),
            args: json!({"max_edge": max_edge, "keep": keep}),
        };
        let out = self.0.call(&call).await?;
        if out.is_error {
            return Err(out.text);
        }
        Ok(out.image_b64.map(|image_b64| Shot {
            image_b64,
            path: (!out.text.is_empty()).then(|| PathBuf::from(out.text)),
        }))
    }
}

/// The desk verb shuttle answers for tool `name`: the verb itself, or `desk_<op>` for the `desk` tool.
pub fn desk_verb(name: &str, args: &Value) -> Option<String> {
    if name == "desk" {
        Some(format!("desk_{}", args["op"].as_str().unwrap_or_default()))
    } else {
        name.starts_with("desk_").then(|| name.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::UnixListener;
    use tokio::sync::mpsc;

    use super::*;
    use crate::toolhost::rpc::{read_frame, write_frame};

    fn fake(path: &std::path::Path, answer: fn(&Call) -> Output) -> mpsc::UnboundedReceiver<Call> {
        let listener = UnixListener::bind(path).unwrap();
        let (seen, calls) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                let call: Call = read_frame(&mut conn).await.unwrap();
                write_frame(&mut conn, &answer(&call)).await.unwrap();
                seen.send(call).unwrap();
            }
        });
        calls
    }

    #[tokio::test]
    async fn desk_calls_and_runs_go_to_shuttle_and_the_rest_and_run_screenshots_to_the_toolhost() {
        let dir = tempfile::tempdir().unwrap();
        let mut shuttle = fake(&dir.path().join("desk.sock"), |c| match c.tool.as_str() {
            "script_run" => Output::text(r#"{"duration_ms":1,"result":7}"#),
            _ => Output::text("{}"),
        });
        let mut toolhost = fake(&dir.path().join("toolhost.sock"), |c| {
            match c.tool.as_str() {
                "run_screenshot" => Output {
                    text: "/tmp/shot.webp".into(),
                    image_b64: Some("aW1n".into()),
                    ..Output::default()
                },
                _ => Output::text("from the toolhost"),
            }
        });
        let split = Split::new(
            Client::new(dir.path().join("desk.sock")),
            Client::new(dir.path().join("toolhost.sock")),
            "/w".into(),
            vec!["demo_run".into()],
        );

        split.run("ag", "desk", json!({"op": "windows"})).await;
        let got = shuttle.recv().await.unwrap();
        assert_eq!(
            (got.agent_id.as_str(), got.tool.as_str()),
            ("ag", "desk_windows")
        );

        let out = split
            .run("ag", "script_run", json!({"script": "s.mts"}))
            .await;
        assert!(
            out.is_error && out.text.contains("/w/s.mts does not exist"),
            "{}",
            out.text
        );

        let out = split.run("ag", "demo_run", json!({"code": "x"})).await;
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("\"screenshot_path\": \"/tmp/shot.webp\""),
            "{}",
            out.text
        );
        assert_eq!(out.image.as_deref(), Some(&b"img"[..]));
        assert_eq!(shuttle.recv().await.unwrap().tool, "demo_run");
        let shot = toolhost.recv().await.unwrap();
        assert_eq!(
            (shot.tool.as_str(), &shot.args["keep"]),
            ("run_screenshot", &json!(true))
        );

        for (name, args, sent) in [
            ("read_file", json!({"path": "a"}), "read_file"),
            ("gui_plan", json!({"flows": true}), "gui_flows"),
            (
                "screenshot",
                json!({"max_edge": 512}),
                "computer_screenshot",
            ),
        ] {
            let out = split.run("ag", name, args).await;
            assert_eq!(out.text, "from the toolhost");
            assert_eq!(toolhost.recv().await.unwrap().tool, sent);
        }
    }
}
