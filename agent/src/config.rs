//! Environment configuration for both subcommands.

use std::env;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::plugins::{self, Plugin};
use crate::toolhost::state::Root;

const DEFAULT_STATE_DIR: &str = "/var/lib/cocoonbox";
const DEFAULT_TOOLHOST_SOCKET: &str = "/run/cocoonbox/toolhost.sock";
const DEFAULT_OFFLOAD_SOCKET: &str = "/run/cocoonbox/offload.sock";
const DEFAULT_VNC_ADDR: &str = "127.0.0.1:5900";
const DEFAULT_WORKSPACE: &str = "/workspace";
const IDENTITY_PATH: &str = "/run/cocoonbox-identity/identity";
const ROOT_UID: &str = "0";
const REDACTED: &str = "<redacted>";
const DESKTOP_UNIT: &str = "cocoonbox-session.service";
const SESSION_SAVE: &str = "/usr/local/lib/cocoonbox/session-save";
const NODE_BIN: &str = "/usr/local/bin/node";
const DESK_GUIDE: &str = "/opt/desk/AGENTS.md";
const ANDROID_GUIDE: &str = "/opt/android/AGENTS.md";
const TOOLS_DIR: &str = "/etc/cocoonbox/tools.d";
const ROUTING_DIR: &str = "/etc/cocoonbox/routing.d";
const DESK_ENV: &str = "/etc/cocoonbox/desk.env";
const MCP_IDLE: Duration = Duration::from_secs(600);
const MCP_PRUNE_AFTER: Duration = Duration::from_secs(24 * 3600);
const QUIESCE_WAIT: Duration = Duration::from_secs(30);

/// The desktop automation of the box's image family, found by its guide at start.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Automation {
    /// cua-driver only: the cbdesk family.
    #[default]
    Plain,
    /// GUI plans and scripts on Node with the desk API.
    Desk,
    /// Android in the box, driven through adb: the cbdroid family.
    Android,
}

impl Automation {
    /// The automation whose guide the image holds.
    pub fn detect() -> Self {
        if Path::new(ANDROID_GUIDE).exists() {
            Self::Android
        } else if Path::new(DESK_GUIDE).exists() {
            Self::Desk
        } else {
            Self::Plain
        }
    }

    /// Whether the box runs GUI plans on its desktop, with still-screen settling, scaled screenshots, action lists, browser_fetch.
    pub fn plans(self) -> bool {
        self == Self::Desk
    }

    /// Whether the box runs GUI plans and scripts on the desk API: a desk box, and an Android box through adb.
    pub fn scripts(self) -> bool {
        self != Self::Plain
    }
}

/// Settings of `serve`, read from `/etc/cocoonbox/agent.env` via systemd.
#[derive(Clone)]
pub struct ServeConfig {
    pub gw_base_url: String,
    pub gw_api_key: String,
    pub model_main: String,
    pub model_computer: String,
    pub identity_path: PathBuf,
    pub identity_owner: u32,
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
    pub toolhost_socket: PathBuf,
    /// The offload inbox, whose changes become `offload` events.
    pub offload_socket: PathBuf,
    pub vnc_addr: String,
    pub anthropic_messages: bool,
    pub mcp_idle: Duration,
    pub mcp_prune_after: Duration,
    /// How long a quiesce waits for running turns and MCP calls before it interrupts the share calls.
    pub quiesce_wait: Duration,
    pub state_roots: Vec<Root>,
    pub desktop_unit: Option<String>,
    /// The Connect box door; without it the loop offers no connector tools.
    pub connect_url: Option<String>,
    /// The box's desktop automation, which adds GUI plans, a script tool and its guide to MCP.
    pub automation: Automation,
    /// Script tools the image adds; a box that runs GUI plans offers them.
    pub plugins: Vec<Plugin>,
    /// Paragraphs the image appends to the box agent's routing and, within their cap, to the MCP instructions.
    pub routing_extra: Vec<String>,
    /// shuttle's desk endpoint, which takes the desk calls and runs straight from serve when set.
    pub shuttle_socket: Option<PathBuf>,
    /// The workspace that relative script paths of runs on shuttle start from.
    pub workspace: PathBuf,
}

impl std::fmt::Debug for ServeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeConfig")
            .field("gw_base_url", &self.gw_base_url)
            .field("gw_api_key", &REDACTED)
            .field("model_main", &self.model_main)
            .field("model_computer", &self.model_computer)
            .field("identity_path", &self.identity_path)
            .field("identity_owner", &self.identity_owner)
            .field("listen", &self.listen)
            .field("state_dir", &self.state_dir)
            .field("toolhost_socket", &self.toolhost_socket)
            .field("offload_socket", &self.offload_socket)
            .field("vnc_addr", &self.vnc_addr)
            .field("anthropic_messages", &self.anthropic_messages)
            .field("mcp_idle", &self.mcp_idle)
            .field("mcp_prune_after", &self.mcp_prune_after)
            .field("quiesce_wait", &self.quiesce_wait)
            .field("state_roots", &self.state_roots)
            .field("desktop_unit", &self.desktop_unit)
            .field("connect_url", &self.connect_url)
            .field("automation", &self.automation)
            .field("plugins", &self.plugins)
            .field("routing_extra", &self.routing_extra)
            .field("shuttle_socket", &self.shuttle_socket)
            .field("workspace", &self.workspace)
            .finish()
    }
}

impl ServeConfig {
    /// Reads the contract keys; optional overrides exist for tests and debugging.
    pub fn from_env() -> Result<Self, String> {
        let listen = required("AGENT_LISTEN")?;
        Ok(Self {
            gw_base_url: required("GW_BASE_URL")?.trim_end_matches('/').to_owned(),
            gw_api_key: required("GW_API_KEY")?,
            model_main: required("MODEL_MAIN")?,
            model_computer: required("MODEL_COMPUTER")?,
            identity_path: optional("CB_IDENTITY_PATH", IDENTITY_PATH).into(),
            identity_owner: optional("CB_IDENTITY_OWNER", ROOT_UID)
                .parse()
                .map_err(|e| format!("parse CB_IDENTITY_OWNER: {e}"))?,
            listen: listen
                .parse()
                .map_err(|e| format!("parse AGENT_LISTEN {listen}: {e}"))?,
            state_dir: optional("CB_STATE_DIR", DEFAULT_STATE_DIR).into(),
            toolhost_socket: optional("TOOLHOST_SOCKET", DEFAULT_TOOLHOST_SOCKET).into(),
            offload_socket: optional("OFFLOAD_SOCKET", DEFAULT_OFFLOAD_SOCKET).into(),
            vnc_addr: optional("VNC_ADDR", DEFAULT_VNC_ADDR),
            anthropic_messages: optional("GW_ANTHROPIC_WIRE", "chat") == "messages",
            mcp_idle: MCP_IDLE,
            mcp_prune_after: MCP_PRUNE_AFTER,
            quiesce_wait: QUIESCE_WAIT,
            state_roots: crate::state::default_roots(),
            desktop_unit: Some(DESKTOP_UNIT.to_owned()),
            connect_url: env::var("CONNECT_URL").ok().filter(|v| !v.is_empty()),
            automation: Automation::detect(),
            plugins: plugins::load(Path::new(&optional("TOOLS_DIR", TOOLS_DIR))),
            routing_extra: routing_extra(Path::new(&optional("ROUTING_DIR", ROUTING_DIR))),
            shuttle_socket: shuttle_socket(),
            workspace: optional("CB_WORKSPACE", DEFAULT_WORKSPACE).into(),
        })
    }
}

/// Settings of `toolhost`.
#[derive(Clone, Debug)]
pub struct ToolhostConfig {
    pub socket: PathBuf,
    pub socket_group: String,
    pub workspace: PathBuf,
    pub chrome_bin: String,
    pub chrome_profile: PathBuf,
    pub cdp_port: u16,
    pub cua_socket: PathBuf,
    /// The program that runs scripts and GUI plans.
    pub script_bin: PathBuf,
    pub session_save: PathBuf,
    pub home: PathBuf,
    /// The box's desktop automation.
    pub automation: Automation,
    /// Script tools the image adds, each run by its own program.
    pub plugins: Vec<Plugin>,
    /// The adb program of an Android box.
    pub adb_bin: PathBuf,
    /// The gw endpoint of the desk runtime's model calls.
    pub gw_base_url: Option<String>,
    /// shuttle's desk endpoint, which answers the desk verbs, model calls and runs when set.
    pub shuttle_socket: Option<PathBuf>,
}

impl ToolhostConfig {
    /// Reads toolhost settings; every key has a contract default.
    pub fn from_env() -> Result<Self, String> {
        let home = env::var("HOME").map_err(|e| format!("read HOME: {e}"))?;
        Ok(Self {
            socket: optional("TOOLHOST_SOCKET", DEFAULT_TOOLHOST_SOCKET).into(),
            socket_group: optional("TOOLHOST_SOCKET_GROUP", "cbagent"),
            workspace: optional("CB_WORKSPACE", DEFAULT_WORKSPACE).into(),
            chrome_bin: optional("CHROME_BIN", "google-chrome"),
            chrome_profile: env::var("CHROME_PROFILE").map_or_else(
                |_| format!("{home}/.config/cocoonbox-chrome").into(),
                Into::into,
            ),
            cdp_port: optional("CDP_PORT", "9222")
                .parse()
                .map_err(|e| format!("parse CDP_PORT: {e}"))?,
            cua_socket: env::var("CUA_DRIVER_SOCKET").map_or_else(
                |_| format!("{home}/.cache/cua-driver/cua-driver.sock").into(),
                Into::into,
            ),
            script_bin: optional("SCRIPT_BIN", NODE_BIN).into(),
            session_save: SESSION_SAVE.into(),
            home: home.into(),
            automation: Automation::detect(),
            plugins: plugins::load(Path::new(&optional("TOOLS_DIR", TOOLS_DIR))),
            adb_bin: optional("ADB_BIN", "adb").into(),
            gw_base_url: env::var("GW_BASE_URL")
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| env_file_value(Path::new(DESK_ENV), "GW_BASE_URL")),
            shuttle_socket: shuttle_socket(),
        })
    }
}

/// The `*.md` files in `dir` in file-name order, each trimmed at its end; none without any.
pub fn routing_extra(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    let mut parts = Vec::new();
    for file in files {
        match std::fs::read_to_string(&file) {
            Ok(part) => parts.push(part.trim_end().to_owned()),
            Err(e) => eprintln!("routing: skip {}: {e}", file.display()),
        }
    }
    parts
}

fn required(key: &str) -> Result<String, String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => Err(format!("missing required env {key}")),
    }
}

fn env_file_value(path: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .map(str::to_owned)
        .filter(|v| !v.is_empty())
}

fn optional(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// shuttle's desk endpoint from `SHUTTLE_SOCKET`; none when unset.
fn shuttle_socket() -> Option<PathBuf> {
    env::var_os("SHUTTLE_SOCKET")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secrets() {
        let cfg = ServeConfig {
            gw_base_url: "http://gw/v1".into(),
            gw_api_key: "sk-secret-key".into(),
            model_main: "m".into(),
            model_computer: "c".into(),
            identity_path: "/i".into(),
            identity_owner: 0,
            listen: "127.0.0.1:1".parse().unwrap(),
            state_dir: "/s".into(),
            toolhost_socket: "/t".into(),
            offload_socket: "/o".into(),
            vnc_addr: "v".into(),
            anthropic_messages: false,
            mcp_idle: MCP_IDLE,
            mcp_prune_after: MCP_PRUNE_AFTER,
            quiesce_wait: QUIESCE_WAIT,
            state_roots: Vec::new(),
            desktop_unit: None,
            connect_url: None,
            automation: Automation::Plain,
            plugins: Vec::new(),
            routing_extra: Vec::new(),
            shuttle_socket: None,
            workspace: "/w".into(),
        };
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("sk-secret-key"), "{shown}");
        assert!(
            shown.contains("<redacted>") && shown.contains("http://gw/v1"),
            "{shown}"
        );
    }

    #[test]
    fn routing_extra_joins_the_md_files_in_file_name_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("20-b.md"), "second\n").unwrap();
        std::fs::write(dir.path().join("10-a.md"), "first").unwrap();
        std::fs::write(dir.path().join("30-c.txt"), "ignored").unwrap();
        assert_eq!(routing_extra(dir.path()), ["first", "second"]);
        assert!(routing_extra(Path::new("/nonexistent/routing.d")).is_empty());
    }
}
