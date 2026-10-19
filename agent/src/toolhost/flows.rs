//! The flow files of `/workspace/flows` that the library sync reads and writes; never MCP tools.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::{files, rpc};

/// The largest flow the library takes, in bytes.
pub const MAX_BYTES: u64 = 128 << 10;
const DIR: &str = "flows";
const MARK: &str = ".sync-id";
const INDEX: &str = "INDEX";
const MAX_NAME: usize = 96;
const EVENT_HEADER: usize = 16;
const EVENT_BUF: usize = 64 << 10;

pub static INDEX_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Deserialize)]
struct NameArgs {
    name: String,
}

#[derive(Deserialize)]
struct MarkArgs {
    id: String,
}

#[derive(Deserialize)]
struct WriteArgs {
    name: String,
    content: String,
}

/// Whether `name` is a flow file the library syncs: a `.json` or `.mts` name of letters, digits, `.`, `_` and `-`.
pub fn valid(name: &str) -> bool {
    name.len() <= MAX_NAME
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && (name.ends_with(".json") || name.ends_with(".mts"))
}

/// Hex SHA-256 of `data`.
pub fn digest(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn dir(workspace: &Path) -> PathBuf {
    workspace.join(DIR)
}

pub fn index(workspace: &Path) -> PathBuf {
    dir(workspace).join(INDEX)
}

/// `flows_scan`: JSON `{name: sha256}` of the regular flow files of at most [`MAX_BYTES`] that are UTF-8 text.
pub async fn scan(workspace: &Path) -> Result<String, String> {
    let dir = dir(workspace);
    let mut out = serde_json::Map::new();
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok("{}".to_owned()),
        Err(e) => return Err(format!("list {}: {e}", dir.display())),
    };
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("list {}: {e}", dir.display()))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !valid(&name) || !meta.is_file() || meta.len() > MAX_BYTES {
            continue;
        }
        let Ok(data) = tokio::fs::read(entry.path()).await else {
            continue;
        };
        if std::str::from_utf8(&data).is_ok() {
            out.insert(name, digest(&data).into());
        }
    }
    Ok(Value::Object(out).to_string())
}

/// `flows_read {name}`: the flow's text.
pub async fn read(workspace: &Path, args: Value) -> Result<String, String> {
    let NameArgs { name } = rpc::parse(args)?;
    let path = path_of(workspace, &name)?;
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("read {}: {e}", path.display()))
}

/// `flows_write {name, content}`: replaces the flow through a temporary file, so a reader never sees half of it.
pub async fn write(workspace: &Path, args: Value) -> Result<String, String> {
    let WriteArgs { name, content } = rpc::parse(args)?;
    let path = path_of(workspace, &name)?;
    let tmp = path.with_file_name(format!(".{name}.sync"));
    files::write_bytes(&tmp, content.as_bytes()).await?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(json!({"written": name}).to_string())
}

/// `flows_remove {name}`: deletes the flow; a missing one is no error.
pub async fn remove(workspace: &Path, args: Value) -> Result<String, String> {
    let NameArgs { name } = rpc::parse(args)?;
    let path = path_of(workspace, &name)?;
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(json!({"removed": name}).to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(json!({"removed": name}).to_string())
        }
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

/// `flows_mark {id}`: the folder's id, first written as `id` into a folder that has none; empty without a folder.
pub async fn mark(workspace: &Path, args: Value) -> Result<String, String> {
    let MarkArgs { id } = rpc::parse(args)?;
    let path = dir(workspace).join(MARK);
    match tokio::fs::read_to_string(&path).await {
        Ok(id) => return Ok(id),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    }
    match tokio::fs::write(&path, &id).await {
        Ok(()) => Ok(id),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("write {}: {e}", path.display())),
    }
}

/// `flows_wait {after}`: returns the count of flow writes once it passes `after`.
pub async fn wait(writes: &watch::Sender<u64>, args: Value) -> Result<String, String> {
    let after = args["after"].as_u64().unwrap_or(0);
    let mut rx = writes.subscribe();
    let seen = *rx
        .wait_for(|n| *n > after)
        .await
        .map_err(|e| format!("flow writes: {e}"))?;
    Ok(seen.to_string())
}

/// Counts in `writes` each write of a flow file into `workspace/flows` and drops each removed flow from `INDEX`, following the folder when a restore replaces it.
#[cfg(target_os = "linux")]
pub async fn watch(workspace: PathBuf, writes: watch::Sender<u64>) -> Result<(), String> {
    use std::io::Read;

    use tokio::io::unix::AsyncFd;

    let root_events = libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_ONLYDIR;
    let removals = libc::IN_DELETE | libc::IN_MOVED_FROM;
    let flow_events = libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | removals;
    let fd = crate::sys::inotify()
        .and_then(|fd| AsyncFd::new(std::fs::File::from(fd)))
        .map_err(|e| format!("inotify: {e}"))?;
    let root = crate::sys::inotify_watch(fd.get_ref(), &workspace, root_events)
        .map_err(|e| format!("watch {}: {e}", workspace.display()))?;
    let dir = dir(&workspace);
    let mut flows = crate::sys::inotify_watch(fd.get_ref(), &dir, flow_events).ok();
    let mut buf = vec![0u8; EVENT_BUF];
    loop {
        let mut ready = fd.readable().await.map_err(|e| format!("inotify: {e}"))?;
        let Ok(read) = ready.try_io(|f| {
            let mut file = f.get_ref();
            file.read(&mut buf)
        }) else {
            continue;
        };
        let n = read.map_err(|e| format!("read inotify: {e}"))?;
        let (mut written, mut removed) = (false, false);
        let mut at = 0;
        while at + EVENT_HEADER <= n {
            let word = |i: usize| {
                let mut b = [0u8; 4];
                b.copy_from_slice(&buf[at + i..at + i + 4]);
                b
            };
            let (wd, mask) = (i32::from_ne_bytes(word(0)), u32::from_ne_bytes(word(4)));
            let len = u32::from_ne_bytes(word(12)) as usize;
            let raw = &buf[(at + EVENT_HEADER).min(n)..(at + EVENT_HEADER + len).min(n)];
            let name = String::from_utf8_lossy(raw.split(|b| *b == 0).next().unwrap_or_default());
            at += EVENT_HEADER + len;
            if mask & libc::IN_Q_OVERFLOW != 0 {
                (written, removed) = (true, true);
            } else if wd == root && name == DIR {
                if let Some(old) = flows.take() {
                    crate::sys::inotify_unwatch(fd.get_ref(), old);
                }
                flows = crate::sys::inotify_watch(fd.get_ref(), &dir, flow_events).ok();
                written = true;
            } else if Some(wd) == flows && valid(&name) {
                if mask & removals == 0 {
                    written = true;
                } else {
                    removed |= name.ends_with(".json");
                }
            }
        }
        if written {
            writes.send_modify(|w| *w += 1);
        }
        if removed && let Err(e) = prune(&dir).await {
            eprintln!("toolhost: prune the flow index: {e}");
        }
    }
}

/// Drops from `dir/INDEX` each flow whose `.json` file is gone, rewriting the file in place so that a folder being removed gains no entry.
#[cfg(target_os = "linux")]
async fn prune(dir: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let _index = INDEX_LOCK.lock().await;
    let path = dir.join(INDEX);
    let index = match tokio::fs::read_to_string(&path).await {
        Ok(index) => index,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    let (mut kept, mut dropped) = (String::new(), false);
    for line in index.lines().filter(|l| !l.is_empty()) {
        let name = line.split('(').next().unwrap_or_default();
        if tokio::fs::try_exists(dir.join(format!("{name}.json")))
            .await
            .unwrap_or(true)
        {
            kept.push_str(line);
            kept.push('\n');
        } else {
            dropped = true;
        }
    }
    if !dropped {
        return Ok(());
    }
    let opened = tokio::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .await;
    let mut file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("open {}: {e}", path.display())),
    };
    file.write_all(kept.as_bytes())
        .await
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Without inotify no flow write is seen.
#[cfg(not(target_os = "linux"))]
pub async fn watch(_workspace: PathBuf, _writes: watch::Sender<u64>) -> Result<(), String> {
    Err("no inotify on this system".to_owned())
}

fn path_of(workspace: &Path, name: &str) -> Result<PathBuf, String> {
    if !valid(name) {
        return Err(format!("{name:?} is no flow name"));
    }
    Ok(dir(workspace).join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_takes_plain_json_and_mts_names_only() {
        for name in ["mousepad-save.json", "save_note.mts", "A1.b.json"] {
            assert!(valid(name), "{name}");
        }
        for name in [
            ".pending.json",
            ".runs.jsonl",
            "INDEX",
            "x.txt",
            "a/b.json",
            "../a.json",
            "-a.json",
        ] {
            assert!(!valid(name), "{name}");
        }
        assert!(!valid(&format!("{}.json", "a".repeat(92))));
    }

    #[tokio::test]
    async fn scan_write_read_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        assert_eq!(scan(ws).await.unwrap(), "{}");
        write(ws, json!({"name": "a.json", "content": "{\"v\":1}"}))
            .await
            .unwrap();
        std::fs::write(ws.join("flows/INDEX"), "a()").unwrap();
        std::fs::write(ws.join("flows/bin.json"), [0xff, 0xfe]).unwrap();
        std::fs::write(
            ws.join("flows/big.mts"),
            vec![b'a'; usize::try_from(MAX_BYTES).unwrap() + 1],
        )
        .unwrap();
        let scanned: Value = serde_json::from_str(&scan(ws).await.unwrap()).unwrap();
        assert_eq!(scanned, json!({"a.json": digest(b"{\"v\":1}")}));
        assert_eq!(
            read(ws, json!({"name": "a.json"})).await.unwrap(),
            "{\"v\":1}"
        );
        assert!(read(ws, json!({"name": "../x.json"})).await.is_err());
        remove(ws, json!({"name": "a.json"})).await.unwrap();
        remove(ws, json!({"name": "a.json"})).await.unwrap();
        assert!(!ws.join("flows/a.json").exists());
    }

    #[tokio::test]
    async fn mark_keeps_the_first_id_and_creates_no_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        assert_eq!(mark(ws, json!({"id": "a"})).await.unwrap(), "");
        assert!(!ws.join("flows").exists());
        std::fs::create_dir(ws.join("flows")).unwrap();
        assert_eq!(mark(ws, json!({"id": "a"})).await.unwrap(), "a");
        assert_eq!(mark(ws, json!({"id": "b"})).await.unwrap(), "a");
        assert!(mark(ws, json!({})).await.is_err());
        assert_eq!(scan(ws).await.unwrap(), "{}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn watch_counts_flow_writes_and_follows_a_replaced_folder() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_path_buf();
        let writes = watch::channel(0).0;
        tokio::spawn(super::watch(ws.clone(), writes.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let next = |after: u64| {
            let writes = writes.clone();
            async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    wait(&writes, json!({"after": after})),
                )
                .await
                .expect("no flow write seen")
                .unwrap()
                .parse::<u64>()
                .unwrap()
            }
        };

        std::fs::create_dir(ws.join("flows")).unwrap();
        let created = next(0).await;
        std::fs::write(ws.join("flows/a.json"), "{}").unwrap();
        let written = next(created).await;
        std::fs::write(ws.join("flows/INDEX"), "a()").unwrap();
        std::fs::write(ws.join("flows/.runs.jsonl"), "{}").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(*writes.borrow(), written, "a file that is no flow counted");

        std::fs::create_dir_all(ws.join("staged/flows")).unwrap();
        std::fs::create_dir(ws.join("trash")).unwrap();
        std::fs::rename(ws.join("flows"), ws.join("trash/flows")).unwrap();
        std::fs::rename(ws.join("staged/flows"), ws.join("flows")).unwrap();
        let swapped = next(written).await;
        std::fs::remove_dir_all(ws.join("trash")).unwrap();
        std::fs::write(ws.join("flows/b.mts"), "x").unwrap();
        next(swapped).await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn watch_drops_a_removed_flow_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().to_path_buf();
        let flows = ws.join("flows");
        std::fs::create_dir(&flows).unwrap();
        for name in ["a.json", "b.json", "c.json", "d.mts"] {
            std::fs::write(flows.join(name), "{}").unwrap();
        }
        std::fs::write(
            flows.join("INDEX"),
            "a(x): one\nb(): two\nc() once: three\n",
        )
        .unwrap();
        tokio::spawn(super::watch(ws.clone(), watch::channel(0).0));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let index_becomes = |want: &'static str| {
            let path = flows.join("INDEX");
            async move {
                let started = std::time::Instant::now();
                loop {
                    let index = std::fs::read_to_string(&path).unwrap_or_default();
                    if index == want || started.elapsed() > std::time::Duration::from_secs(2) {
                        return index;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
        };

        std::fs::remove_file(flows.join("a.json")).unwrap();
        assert_eq!(
            index_becomes("b(): two\nc() once: three\n").await,
            "b(): two\nc() once: three\n"
        );
        std::fs::rename(flows.join("c.json"), ws.join("c.json")).unwrap();
        assert_eq!(index_becomes("b(): two\n").await, "b(): two\n");
        std::fs::remove_file(flows.join("d.mts")).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            std::fs::read_to_string(flows.join("INDEX")).unwrap(),
            "b(): two\n"
        );
        std::fs::remove_dir_all(&flows).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!flows.exists(), "the removed folder came back");
    }
}
