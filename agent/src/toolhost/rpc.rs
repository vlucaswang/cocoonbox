//! Toolhost wire format: one length-prefixed JSON request and response per connection;
//! state transfers add raw data frames between them, and `exec` streams tagged frames
//! both ways.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use desk_core::args::parse;
use desk_core::wire::MAX_FRAME;
pub use desk_core::wire::{Call, Output, read_frame, write_frame};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use zeroize::Zeroize;

/// The bound of one desk or model call to shuttle's desk service.
pub const DESK_DEADLINE: Duration = Duration::from_secs(30);
/// What a call to shuttle's desk service gives when the service is missing or past the call's deadline.
pub const DESK_DOWN: &str = "the desk service is not answering";

/// Tag of a command's stdout data, from the toolhost.
pub const STDOUT: u8 = 1;
/// Tag of a command's stderr data, from the toolhost.
pub const STDERR: u8 = 2;
/// Tag of a command's exit code, a big-endian i32; the toolhost's last frame.
pub const EXIT: u8 = 3;
/// Tag of a command's stdin data, to the toolhost.
pub const STDIN: u8 = 4;
/// Tag of the end of a command's stdin, to the toolhost.
pub const EOF: u8 = 5;
/// Tag of a signal for the command's process group, a big-endian i32, to the toolhost.
pub const SIGNAL: u8 = 6;
/// Tag of a terminal size, big-endian u16 rows and cols, to the toolhost.
pub const RESIZE: u8 = 7;

/// Size and mtime of a file before or after it was streamed.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Stamp {
    pub size: u64,
    pub mtime_ms: i64,
    /// The bytes come from an SQLite online backup, consistent by construction.
    #[serde(default)]
    pub backup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Client side of the toolhost socket.
#[derive(Clone, Debug)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    /// Creates a client for the socket at `socket`.
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// Runs one call; dropping the future closes the connection, which cancels the tool.
    pub async fn call(&self, call: &Call) -> Result<Output, String> {
        let mut stream = self.open(call).await?;
        read_frame(&mut stream).await
    }

    /// Runs one call on shuttle's desk service within `deadline`; no answer by then, or no service, is [`DESK_DOWN`].
    pub async fn desk(&self, call: &Call, deadline: Duration) -> Result<Output, String> {
        match tokio::time::timeout(deadline, self.call(call)).await {
            Ok(Ok(out)) => Ok(out),
            _ => Err(DESK_DOWN.to_owned()),
        }
    }

    /// Streams a file as `user`; the stamp comes first, [`FileReader::finish`] returns the one taken after.
    pub async fn read_file(
        &self,
        path: &Path,
        sqlite_backup: bool,
    ) -> Result<(Stamp, FileReader), String> {
        let call = Call {
            agent_id: String::new(),
            tool: "state_read".to_owned(),
            args: json!({"path": path, "sqlite_backup": sqlite_backup}),
        };
        let mut stream = self.open(&call).await?;
        let stamp: Stamp = read_frame(&mut stream).await?;
        if let Some(e) = &stamp.error {
            return Err(e.clone());
        }
        Ok((stamp, FileReader { stream }))
    }

    /// Creates a file as `user` from data sent through the returned writer.
    pub async fn write_file(
        &self,
        path: &Path,
        mode: u32,
        mtime_ms: i64,
    ) -> Result<FileWriter, String> {
        let call = Call {
            agent_id: String::new(),
            tool: "state_write".to_owned(),
            args: json!({"path": path, "mode": mode, "mtime_ms": mtime_ms}),
        };
        Ok(FileWriter {
            stream: self.open(&call).await?,
        })
    }

    /// Starts `command` as `user`, on a terminal when `tty` is `{term, rows, cols}`; the
    /// connection then carries [`write_tagged`] frames both ways. Closing it kills the command.
    pub async fn exec(&self, command: &str, tty: Option<Value>) -> Result<UnixStream, String> {
        let call = Call {
            agent_id: String::new(),
            tool: "exec".to_owned(),
            args: json!({"command": command, "tty": tty}),
        };
        let mut stream = self.open(&call).await?;
        let started: Output = read_frame(&mut stream).await?;
        if started.is_error {
            Err(started.text)
        } else {
            Ok(stream)
        }
    }

    /// Sends `call` and hands back the connection for a streamed exchange.
    async fn open(&self, call: &Call) -> Result<UnixStream, String> {
        let mut stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("connect toolhost {}: {e}", self.socket.display()))?;
        write_frame(&mut stream, call).await?;
        Ok(stream)
    }
}

/// Data of a file being read by the toolhost.
pub struct FileReader {
    stream: UnixStream,
}

impl FileReader {
    /// Next block of the file; `None` at the end.
    pub async fn next(&mut self) -> Result<Option<Vec<u8>>, String> {
        read_data(&mut self.stream).await
    }

    /// The stamp taken after the whole file was sent.
    pub async fn finish(mut self) -> Result<Stamp, String> {
        read_frame(&mut self.stream).await
    }
}

/// A file being written by the toolhost.
pub struct FileWriter {
    stream: UnixStream,
}

impl FileWriter {
    /// Appends `data`.
    pub async fn send(&mut self, data: &[u8]) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        write_data(&mut self.stream, data).await
    }

    /// Ends the file and waits for the toolhost to set mode and mtime.
    pub async fn finish(mut self) -> Result<(), String> {
        write_data(&mut self.stream, &[]).await?;
        let out: Output = read_frame(&mut self.stream).await?;
        if out.is_error { Err(out.text) } else { Ok(()) }
    }
}

/// Zeroes the string fields of `args`, which may carry a credential.
pub fn wipe(args: &mut Value) {
    if let Some(fields) = args.as_object_mut() {
        for v in fields.values_mut() {
            if let Value::String(s) = v {
                s.zeroize();
            }
        }
    }
}

/// Writes one raw data frame; an empty slice ends the data.
pub async fn write_data<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<(), String> {
    let len = u32::try_from(data.len()).map_err(|_| "data frame too large".to_owned())?;
    w.write_all(&len.to_be_bytes())
        .await
        .map_err(|e| format!("write data: {e}"))?;
    w.write_all(data)
        .await
        .map_err(|e| format!("write data: {e}"))
}

/// Reads one raw data frame; `None` marks the end of the data.
pub async fn read_data<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>, String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)
        .await
        .map_err(|e| format!("read data length: {e}"))?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 {
        return Ok(None);
    }
    if len > MAX_FRAME {
        return Err(format!("data frame of {len} bytes exceeds {MAX_FRAME}"));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)
        .await
        .map_err(|e| format!("read data: {e}"))?;
    Ok(Some(data))
}

/// Writes one frame of an `exec` exchange: the tag, a big-endian u32 length and the data.
pub async fn write_tagged<W: AsyncWrite + Unpin>(
    w: &mut W,
    tag: u8,
    data: &[u8],
) -> Result<(), String> {
    let len = u32::try_from(data.len()).map_err(|_| "tagged frame too large".to_owned())?;
    let mut head = [tag, 0, 0, 0, 0];
    head[1..].copy_from_slice(&len.to_be_bytes());
    w.write_all(&head)
        .await
        .map_err(|e| format!("write tagged frame: {e}"))?;
    w.write_all(data)
        .await
        .map_err(|e| format!("write tagged frame: {e}"))
}

/// Reads one frame written by [`write_tagged`].
pub async fn read_tagged<R: AsyncRead + Unpin>(r: &mut R) -> Result<(u8, Vec<u8>), String> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)
        .await
        .map_err(|e| format!("read tagged frame header: {e}"))?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > MAX_FRAME {
        return Err(format!("tagged frame of {len} bytes exceeds {MAX_FRAME}"));
    }
    let mut data = vec![0u8; len];
    r.read_exact(&mut data)
        .await
        .map_err(|e| format!("read tagged frame: {e}"))?;
    Ok((head[0], data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let call = Call {
            agent_id: "ag".into(),
            tool: "shell".into(),
            args: serde_json::json!({"command": "ls"}),
        };
        write_frame(&mut a, &call).await.unwrap();
        let got: Call = read_frame(&mut b).await.unwrap();
        assert_eq!(got.tool, "shell");
        assert_eq!(got.args["command"], "ls");
    }

    #[tokio::test]
    async fn a_desk_call_past_its_deadline_or_without_a_service_is_down_without_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desk.sock");
        let call = Call {
            agent_id: String::new(),
            tool: "desk_windows".into(),
            args: Value::Null,
        };
        let missing = Client::new(path.clone()).desk(&call, DESK_DEADLINE).await;
        assert_eq!(missing.unwrap_err(), DESK_DOWN);
        let _silent = tokio::net::UnixListener::bind(&path).unwrap();
        let late = Client::new(path)
            .desk(&call, Duration::from_millis(100))
            .await;
        assert_eq!(late.unwrap_err(), DESK_DOWN);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
        assert!(read_frame::<_, Output>(&mut b).await.is_err());
    }
}
