//! Flow library sync of a box that runs GUI plans: the owner's saved flows reach every such box of the owner through the box door.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::dispatch::Tools;
use crate::store::Store;
use crate::toolhost::flows::{self, digest};
use crate::{lock, new_id};

const DOOR_PATH: &str = "/flows";
const PULL_BUDGET: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PUSH_DELAY: Duration = Duration::from_millis(500);
const WATCH_RETRY: Duration = Duration::from_secs(60);
const FLOW_OWNER: &str = "flows";

/// Keeps `/workspace/flows` and the owner's library in step.
pub struct Syncer {
    url: String,
    http: reqwest::Client,
    store: Store,
    tools: Arc<dyn Tools>,
    kick: Notify,
    gate: tokio::sync::Mutex<()>,
    stale_index: AtomicBool,
    /// Content the library refused, by name and SHA-256, so it is not sent again until it changes.
    refused: Mutex<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct Library {
    flows: Vec<Remote>,
}

#[derive(Deserialize)]
struct Remote {
    name: String,
    rev: u64,
    sha256: String,
}

impl Syncer {
    /// Syncs through the box door that serves `connect_url`, keeping its rows in `store`.
    pub fn new(connect_url: &str, store: Store, tools: Arc<dyn Tools>) -> Result<Self, String> {
        let mut url =
            reqwest::Url::parse(connect_url).map_err(|e| format!("parse {connect_url}: {e}"))?;
        url.set_path(DOOR_PATH);
        let http = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("build flows client: {e}"))?;
        Ok(Self {
            url: url.to_string(),
            http,
            store,
            tools,
            kick: Notify::new(),
            gate: tokio::sync::Mutex::new(()),
            stale_index: AtomicBool::new(true),
            refused: Mutex::default(),
        })
    }

    /// Pulls and pushes once, then pushes after every kick and every flow write the toolhost sees until `stop`.
    pub fn spawn(self: &Arc<Self>, stop: CancellationToken) {
        let sync = Arc::clone(self);
        // detached: ends with stop
        tokio::spawn(async move {
            sync.pull().await;
            let mut seen = 0;
            loop {
                if let Err(e) = sync.push().await {
                    eprintln!("flows: push: {e}");
                }
                let written = tokio::select! {
                    () = stop.cancelled() => return,
                    () = sync.kick.notified() => Ok(seen),
                    w = sync.written(seen) => w,
                };
                match written {
                    Ok(n) => seen = n,
                    Err(e) => {
                        eprintln!("flows: watch: {e}");
                        tokio::select! {
                            () = stop.cancelled() => return,
                            () = sync.kick.notified() => {}
                            () = tokio::time::sleep(WATCH_RETRY) => {}
                        }
                    }
                }
                tokio::time::sleep(PUSH_DELAY).await;
            }
        });
    }

    /// Brings the library's newer flows into the box, and every flow it lacks once its flows folder is gone or replaced, within a bounded time; a failure leaves the box's flows as they are, and the index follows every flow a pull wrote, retried by the next pull until it succeeds.
    pub async fn pull(&self) {
        match tokio::time::timeout(PULL_BUDGET, self.try_pull()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("flows: pull: {e}"),
            Err(_) => eprintln!("flows: pull: no answer in {PULL_BUDGET:?}"),
        }
        let _gate = self.gate.lock().await;
        if self.stale_index.load(Ordering::SeqCst) {
            match self.run("flows_index", json!({})).await {
                Ok(_) => self.stale_index.store(false, Ordering::SeqCst),
                Err(e) => eprintln!("flows: index: {e}"),
            }
        }
    }

    /// Asks for a push of the flows changed since the last one.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    async fn try_pull(&self) -> Result<(), String> {
        let _gate = self.gate.lock().await;
        let library: Library = self
            .get(&self.url)
            .await?
            .json()
            .await
            .map_err(|e| format!("decode the library: {e}"))?;
        let local = self.scan().await?;
        let rows = self.store.flow_rows().await?;
        let folder = self.mark().await?;
        let known = self.store.flow_folder().await?;
        let replaced = known.as_ref().map_or(folder.is_empty(), |id| *id != folder);
        for remote in &library.flows {
            if !flows::valid(&remote.name) {
                continue;
            }
            let row = rows.get(&remote.name);
            if !replaced && row.is_some_and(|(rev, _)| *rev >= remote.rev) {
                continue;
            }
            match local.get(&remote.name) {
                Some(sha) if *sha == remote.sha256 => {}
                Some(sha) if row.is_none_or(|(_, synced)| synced != sha) => continue,
                _ => {
                    let content = self.fetch(remote).await?;
                    if remote.name.ends_with(".json") {
                        self.stale_index.store(true, Ordering::SeqCst);
                    }
                    self.run(
                        "flows_write",
                        json!({"name": remote.name, "content": content}),
                    )
                    .await?;
                }
            }
            self.store
                .set_flow_row(remote.name.clone(), remote.rev, remote.sha256.clone())
                .await?;
        }
        let held: HashSet<&str> = library.flows.iter().map(|r| r.name.as_str()).collect();
        for (name, (_, synced)) in rows.iter().filter(|(n, _)| !held.contains(n.as_str())) {
            if local.get(name) == Some(synced) {
                if name.ends_with(".json") {
                    self.stale_index.store(true, Ordering::SeqCst);
                }
                self.run("flows_remove", json!({"name": name})).await?;
            }
            self.store.drop_flow_row(name.clone()).await?;
        }
        let folder = if folder.is_empty() {
            self.mark().await?
        } else {
            folder
        };
        if !folder.is_empty() && known.as_ref() != Some(&folder) {
            self.store.set_flow_folder(folder).await?;
        }
        Ok(())
    }

    async fn push(&self) -> Result<(), String> {
        let _gate = self.gate.lock().await;
        let local = self.scan().await?;
        let rows = self.store.flow_rows().await?;
        for (name, sha) in local {
            let refused = lock(&self.refused).get(&name) == Some(&sha);
            if refused || rows.get(&name).is_some_and(|(_, synced)| *synced == sha) {
                continue;
            }
            let content = self.run("flows_read", json!({"name": name})).await?;
            let sent = digest(content.as_bytes());
            let resp = self
                .http
                .put(format!("{}/{name}", self.url))
                .body(content)
                .send()
                .await
                .map_err(|e| format!("put {name}: {e}"))?;
            let status = resp.status();
            if status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE {
                let why = resp.text().await.unwrap_or_default();
                eprintln!("flows: the library refused {name}: {status} {}", why.trim());
                lock(&self.refused).insert(name, sent);
                continue;
            }
            if !status.is_success() {
                return Err(format!("put {name}: {status}"));
            }
            let stored: Remote = resp
                .json()
                .await
                .map_err(|e| format!("decode put {name}: {e}"))?;
            self.store.set_flow_row(name, stored.rev, sent).await?;
        }
        Ok(())
    }

    async fn fetch(&self, remote: &Remote) -> Result<String, String> {
        let url = format!("{}/{}", self.url, remote.name);
        let content = self
            .get(&url)
            .await?
            .text()
            .await
            .map_err(|e| format!("read {}: {e}", remote.name))?;
        if digest(content.as_bytes()) != remote.sha256 {
            return Err(format!("{} changed while it was read", remote.name));
        }
        Ok(content)
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, String> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("get {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("get {url}: {}", resp.status()));
        }
        Ok(resp)
    }

    async fn mark(&self) -> Result<String, String> {
        self.run("flows_mark", json!({"id": new_id("flows")})).await
    }

    async fn written(&self, after: u64) -> Result<u64, String> {
        let n = self.run("flows_wait", json!({"after": after})).await?;
        n.parse().map_err(|e| format!("decode flows_wait: {e}"))
    }

    async fn scan(&self) -> Result<HashMap<String, String>, String> {
        let text = self.run("flows_scan", json!({})).await?;
        serde_json::from_str(&text).map_err(|e| format!("decode scan: {e}"))
    }

    async fn run(&self, tool: &str, args: Value) -> Result<String, String> {
        let out = self.tools.run(FLOW_OWNER, tool, args).await;
        if out.is_error {
            return Err(format!("{tool}: {}", out.text));
        }
        Ok(out.text)
    }
}

/// Whether call `name` reads saved flows, so the library is pulled before it runs.
pub fn reads(name: &str, args: &Value) -> bool {
    match name {
        "gui_plan" => ["flow", "flows", "find"]
            .iter()
            .any(|k| !args[*k].is_null()),
        "gui_flows" => true,
        "script_run" => runs_flow(args),
        _ => false,
    }
}

/// Whether call `name` may write a flow file, so the changed flows are pushed after it.
pub fn writes(name: &str) -> bool {
    matches!(
        name,
        "gui_plan" | "script_run" | "write_file" | "edit_file" | "shell" | "upload"
    )
}

/// Whether a script call's `script` is a saved flow under `flows/`.
pub fn runs_flow(args: &Value) -> bool {
    args["script"]
        .as_str()
        .is_some_and(|s| s.trim_start_matches("/workspace/").starts_with("flows/"))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::dispatch::ToolOutput;
    use crate::toolhost;

    #[test]
    fn reads_and_writes_name_the_calls_around_saved_flows() {
        assert!(reads("gui_plan", &json!({"flow": "x"})));
        assert!(reads("gui_plan", &json!({"flows": true})));
        assert!(reads("gui_plan", &json!({"find": "save a note"})));
        assert!(!reads("gui_plan", &json!({"steps": []})));
        assert!(reads("gui_flows", &json!({})));
        assert!(reads(
            "script_run",
            &json!({"script": "/workspace/flows/a.mts"})
        ));
        assert!(reads("script_run", &json!({"script": "flows/a.mts"})));
        assert!(!reads("script_run", &json!({"code": "x"})));
        assert!(
            writes("gui_plan") && writes("script_run") && writes("write_file") && writes("shell")
        );
        assert!(!writes("screenshot") && !writes("read_file"));
    }

    #[tokio::test]
    async fn a_new_box_pulls_the_library_and_pushes_only_its_own_changes() {
        let door = Door::start().await;
        door.put("save.json", "{\"v\":1}");
        door.put("note.mts", "export function main() {}");
        let b = Box_::new(&door);
        b.sync.pull().await;
        assert_eq!(b.file("save.json").as_deref(), Some("{\"v\":1}"));
        assert_eq!(
            b.file("note.mts").as_deref(),
            Some("export function main() {}")
        );
        assert_eq!(b.indexed(), 1);
        b.sync.push().await.unwrap();
        assert_eq!(door.puts(), 0, "pulled flows are not pushed back");

        b.write("mine.json", "{\"mine\":1}");
        b.sync.push().await.unwrap();
        assert_eq!(door.content("mine.json").as_deref(), Some("{\"mine\":1}"));
        b.sync.push().await.unwrap();
        b.sync.pull().await;
        assert_eq!(door.puts(), 1);
        assert_eq!(b.indexed(), 1);
    }

    #[tokio::test]
    async fn a_newer_library_flow_replaces_an_unchanged_copy_and_loses_to_a_changed_one() {
        let door = Door::start().await;
        door.put("a.json", "{\"v\":1}");
        door.put("b.json", "{\"v\":1}");
        let b = Box_::new(&door);
        b.sync.pull().await;
        door.put("a.json", "{\"v\":2}");
        door.put("b.json", "{\"v\":2}");
        b.write("b.json", "{\"local\":true}");
        b.sync.pull().await;
        assert_eq!(b.file("a.json").as_deref(), Some("{\"v\":2}"));
        assert_eq!(b.file("b.json").as_deref(), Some("{\"local\":true}"));
        b.sync.push().await.unwrap();
        assert_eq!(door.content("b.json").as_deref(), Some("{\"local\":true}"));
    }

    #[tokio::test]
    async fn library_deletes_reach_unchanged_copies_and_box_deletes_stay_local() {
        let door = Door::start().await;
        for name in ["gone.json", "edited.json", "local.json"] {
            door.put(name, "{}");
        }
        let b = Box_::new(&door);
        b.sync.pull().await;
        door.delete("gone.json");
        door.delete("edited.json");
        b.write("edited.json", "{\"edited\":1}");
        std::fs::remove_file(b.ws.join("flows/local.json")).unwrap();
        b.sync.pull().await;
        assert_eq!(b.file("gone.json"), None);
        assert_eq!(b.file("local.json"), None, "a box delete is not undone");
        assert_eq!(door.content("local.json").as_deref(), Some("{}"));
        b.sync.push().await.unwrap();
        assert_eq!(
            door.content("edited.json").as_deref(),
            Some("{\"edited\":1}")
        );
        door.put("local.json", "{\"v\":2}");
        b.sync.pull().await;
        assert_eq!(b.file("local.json").as_deref(), Some("{\"v\":2}"));
    }

    #[tokio::test]
    async fn a_removed_folder_pulls_the_library_again_and_keeps_new_flows() {
        let door = Door::start().await;
        let names = ["a.json", "b.json", "c.mts", "d.json"];
        for name in names {
            door.put(name, "{}");
        }
        let b = Box_::new(&door);
        b.sync.pull().await;
        std::fs::remove_dir_all(b.ws.join("flows")).unwrap();
        b.sync.pull().await;
        for name in names {
            assert_eq!(b.file(name).as_deref(), Some("{}"), "{name}");
        }

        std::fs::remove_dir_all(b.ws.join("flows")).unwrap();
        b.write("new.json", "{\"new\":1}");
        b.sync.push().await.unwrap();
        b.sync.pull().await;
        for name in names {
            assert_eq!(b.file(name).as_deref(), Some("{}"), "{name}");
        }
        assert_eq!(b.file("new.json").as_deref(), Some("{\"new\":1}"));
        assert_eq!(door.content("new.json").as_deref(), Some("{\"new\":1}"));

        std::fs::remove_file(b.ws.join("flows/a.json")).unwrap();
        b.sync.pull().await;
        assert_eq!(b.file("a.json"), None, "a box delete is not undone");

        let c = Box_::new(&door);
        let rev = lock(&door.state).flows["b.json"].0;
        c.sync
            .store
            .set_flow_row("b.json".to_owned(), rev, digest(b"{}"))
            .await
            .unwrap();
        c.sync.pull().await;
        assert_eq!(c.file("b.json").as_deref(), Some("{}"), "lost before marks");
    }

    #[tokio::test]
    async fn box_deletes_stay_local_in_the_same_folder_a_restored_copy_and_an_unmarked_one() {
        let door = Door::start().await;
        door.put("a.json", "{}");
        door.put("b.mts", "x");
        let b = Box_::new(&door);
        b.sync.pull().await;
        std::fs::remove_file(b.ws.join("flows/a.json")).unwrap();
        std::fs::remove_file(b.ws.join("flows/b.mts")).unwrap();
        b.sync.pull().await;
        assert_eq!((b.file("a.json"), b.file("b.mts")), (None, None));

        let old = b.ws.join("old");
        std::fs::rename(b.ws.join("flows"), &old).unwrap();
        std::fs::create_dir(b.ws.join("flows")).unwrap();
        for entry in std::fs::read_dir(&old).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), b.ws.join("flows").join(entry.file_name())).unwrap();
        }
        b.sync.pull().await;
        assert_eq!((b.file("a.json"), b.file("b.mts")), (None, None));

        let c = Box_::new(&door);
        c.write("c.json", "{}");
        let rev = lock(&door.state).flows["a.json"].0;
        c.sync
            .store
            .set_flow_row("a.json".to_owned(), rev, digest(b"{}"))
            .await
            .unwrap();
        c.sync.pull().await;
        assert_eq!(c.file("a.json"), None);
        assert_eq!(c.file("b.mts").as_deref(), Some("x"));
    }

    #[tokio::test]
    async fn content_the_library_refuses_is_not_sent_again_until_it_changes() {
        let door = Door::start().await;
        let b = Box_::new(&door);
        b.write("big.json", "{\"too\":\"big\"}");
        lock(&door.state).refuse = true;
        b.sync.push().await.unwrap();
        b.sync.push().await.unwrap();
        assert_eq!(door.puts(), 1);
        lock(&door.state).refuse = false;
        b.write("big.json", "{}");
        b.sync.push().await.unwrap();
        assert_eq!(door.content("big.json").as_deref(), Some("{}"));
    }

    #[tokio::test]
    async fn a_flow_written_by_no_tool_is_pushed_within_a_second() {
        let door = Door::start().await;
        let b = Box_::new(&door);
        let stop = CancellationToken::new();
        b.sync.spawn(stop.clone());
        tokio::time::sleep(Duration::from_millis(300)).await;
        b.write("shell.json", "{\"by\":\"shell\"}");
        let started = std::time::Instant::now();
        while door.content("shell.json").is_none() && started.elapsed() < Duration::from_secs(2) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.cancel();
        assert_eq!(
            door.content("shell.json").as_deref(),
            Some("{\"by\":\"shell\"}"),
            "not pushed within {:?}",
            started.elapsed()
        );
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_pull_past_its_budget_after_a_flow_landed_still_rebuilds_the_index() {
        let door = Door::start().await;
        door.put("save.json", "{\"v\":1}");
        let b = Box_::new(&door);
        b.tools.stall_write.store(true, Ordering::SeqCst);
        b.sync.pull().await;
        assert_eq!(b.file("save.json").as_deref(), Some("{\"v\":1}"));
        assert_eq!(b.indexed(), 1);
    }

    #[tokio::test]
    async fn a_pull_cancelled_while_it_indexes_leaves_the_index_to_the_next_pull() {
        let door = Door::start().await;
        door.put("save.json", "{\"v\":1}");
        let b = Box_::new(&door);
        b.tools.stall_index.store(true, Ordering::SeqCst);
        tokio::select! {
            () = b.sync.pull() => panic!("the pull did not stall in flows_index"),
            () = b.tools.index_started.notified() => {}
        }
        assert_eq!(b.file("save.json").as_deref(), Some("{\"v\":1}"));
        assert_eq!(b.indexed(), 0);
        b.sync.pull().await;
        assert_eq!(b.indexed(), 1);
        b.sync.pull().await;
        assert_eq!(b.indexed(), 1);
    }

    #[tokio::test]
    async fn the_first_pull_after_a_start_reindexes_once_with_nothing_to_write() {
        let door = Door::start().await;
        let b = Box_::new(&door);
        b.sync.pull().await;
        b.sync.pull().await;
        assert_eq!(b.indexed(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_library_leaves_the_box_as_it_is() {
        let b = Box_::at("http://127.0.0.1:9/mcp");
        b.write("a.json", "{}");
        b.sync.pull().await;
        assert!(b.sync.push().await.is_err());
        assert_eq!(b.file("a.json").as_deref(), Some("{}"));
    }

    struct Box_ {
        _dir: tempfile::TempDir,
        ws: PathBuf,
        tools: Arc<FileTools>,
        sync: Arc<Syncer>,
    }

    impl Box_ {
        fn new(door: &Door) -> Self {
            Self::at(&format!("http://{}/mcp", door.addr))
        }

        fn at(url: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let ws = dir.path().join("workspace");
            std::fs::create_dir_all(&ws).unwrap();
            let store = Store::open(&dir.path().join("agent.db")).unwrap();
            let tools = Arc::new(FileTools {
                ws: ws.clone(),
                indexed: AtomicUsize::new(0),
                stall_write: AtomicBool::new(false),
                stall_index: AtomicBool::new(false),
                index_started: Notify::new(),
                writes: tokio::sync::watch::channel(0).0,
            });
            tokio::spawn(toolhost::flows::watch(ws.clone(), tools.writes.clone()));
            let sync = Arc::new(Syncer::new(url, store, tools.clone()).unwrap());
            Self {
                _dir: dir,
                ws,
                tools,
                sync,
            }
        }

        fn write(&self, name: &str, content: &str) {
            std::fs::create_dir_all(self.ws.join("flows")).unwrap();
            std::fs::write(self.ws.join("flows").join(name), content).unwrap();
        }

        fn file(&self, name: &str) -> Option<String> {
            std::fs::read_to_string(self.ws.join("flows").join(name)).ok()
        }

        fn indexed(&self) -> usize {
            self.tools.indexed.load(Ordering::SeqCst)
        }
    }

    struct FileTools {
        ws: PathBuf,
        indexed: AtomicUsize,
        stall_write: AtomicBool,
        stall_index: AtomicBool,
        index_started: Notify,
        writes: tokio::sync::watch::Sender<u64>,
    }

    #[tonic::async_trait]
    impl Tools for FileTools {
        async fn run(&self, _owner: &str, name: &str, args: Value) -> ToolOutput {
            let ws: &Path = &self.ws;
            let out = match name {
                "flows_scan" => toolhost::flows::scan(ws).await,
                "flows_read" => toolhost::flows::read(ws, args).await,
                "flows_write" => {
                    let written = toolhost::flows::write(ws, args).await;
                    if self.stall_write.swap(false, Ordering::SeqCst) {
                        tokio::time::sleep(PULL_BUDGET * 2).await;
                    }
                    written
                }
                "flows_remove" => toolhost::flows::remove(ws, args).await,
                "flows_mark" => toolhost::flows::mark(ws, args).await,
                "flows_wait" => toolhost::flows::wait(&self.writes, args).await,
                "flows_index" => {
                    self.index_started.notify_one();
                    if self.stall_index.swap(false, Ordering::SeqCst) {
                        tokio::time::sleep(PULL_BUDGET * 2).await;
                    }
                    self.indexed.fetch_add(1, Ordering::SeqCst);
                    Ok("{}".to_owned())
                }
                other => Err(format!("unexpected {other}")),
            };
            match out {
                Ok(text) => ToolOutput {
                    text,
                    image: None,
                    is_error: false,
                },
                Err(e) => ToolOutput::error(e),
            }
        }
    }

    #[derive(Default)]
    struct DoorState {
        rev: u64,
        flows: HashMap<String, (u64, String)>,
        puts: usize,
        refuse: bool,
    }

    /// A box door that answers one HTTP/1.1 request per connection.
    struct Door {
        addr: std::net::SocketAddr,
        state: Arc<Mutex<DoorState>>,
    }

    impl Door {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let state = Arc::new(Mutex::new(DoorState::default()));
            let shared = Arc::clone(&state);
            tokio::spawn(async move {
                while let Ok((conn, _)) = listener.accept().await {
                    tokio::spawn(serve(conn, Arc::clone(&shared)));
                }
            });
            Self { addr, state }
        }

        fn put(&self, name: &str, content: &str) {
            let mut st = lock(&self.state);
            st.rev += 1;
            let rev = st.rev;
            st.flows.insert(name.to_owned(), (rev, content.to_owned()));
        }

        fn delete(&self, name: &str) {
            let mut st = lock(&self.state);
            st.rev += 1;
            st.flows.remove(name);
        }

        fn content(&self, name: &str) -> Option<String> {
            lock(&self.state).flows.get(name).map(|(_, c)| c.clone())
        }

        fn puts(&self) -> usize {
            lock(&self.state).puts
        }
    }

    async fn serve(mut conn: tokio::net::TcpStream, state: Arc<Mutex<DoorState>>) {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        let (head, body) = loop {
            let n = conn.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&raw[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .map_or(0, |v| v.trim().parse::<usize>().unwrap());
            while raw.len() < end + 4 + length {
                let n = conn.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            break (
                head,
                String::from_utf8(raw[end + 4..end + 4 + length].to_vec()).unwrap(),
            );
        };
        let mut words = head.split_whitespace();
        let (method, path) = (words.next().unwrap(), words.next().unwrap());
        let name = path.strip_prefix("/flows/").unwrap_or_default().to_owned();
        let (status, reply) = {
            let mut st = lock(&state);
            match (method, path) {
                ("GET", "/flows") => {
                    let flows: Vec<Value> = st
                        .flows
                        .iter()
                        .map(|(n, (rev, c))| json!({"name": n, "rev": rev, "sha256": digest(c.as_bytes())}))
                        .collect();
                    ("200 OK", json!({"rev": st.rev, "flows": flows}).to_string())
                }
                ("GET", _) => match st.flows.get(&name) {
                    Some((_, c)) => ("200 OK", c.clone()),
                    None => ("404 Not Found", String::new()),
                },
                ("PUT", _) => {
                    st.puts += 1;
                    if st.refuse {
                        ("413 Payload Too Large", "too big".to_owned())
                    } else {
                        st.rev += 1;
                        let rev = st.rev;
                        st.flows.insert(name.clone(), (rev, body.clone()));
                        let sha = digest(body.as_bytes());
                        (
                            "200 OK",
                            json!({"name": name, "rev": rev, "sha256": sha}).to_string(),
                        )
                    }
                }
                _ => ("405 Method Not Allowed", String::new()),
            }
        };
        let resp = format!(
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
            reply.len()
        );
        conn.write_all(resp.as_bytes()).await.unwrap();
    }
}
