//! The model calls of the desk runtime (`model_decide`, `model_ask`, `model_ground`), made to gw for scripts that hold no key.

use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;
use serde_json::{Value, json};
use shuttle_models::config::Config;

use desk_core::args::parse;

use crate::route::{BUILD, is_build};

// the box's egress proxy sets gw's key on a plain-http request
const KEY: &str = "unprovisioned";

/// gw's models, or why the box has none.
pub(super) struct Models {
    models: Result<shuttle_models::Models, String>,
    warned: AtomicBool,
}

impl Models {
    /// The models behind the gw endpoint `base_url`.
    pub(super) fn new(base_url: Option<&str>) -> Self {
        let models = base_url
            .ok_or_else(|| "no GW_BASE_URL for model calls".to_owned())
            .and_then(|base| {
                let base = base.trim_end_matches('/');
                serde_json::from_value::<Config>(json!({
                    "base_url": base,
                    "decisions_url": format!("{base}/decisions"),
                    "api_key": KEY,
                }))
                .map_err(|e| format!("model config: {e}"))
            })
            .map(shuttle_models::Models::new);
        Self {
            models,
            warned: AtomicBool::new(false),
        }
    }

    /// Runs one `model_*` call; a decision from anything but a dated jev-1.14 build fails.
    pub(super) async fn run(&self, tool: &str, args: Value) -> Result<String, String> {
        let models = self.models.as_ref().map_err(Clone::clone)?;
        match tool {
            "model_decide" => {
                let d = models.decide(parse(args)?).await?;
                let build = d.served.build.as_deref().unwrap_or_default();
                if !is_build(build) {
                    return Err(format!(
                        "gw decisions served {build} for {}",
                        d.served.model
                    ));
                }
                if build != BUILD && !self.warned.swap(true, Ordering::Relaxed) {
                    eprintln!("toolhost: jev answers from build {build}, not the tested {BUILD}");
                }
                encode(&d)
            }
            "model_ask" => encode(&models.ask(parse(args)?).await?),
            "model_ground" => encode(&models.ground(parse(args)?).await?),
            other => Err(format!("unknown model tool {other}")),
        }
    }
}

fn encode(v: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(v).map_err(|e| format!("encode reply: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn gw(model: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let mut buf = vec![0u8; 64 << 10];
                let _ = conn.read(&mut buf).await.unwrap();
                let body = json!({"id": "g1", "model": model, "answers": {"ok": {"type": "noul", "noul": 0.9}},
                    "usage": {"input_tokens": 5, "output_tokens": 1}})
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                conn.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        format!("http://{addr}/v1/")
    }

    fn ask() -> Value {
        json!({"state": {"title": "x"}, "questions": {"ok": {"type": "noul", "instructions": "Is it ok?"}}})
    }

    #[tokio::test]
    async fn a_dated_jev_build_answers_with_its_served_build_and_usage() {
        let m = Models::new(Some(&gw("typesafe/jev-1.14-20270101").await));
        let out: Value =
            serde_json::from_str(&m.run("model_decide", ask()).await.unwrap()).unwrap();
        assert_eq!(out["answers"]["ok"]["probability"], 0.9, "{out}");
        assert_eq!(
            out["served"]["build"], "typesafe/jev-1.14-20270101",
            "{out}"
        );
        assert_eq!(out["served"]["id"], "g1", "{out}");
        assert_eq!(out["usage"]["model"], "typesafe/jev-1.14", "{out}");
        assert_eq!(out["usage"]["input_tokens"], 5, "{out}");
    }

    #[tokio::test]
    async fn an_undated_or_other_jev_build_is_refused() {
        for model in ["typesafe/jev-1.13-20260917", "~typesafe/jev-latest"] {
            let m = Models::new(Some(&gw(model).await));
            let err = m.run("model_decide", ask()).await.unwrap_err();
            assert!(err.contains(model), "{err}");
        }
    }

    #[tokio::test]
    async fn a_box_without_gw_names_the_missing_endpoint() {
        let err = Models::new(None)
            .run("model_ask", json!({"prompt": "p"}))
            .await
            .unwrap_err();
        assert!(err.contains("GW_BASE_URL"), "{err}");
    }
}
