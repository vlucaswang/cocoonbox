//! Desk tools: accessibility snapshots of desktop windows, actions on their elements, and desktop
//! input, window and launch calls through cua-driver.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::future::join_all;
use serde_json::{Map, Value, json};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask};

use desk_core::args::{
    ActArgs, AtArgs, ClickArgs, FindArgs, IdArgs, KeyArgs, LaunchArgs, ReadArgs, ShotArgs,
    SnapArgs, TypeArgs, WaitArgs, WindowArgs, parse,
};
use desk_core::press::{MENU_POLL, MENU_POLLS, SELECTED_ROLES};
use desk_core::text::{MAX_FOUND, NAME_CHARS, Named, cli_args, matches, ms, quote};
use desk_core::window::{self, TITLE_TIERS, Window, app_window, title_matches};

use super::atspi::{Atspi, Extras, FOCUSED, Frame, Node, Obj, SHOWING, key_combos};
use super::cua::{Cua, libreoffice};
use super::files;
use super::rpc::{Call, Client, Output};

const FRAME_ROLES: [u32; 5] = [2, 16, 19, 23, WINDOW_ROLE];
const WINDOW_ROLE: u32 = 69;
const DEFAULT_SOCKET: &str = "/run/cocoonbox/toolhost.sock";
const CLI_USAGE: &str = "usage: desk VERB [key=value ...|JSON]; verbs: windows, snapshot, find, read, press, act, at, key, type, click, focus, close, launch, shot, screen";
const READ_CHARS: i32 = 64 << 10;
const TYPE_SETTLE: Duration = Duration::from_millis(100);
const TYPE_POLL: Duration = Duration::from_millis(10);
const TYPE_TAIL: usize = 16;
const WAIT_MAX: Duration = Duration::from_secs(5);
const WAIT_POLL: Duration = Duration::from_millis(10);
const KEY_ALIASES: [(&str, &str); 47] = [
    ("control", "ctrl"),
    ("primary", "ctrl"),
    ("ctl", "ctrl"),
    ("control_l", "ctrl"),
    ("control_r", "ctrl"),
    ("ctrlleft", "ctrl"),
    ("ctrlright", "ctrl"),
    ("shift_l", "shift"),
    ("shift_r", "shift"),
    ("shiftleft", "shift"),
    ("shiftright", "shift"),
    ("alt_l", "alt"),
    ("alt_r", "alt"),
    ("altleft", "alt"),
    ("altright", "alt"),
    ("option", "alt"),
    ("mod1", "alt"),
    ("cmd", "super"),
    ("command", "super"),
    ("meta", "super"),
    ("win", "super"),
    ("windows", "super"),
    ("winleft", "super"),
    ("winright", "super"),
    ("super_l", "super"),
    ("super_r", "super"),
    ("mod4", "super"),
    ("enter", "return"),
    ("kp_enter", "return"),
    ("esc", "escape"),
    ("del", "delete"),
    ("ins", "insert"),
    ("prior", "pageup"),
    ("page_up", "pageup"),
    ("pgup", "pageup"),
    ("next", "pagedown"),
    ("page_down", "pagedown"),
    ("pgdn", "pagedown"),
    ("arrowup", "up"),
    ("arrowdown", "down"),
    ("arrowleft", "left"),
    ("arrowright", "right"),
    ("spacebar", "space"),
    ("caps_lock", "capslock"),
    ("num_lock", "numlock"),
    ("back_space", "backspace"),
    ("bksp", "backspace"),
];

/// Snapshots and element actions over AT-SPI; the bus connection opens on first use.
#[derive(Default)]
pub struct Desk {
    atspi: Atspi,
    shuttle: Option<Client>,
}

impl Desk {
    /// A desk whose point lookups go to shuttle, when set.
    pub fn new(shuttle: Option<Client>) -> Self {
        Self {
            shuttle,
            ..Self::default()
        }
    }

    /// Runs desk tool `tool`; input, window and launch calls go through `cua`.
    pub async fn run(&self, cua: &Cua, tool: &str, args: Value) -> Result<Output, String> {
        let text = match tool {
            "desk_windows" => self.windows(cua).await?,
            "desk_wait" => {
                let a: WaitArgs = parse(args)?;
                wait_for(a.since, Duration::from_millis(a.timeout_ms), || async {
                    serde_json::from_str(&self.windows(cua).await?)
                        .map_err(|e| format!("decode windows: {e}"))
                })
                .await?
                .to_string()
            }
            "desk_snapshot" => self.snapshot(parse(args)?).await?,
            "desk_find" => self.find(parse(args)?).await?,
            "desk_read" => self.read(parse(args)?).await?,
            "desk_press" => self.press(cua, parse::<IdArgs>(args)?.id).await?,
            "desk_act" => self.act(cua, parse(args)?).await?,
            "desk_at" => self.at(parse(args)?).await?,
            "desk_key" => key(cua, &parse::<KeyArgs>(args)?.keys).await?,
            "desk_type" => {
                let text = parse::<TypeArgs>(args)?.text;
                let out = type_text(cua, &text).await?;
                self.typed(&text).await;
                out
            }
            "desk_click" => click(cua, parse(args)?).await?,
            "desk_focus" => {
                let a = parse(args)?;
                match focus(cua, &a).await {
                    Err(e) if e.starts_with("no window") => {
                        focus(cua, &self.by_app(&a).await?).await?
                    }
                    r => r?,
                }
            }
            "desk_close" => {
                let a = parse(args)?;
                match close(&a) {
                    Err(e) if e.starts_with("no window") => close(&self.by_app(&a).await?)?,
                    r => r?,
                }
            }
            "desk_launch" => {
                let app = parse::<LaunchArgs>(args)?.app;
                let v = cua.desk_call("launch_app", json!({"name": app})).await?;
                json!({"ok": true, "launched": text_of(&v)}).to_string()
            }
            "desk_shot" => return shot(cua, parse(args)?).await,
            "desk_screen" => {
                let v = cua.desk_call("get_screen_size", json!({})).await?;
                v["structuredContent"].to_string()
            }
            other => return Err(format!("unknown desk tool {other}")),
        };
        Ok(Output::text(text))
    }

    /// The accessible top-level windows, focused first; a modal dialog the screen does not show says where it is.
    async fn windows(&self, cua: &Cua) -> Result<String, String> {
        let mut frames = self.frames().await?;
        frames.sort_by_key(|f| !f.active());
        let modals = cua.modals();
        let list: Vec<Value> = frames
            .iter()
            .map(|f| {
                let mut v = frame_json(f);
                if let Some(m) = modals.iter().find(|m| m.title == f.name && m.pid == f.pid) {
                    v["modal"] = m.place.as_str().into();
                    v["blocks"] = m.owner.as_str().into();
                }
                v
            })
            .collect();
        Ok(Value::Array(list).to_string())
    }

    /// The element tree of one window: the one `id` names, else the focused one unless `window` names another.
    async fn snapshot(&self, a: SnapArgs) -> Result<String, String> {
        let started = Instant::now();
        let frames = self.frames().await?;
        let found = started.elapsed().as_micros();
        let frame = match a.id.as_deref() {
            Some(id) => frames
                .into_iter()
                .find(|f| f.obj.id() == id)
                .ok_or_else(|| format!("no window {id}"))?,
            None => window::pick(frames, a.window.as_deref(), a.pid)?,
        };
        let extras = Extras {
            values: true,
            bounds: a.bounds.unwrap_or(true),
            keys: a.keys,
        };
        let tree = self.atspi.tree(frame, extras).await?;
        let mut phases = Map::new();
        phases.insert("frames".into(), ms(found));
        for (name, us) in &tree.phases {
            phases.insert((*name).into(), ms(*us));
        }
        let mut out = json!({
            "window": frame_json(&tree.frame),
            "ms": ms(started.elapsed().as_micros()),
            "phases": phases,
            "truncated": tree.truncated,
        });
        if a.text {
            out["text"] = outline(&tree.nodes, a.max_chars.unwrap_or(NAME_CHARS)).into();
        } else {
            out["nodes"] = tree.nodes.iter().enumerate().map(node_json).collect();
        }
        Ok(out.to_string())
    }

    /// The elements of one window whose name is `name`, else those that contain it, ignoring case and punctuation.
    async fn find(&self, a: FindArgs) -> Result<String, String> {
        let frame = window::pick(self.frames().await?, a.window.as_deref(), a.pid)?;
        let extras = Extras {
            values: true,
            bounds: true,
            keys: false,
        };
        let tree = self.atspi.tree(frame, extras).await?;
        let found: Vec<Value> = matches(&tree.nodes, &a.name, a.role.as_deref())
            .into_iter()
            .take(MAX_FOUND)
            .map(|i| node_json((i, &tree.nodes[i])))
            .collect();
        Ok(json!({"window": frame_json(&tree.frame), "found": found}).to_string())
    }

    /// One element read live, with its full text when `text` is set.
    async fn read(&self, a: ReadArgs) -> Result<String, String> {
        let obj = Obj::parse(&a.id)?;
        let extras = Extras {
            values: true,
            bounds: true,
            keys: true,
        };
        let chars = if a.text {
            READ_CHARS
        } else {
            NAME_CHARS as i32
        };
        let node = self.atspi.node(&obj, extras, chars).await?;
        Ok(node_json((0, &node)).to_string())
    }

    /// Presses an element the way a user would: selects a tab or list row, opens the menus above a hidden
    /// menu entry, clicks the element's centre when the element is what lies there and no LibreOffice
    /// control outside a menu, whose extents can sit above it, else runs its action.
    async fn press(&self, cua: &Cua, id: String) -> Result<String, String> {
        let obj = Obj::parse(&id)?;
        let shown = Extras {
            values: false,
            bounds: true,
            keys: false,
        };
        let mut node = self.atspi.node(&obj, shown, 0).await?;
        let role = node.role();
        if SELECTED_ROLES.contains(&role) && self.atspi.select(&obj).await.unwrap_or(false) {
            return Ok(json!({"ok": true, "how": "select"}).to_string());
        }
        let chain = self.atspi.ancestors(&obj).await?;
        if role.starts_with("menu") && !node.has(SHOWING) {
            self.open_menus(&chain).await;
            for _ in 0..MENU_POLLS {
                node = self.atspi.node(&obj, shown, 0).await?;
                if node.has(SHOWING) {
                    break;
                }
                tokio::time::sleep(MENU_POLL).await;
            }
        }
        let pixel = role.starts_with("menu")
            || !self
                .atspi
                .pid(&obj.bus)
                .await
                .is_ok_and(|p| libreoffice(p.into()));
        if pixel && let Some((x, y)) = self.reachable(&obj, &node, &chain).await {
            cua.desk_input("click", json!({"x": x, "y": y})).await?;
            return Ok(json!({"ok": true, "how": "click", "x": x, "y": y}).to_string());
        }
        let done = self.atspi.do_action(&obj).await?;
        Ok(json!({"ok": done, "how": "action"}).to_string())
    }

    /// Opens the menus right above a menu entry, outermost first, by their own actions.
    async fn open_menus(&self, chain: &[Obj]) {
        let nodes = join_all(
            chain
                .iter()
                .map(|o| self.atspi.node(o, Extras::default(), 0)),
        )
        .await;
        let start = nodes
            .iter()
            .rposition(|n| !n.as_ref().is_ok_and(|n| n.role() == "menu"))
            .map_or(0, |i| i + 1);
        for menu in &chain[start..] {
            let _ = self.atspi.do_action(menu).await;
        }
    }

    /// The centre of `node` when a click there reaches it: a menu entry that shows, else an element whose
    /// centre lies inside every container above it, where a hit test of its window finds it, a part of it
    /// or a container of it.
    async fn reachable(&self, obj: &Obj, node: &Node, chain: &[Obj]) -> Option<(i32, i32)> {
        let [x, y, w, h] = node.bounds?;
        if !node.has(SHOWING) || w <= 0 || h <= 0 || x < 0 || y < 0 {
            return None;
        }
        let (cx, cy) = (x + w / 2, y + h / 2);
        if node.role().starts_with("menu") {
            return Some((cx, cy));
        }
        let shown = Extras {
            values: false,
            bounds: true,
            keys: false,
        };
        let above = join_all(chain.iter().map(|o| self.atspi.node(o, shown, 0))).await;
        let inside = |n: &Node| {
            n.role() == "tab"
                || n.bounds.is_none_or(|[bx, by, bw, bh]| {
                    (bx..bx + bw).contains(&cx) && (by..by + bh).contains(&cy)
                })
        };
        if !above.iter().flatten().all(inside) {
            return None;
        }
        let hit = self.atspi.at_point(chain.first()?, cx, cy).await.ok()??;
        if &hit == obj
            || chain.contains(&hit)
            || self.atspi.ancestors(&hit).await.ok()?.contains(obj)
        {
            return Some((cx, cy));
        }
        None
    }

    /// One element action: `click` at its centre, `action` (its own default action), `select` or `focus`.
    async fn act(&self, cua: &Cua, a: ActArgs) -> Result<String, String> {
        let obj = Obj::parse(&a.id)?;
        let ok = match a.action.as_str() {
            "action" => self.atspi.do_action(&obj).await?,
            "select" => self.atspi.select(&obj).await?,
            "focus" => self.atspi.grab_focus(&obj).await?,
            "click" => {
                let shown = Extras {
                    values: false,
                    bounds: true,
                    keys: false,
                };
                let n = self.atspi.node(&obj, shown, 0).await?;
                let [x, y, w, h] = n.bounds.ok_or("the element has no screen position")?;
                cua.desk_input("click", json!({"x": x + w / 2, "y": y + h / 2}))
                    .await?;
                true
            }
            other => {
                return Err(format!(
                    "unknown action {other}; use click, action, select or focus"
                ));
            }
        };
        Ok(json!({"ok": ok}).to_string())
    }

    /// The element of the focused window at the screen point `(x, y)`, as the `at` op answers.
    pub async fn at_point(&self, x: i32, y: i32) -> Result<Value, String> {
        let text = match &self.shuttle {
            Some(shuttle) => {
                let call = Call {
                    agent_id: String::new(),
                    tool: "desk_at".to_owned(),
                    args: json!({"x": x, "y": y}),
                };
                let out = shuttle.desk(&call, super::rpc::DESK_DEADLINE).await?;
                if out.is_error {
                    return Err(out.text);
                }
                out.text
            }
            None => {
                self.at(AtArgs {
                    x,
                    y,
                    window: None,
                    pid: None,
                })
                .await?
            }
        };
        serde_json::from_str(&text).map_err(|e| format!("at: {e}"))
    }

    /// The element at the screen point `(x, y)` of a window, with the names and roles above it.
    async fn at(&self, a: AtArgs) -> Result<String, String> {
        let frame = window::pick(self.frames().await?, a.window.as_deref(), a.pid)?;
        let Some(hit) = self.atspi.at_point(&frame.obj, a.x, a.y).await? else {
            return Ok(json!({"found": null, "window": frame_json(&frame)}).to_string());
        };
        let (node, above) = tokio::join!(
            self.atspi.node(&hit, Extras::default(), 0),
            self.atspi.ancestors(&hit)
        );
        let mut path = Vec::new();
        for o in above? {
            if let Ok(n) = self.atspi.node(&o, Extras::default(), 0).await {
                path.push(json!({"id": o.id(), "role": n.role(), "name": n.name}));
            }
        }
        Ok(json!({"found": node_json((0, &node?)), "path": path}).to_string())
    }

    /// Waits, at most [`TYPE_SETTLE`], until the focused element of the focused window holds the end of
    /// the last line typed, since an app reads typed keys after the input call returns.
    async fn typed(&self, text: &str) {
        let Some(tail) = typed_tail(text) else {
            return;
        };
        let _ = tokio::time::timeout(TYPE_SETTLE, async {
            let frame = window::pick(self.frames().await?, None, None)?;
            let tree = self.atspi.tree(frame, Extras::default()).await?;
            let Some((obj, ifaces)) = tree
                .nodes
                .into_iter()
                .find(|n| n.has(FOCUSED))
                .and_then(|n| Some((n.obj?, n.ifaces)))
            else {
                return Ok(());
            };
            while !self
                .atspi
                .value(&obj, ifaces, READ_CHARS)
                .await?
                .contains(&tail)
            {
                tokio::time::sleep(TYPE_POLL).await;
            }
            Ok::<(), String>(())
        })
        .await;
    }

    /// The window that `a` names by its app part, for a call that found no window titled as `a` says.
    async fn by_app(&self, a: &WindowArgs) -> Result<WindowArgs, String> {
        let frames: Vec<Frame> = self
            .frames()
            .await?
            .into_iter()
            .filter(|f| FRAME_ROLES.contains(&f.role) && a.pid.is_none_or(|p| f.pid == Some(p)))
            .collect();
        let f = app_window(&frames, &a.window)?;
        Ok(WindowArgs {
            window: f.name,
            pid: f.pid,
        })
    }

    async fn frames(&self) -> Result<Vec<Frame>, String> {
        match self.atspi.frames().await {
            Ok(frames) => Ok(frames),
            Err(e) => {
                self.atspi.reset().await;
                Err(e)
            }
        }
    }
}

impl Window for Frame {
    fn title(&self) -> &str {
        &self.name
    }

    fn active(&self) -> bool {
        Frame::active(self)
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn listed(&self) -> bool {
        FRAME_ROLES.contains(&self.role)
    }

    fn focusable(&self) -> bool {
        self.role != WINDOW_ROLE || !self.name.is_empty()
    }
}

impl Named for Node {
    fn name(&self) -> &str {
        &self.name
    }

    fn role(&self) -> &str {
        Node::role(self)
    }
}

/// Presses a key or a combination such as `ctrl+shift+s` on the desktop.
async fn key(cua: &Cua, combo: &str) -> Result<String, String> {
    let keys = key_names(combo)?;
    match keys.as_slice() {
        [one] => cua.desk_input("press_key", json!({"key": one})).await?,
        _ => cua.desk_input("hotkey", json!({"keys": keys})).await?,
    };
    Ok(json!({"ok": true}).to_string())
}

/// Types `text` on the desktop; each newline is a Return key.
async fn type_text(cua: &Cua, text: &str) -> Result<String, String> {
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            cua.desk_input("press_key", json!({"key": "return"}))
                .await?;
        }
        let line = line.trim_end_matches('\r');
        if !line.is_empty() {
            cua.desk_type(line).await?;
        }
    }
    Ok(json!({"ok": true, "chars": text.chars().count()}).to_string())
}

async fn click(cua: &Cua, a: ClickArgs) -> Result<String, String> {
    let mut req = json!({"x": a.x, "y": a.y});
    if let Some(button) = a.button {
        req["button"] = button.into();
    }
    if let Some(count) = a.count {
        req["count"] = count.into();
    }
    cua.desk_input("click", req).await?;
    Ok(json!({"ok": true}).to_string())
}

/// Brings the window titled `window` to the front.
async fn focus(cua: &Cua, a: &WindowArgs) -> Result<String, String> {
    let listed = cua.desk_call("list_windows", json!({})).await?;
    let windows = listed["structuredContent"]["windows"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let titled = |w: &Value| w["title"].as_str().unwrap_or_default().to_owned();
    let pid_ok = |w: &Value| {
        a.pid
            .is_none_or(|p| w["pid"].as_u64() == Some(u64::from(p)))
    };
    let w = (0..TITLE_TIERS)
        .find_map(|tier| {
            windows
                .iter()
                .rev()
                .find(|w| pid_ok(w) && title_matches(tier, &titled(w), &a.window))
        })
        .ok_or_else(|| format!("no window {:?}", a.window))?;
    cua.desk_call(
        "bring_to_front",
        json!({"pid": w["pid"], "window_id": w["window_id"]}),
    )
    .await?;
    Ok(json!({"ok": true, "title": titled(w), "pid": w["pid"]}).to_string())
}

/// Lists the windows until they differ from `since` or `limit` of wall time passes, list reads included, 5 s at most.
pub(super) async fn wait_for<F, Fut>(
    since: Value,
    limit: Duration,
    mut read: F,
) -> Result<Value, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let started = tokio::time::Instant::now();
    let limit = limit.min(WAIT_MAX);
    let mut last = Duration::ZERO;
    loop {
        tokio::time::sleep(
            last.max(WAIT_POLL)
                .min(limit.saturating_sub(started.elapsed())),
        )
        .await;
        let at = tokio::time::Instant::now();
        let windows = read().await?;
        last = at.elapsed();
        let same = windows == since;
        if !same || started.elapsed() >= limit {
            return Ok(json!({"changed": !same, "windows": windows}));
        }
    }
}

/// Asks the window manager to close the window titled `window`, as its close button does.
fn close(a: &WindowArgs) -> Result<String, String> {
    let (conn, n) = x11rb::connect(None).map_err(|e| format!("connect X: {e}"))?;
    let root = conn.setup().roots[n].root;
    let atom = |name: &[u8]| -> Result<u32, String> {
        conn.intern_atom(false, name)
            .map_err(|e| format!("intern atom: {e}"))?
            .reply()
            .map(|r| r.atom)
            .map_err(|e| format!("intern atom: {e}"))
    };
    let [list, name, utf8, wm_pid, close] = [
        &b"_NET_CLIENT_LIST"[..],
        b"_NET_WM_NAME",
        b"UTF8_STRING",
        b"_NET_WM_PID",
        b"_NET_CLOSE_WINDOW",
    ]
    .map(atom);
    let (list, name, utf8, wm_pid, close) = (list?, name?, utf8?, wm_pid?, close?);
    let prop = |w: u32, p: u32, kind: u32| {
        conn.get_property(false, w, p, kind, 0, 1024)
            .ok()
            .and_then(|c| c.reply().ok())
    };
    let clients: Vec<u32> = prop(root, list, AtomEnum::WINDOW.into())
        .and_then(|r| r.value32().map(Iterator::collect))
        .unwrap_or_default();
    let windows: Vec<(u32, String, Option<u32>)> = clients
        .into_iter()
        .map(|w| {
            let title = prop(w, name, utf8)
                .map(|r| String::from_utf8_lossy(&r.value).into_owned())
                .unwrap_or_default();
            let pid = prop(w, wm_pid, AtomEnum::CARDINAL.into())
                .and_then(|r| r.value32().and_then(|mut v| v.next()));
            (w, title, pid)
        })
        .filter(|(_, _, pid)| a.pid.is_none_or(|p| *pid == Some(p)))
        .collect();
    let (w, title, _) = (0..TITLE_TIERS)
        .find_map(|tier| {
            windows
                .iter()
                .rev()
                .find(|(_, t, _)| title_matches(tier, t, &a.window))
        })
        .ok_or_else(|| format!("no window {:?}", a.window))?;
    let event = ClientMessageEvent::new(32, *w, close, [0, 2, 0, 0, 0]);
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )
    .map_err(|e| format!("close {title:?}: {e}"))?;
    conn.flush().map_err(|e| format!("close {title:?}: {e}"))?;
    Ok(json!({"ok": true, "title": title}).to_string())
}

/// A PNG of a screen rectangle, the whole screen by default, written to `path` when given.
async fn shot(cua: &Cua, a: ShotArgs) -> Result<Output, String> {
    let rect = [
        a.x.unwrap_or(0.0),
        a.y.unwrap_or(0.0),
        a.w.unwrap_or(f64::MAX / 4.0),
        a.h.unwrap_or(f64::MAX / 4.0),
    ];
    let (w, h, png) = cua.region_png(rect)?;
    let image = B64.encode(&png);
    let mut out = json!({"ok": true, "width": w, "height": h});
    match a.path {
        Some(path) => {
            files::write_bytes(std::path::Path::new(&path), &png).await?;
            out["path"] = path.into();
        }
        None => out["png_b64"] = image.clone().into(),
    }
    Ok(Output {
        text: out.to_string(),
        image_b64: Some(image),
        ..Output::default()
    })
}

/// The driver's key names of `combo`: `ctrl+shift+s` is `["ctrl", "shift", "s"]`, an X keysym or
/// browser alias is the driver's name (`Prior` is `pageup`), and a trailing `++` is the plus key.
pub(super) fn key_names(combo: &str) -> Result<Vec<String>, String> {
    let mut parts: Vec<&str> = combo.split('+').collect();
    if parts.ends_with(&["", ""]) {
        parts.truncate(parts.len() - 2);
        parts.push("plus");
    }
    let keys: Vec<String> = parts
        .iter()
        .map(|k| {
            let k = k.trim().to_lowercase();
            KEY_ALIASES
                .iter()
                .find(|(alias, _)| *alias == k)
                .map_or(k, |(_, name)| (*name).to_owned())
        })
        .collect();
    if keys.iter().any(String::is_empty) {
        return Err(format!("bad key combination {combo:?}"));
    }
    Ok(keys)
}

/// The last characters of the last line of `text`, which a field holds once it has read the typing.
fn typed_tail(text: &str) -> Option<String> {
    let line = text.rsplit('\n').next()?.trim_end_matches('\r');
    let n = line.chars().count();
    (n > 0).then(|| line.chars().skip(n.saturating_sub(TYPE_TAIL)).collect())
}

fn text_of(v: &Value) -> String {
    v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `cocoonbox-agent desk VERB [key=value ...|JSON]`: calls the toolhost's `desk_VERB` and prints its result.
pub async fn cli(args: Vec<String>) -> u8 {
    let Some((verb, rest)) = args.split_first() else {
        eprintln!("{CLI_USAGE}");
        return 2;
    };
    let args = match cli_args(rest) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("desk: {e}\n{CLI_USAGE}");
            return 2;
        }
    };
    let socket = std::env::var("TOOLHOST_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SOCKET.to_owned());
    let call = Call {
        agent_id: "desk".to_owned(),
        tool: format!("desk_{verb}"),
        args,
    };
    match Client::new(socket.into()).call(&call).await {
        Ok(out) if !out.is_error => {
            println!("{}", out.text);
            0
        }
        Ok(out) => {
            eprintln!("desk {verb}: {}", out.text);
            1
        }
        Err(e) => {
            eprintln!("desk {verb}: {e}");
            1
        }
    }
}

fn frame_json(f: &Frame) -> Value {
    json!({
        "id": f.obj.id(),
        "title": f.name,
        "role": super::atspi::role_name(f.role, true),
        "pid": f.pid,
        "active": f.active(),
    })
}

fn node_json((i, n): (usize, &Node)) -> Value {
    let mut v = json!({
        "ref": i,
        "id": n.obj.as_ref().map(Obj::id),
        "parent": n.parent.map_or(-1, |p| p as i64),
        "depth": n.depth,
        "role": n.role(),
        "name": n.name,
        "states": n.state_names(),
    });
    if !n.description.is_empty() {
        v["description"] = n.description.clone().into();
    }
    if let Some(value) = &n.value {
        v["value"] = value.clone().into();
    }
    if let Some(native) = n.native_role() {
        v["native_role"] = native.into();
    }
    if let Some(keys) = &n.keys {
        let (mnemonic, shortcut) = key_combos(keys);
        if let Some(m) = mnemonic {
            v["mnemonic"] = m.into();
        }
        if let Some(s) = shortcut {
            v["shortcut"] = s.into();
        }
        v["keys"] = keys.clone().into();
    }
    if let Some(b) = n.bounds {
        v["bounds"] = json!(b);
    }
    v
}

/// One line per node: `- role "name" [disabled] [checked] [keys] [id=ID]: "value"`, indented by depth.
pub fn outline(nodes: &[Node], max_chars: usize) -> String {
    let mut out = String::new();
    for n in nodes {
        let _ = write!(out, "{}- {}", "  ".repeat(n.depth), n.role());
        if !n.name.is_empty() {
            let _ = write!(out, " \"{}\"", quote(&n.name, max_chars));
        }
        if n.disabled() {
            out.push_str(" [disabled]");
        }
        if n.checked() {
            out.push_str(" [checked]");
        }
        if let Some(keys) = &n.keys {
            let _ = write!(out, " [{keys}]");
        }
        if let Some(obj) = &n.obj {
            let _ = write!(out, " [id={}]", obj.id());
        }
        if let Some(value) = &n.value {
            let _ = write!(out, ": \"{}\"", quote(value, max_chars));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(name: &str, role: u32, active: bool, pid: u32) -> Frame {
        Frame {
            obj: Obj {
                bus: ":1.1".into(),
                path: format!("/org/a11y/atspi/accessible/{name}"),
            },
            role,
            name: name.into(),
            states: if active { 1 << 1 } else { 0 },
            pid: Some(pid),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_answers_on_the_first_list_that_differs_within_tens_of_ms() {
        let since = json!([{"title": "Untitled - Mousepad"}]);
        let lists = std::cell::Cell::new(0);
        let started = tokio::time::Instant::now();
        let out = wait_for(since.clone(), Duration::from_secs(4), || {
            lists.set(lists.get() + 1);
            let n = lists.get();
            let since = since.clone();
            async move {
                Ok(if n < 3 {
                    since
                } else {
                    json!([{"title": "Save As"}, {"title": "Untitled - Mousepad"}])
                })
            }
        })
        .await
        .unwrap();
        assert_eq!(out["changed"], true);
        assert_eq!(out["windows"][0]["title"], "Save As");
        assert_eq!(lists.get(), 3);
        assert!(
            started.elapsed() < Duration::from_millis(80),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_wait_with_no_change_ends_at_its_timeout_with_the_same_list() {
        let since = json!([{"title": "Desktop"}]);
        let started = Instant::now();
        let out = wait_for(since.clone(), Duration::from_millis(300), || {
            let since = since.clone();
            async move { Ok(since) }
        })
        .await
        .unwrap();
        let t = started.elapsed();
        assert_eq!(out["changed"], false);
        assert_eq!(out["windows"], since);
        assert!(
            t >= Duration::from_millis(300) && t < Duration::from_millis(600),
            "{t:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_wait_whose_lists_take_a_millisecond_ends_at_its_timeout_in_wall_time() {
        let since = json!([{"title": "user - Thunar"}]);
        let started = tokio::time::Instant::now();
        let out = wait_for(since.clone(), Duration::from_millis(480), || {
            let since = since.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Ok(since)
            }
        })
        .await
        .unwrap();
        let t = started.elapsed();
        assert_eq!(out["changed"], false);
        assert!(
            t >= Duration::from_millis(480) && t <= Duration::from_millis(491),
            "{t:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_timeout_reads_the_list_once_at_once() {
        let since = json!([{"title": "Desktop"}]);
        let lists = std::cell::Cell::new(0);
        let started = tokio::time::Instant::now();
        let out = wait_for(since.clone(), Duration::ZERO, || {
            lists.set(lists.get() + 1);
            let since = since.clone();
            async move { Ok(since) }
        })
        .await
        .unwrap();
        assert_eq!(out["changed"], false);
        assert_eq!(lists.get(), 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn after_a_slow_first_read_the_last_read_starts_at_the_deadline() {
        let since = json!([{"title": "Desktop"}]);
        let lists = std::cell::Cell::new(0);
        let started = tokio::time::Instant::now();
        let out = wait_for(since.clone(), Duration::from_millis(150), || {
            lists.set(lists.get() + 1);
            let slow = lists.get() == 1;
            let since = since.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(if slow { 100 } else { 1 })).await;
                Ok(since)
            }
        })
        .await
        .unwrap();
        assert_eq!(out["changed"], false);
        assert_eq!(lists.get(), 2);
        assert_eq!(started.elapsed(), Duration::from_millis(151));
    }

    #[tokio::test]
    async fn slow_lists_count_against_the_timeout() {
        let since = json!([]);
        let started = Instant::now();
        let out = wait_for(since.clone(), Duration::from_millis(200), || {
            let since = since.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                Ok(since)
            }
        })
        .await
        .unwrap();
        let t = started.elapsed();
        assert_eq!(out["changed"], false);
        assert!(
            t >= Duration::from_millis(200) && t < Duration::from_millis(900),
            "{t:?}"
        );
    }

    #[test]
    fn an_empty_name_finds_no_element_not_the_unnamed_ones() {
        let node = |name: &str| Node {
            name: name.into(),
            ..Node::default()
        };
        let nodes = [node(""), node("Save"), node("")];
        assert!(matches(&nodes, "", None).is_empty());
        assert!(matches(&nodes, "...", None).is_empty());
        assert_eq!(matches(&nodes, "save", None), [1]);
    }

    #[test]
    fn a_window_is_picked_by_exact_then_trimmed_then_contained_title() {
        let frames = vec![
            frame("Untitled 1 - Mousepad", 23, false, 7),
            frame("Mousepad Preferences", 16, true, 7),
            frame("Settings", 16, false, 9),
            frame("xfce4-panel", 35, true, 3),
        ];
        assert_eq!(
            window::pick(frames.clone(), None, None).unwrap().name,
            "Mousepad Preferences"
        );
        assert_eq!(
            window::pick(frames.clone(), Some("settings"), None)
                .unwrap()
                .name,
            "Settings"
        );
        assert_eq!(
            window::pick(frames.clone(), Some("Mousepad"), None)
                .unwrap()
                .name,
            "Mousepad Preferences"
        );
        assert_eq!(
            window::pick(frames.clone(), Some("Mousepad"), Some(7))
                .unwrap()
                .name,
            "Mousepad Preferences"
        );
        assert!(window::pick(frames.clone(), Some("Mousepad"), Some(9)).is_err());
        assert!(window::pick(frames, Some("xfce4-panel"), None).is_err());
    }

    #[test]
    fn a_stale_title_finds_the_window_of_its_app_unless_one_matches() {
        let frames = vec![
            frame("images - Thunar", 23, false, 4),
            frame("docs - Thunar", 23, true, 4),
            frame("notes.txt - Mousepad", 23, false, 7),
            frame("Mousepad Preferences", 16, false, 7),
        ];
        let named = |want: &str| window::pick(frames.clone(), Some(want), None).map(|f| f.name);
        assert_eq!(named("inbox - Thunar").unwrap(), "docs - Thunar");
        assert_eq!(named("images - Thunar").unwrap(), "images - Thunar");
        assert_eq!(
            named("Untitled 5 - Mousepad").unwrap(),
            "notes.txt - Mousepad"
        );
        assert_eq!(
            named("*notes.txt - Mousepad").unwrap(),
            "notes.txt - Mousepad"
        );
        assert_eq!(named("Mousepad").unwrap(), "Mousepad Preferences");
        assert!(named("report - Writer").is_err());
        assert!(named("Thunar Preferences").is_err());
    }

    #[test]
    fn a_stale_title_takes_its_app_window_only_when_that_is_unambiguous() {
        let named =
            |frames: Vec<Frame>, want: &str| window::pick(frames, Some(want), None).map(|f| f.name);
        let one = vec![
            frame("images - Thunar", 23, false, 4),
            frame("notes.txt - Mousepad", 23, true, 7),
        ];
        assert_eq!(named(one, "inbox - Thunar").unwrap(), "images - Thunar");
        let active = vec![
            frame("images - Thunar", 23, false, 4),
            frame("docs - Thunar", 23, true, 5),
        ];
        assert_eq!(named(active, "inbox - Thunar").unwrap(), "docs - Thunar");
        let none_active = vec![
            frame("images - Thunar", 23, false, 4),
            frame("docs - Thunar", 23, false, 5),
            frame("notes.txt - Mousepad", 23, true, 7),
        ];
        assert_eq!(
            named(none_active, "inbox - Thunar").unwrap_err(),
            "no window \"inbox - Thunar\"; windows of its app: [\"images - Thunar\", \"docs - Thunar\"]"
        );
    }

    #[test]
    fn typing_waits_for_the_end_of_its_last_line() {
        assert_eq!(typed_tail("hello").as_deref(), Some("hello"));
        assert_eq!(
            typed_tail("a\r\nsecond line").as_deref(),
            Some("second line")
        );
        assert_eq!(
            typed_tail("/home/user/Desktop/report/final/web.txt").as_deref(),
            Some("rt/final/web.txt")
        );
        assert_eq!(typed_tail("café 草稿").as_deref(), Some("café 草稿"));
        assert_eq!(typed_tail("path\n"), None);
        assert_eq!(typed_tail(""), None);
    }

    #[test]
    fn key_combinations_use_the_driver_names() {
        assert_eq!(key_names("Ctrl+Shift+S").unwrap(), ["ctrl", "shift", "s"]);
        assert_eq!(key_names("enter").unwrap(), ["return"]);
        assert_eq!(key_names("esc").unwrap(), ["escape"]);
        assert_eq!(key_names("CTRL+l").unwrap(), ["ctrl", "l"]);
        assert_eq!(key_names("Prior").unwrap(), ["pageup"]);
        assert_eq!(key_names("Next").unwrap(), ["pagedown"]);
        assert_eq!(key_names("Return").unwrap(), ["return"]);
        assert_eq!(key_names("KP_Enter").unwrap(), ["return"]);
        assert_eq!(
            key_names("Control_L+Shift_R+T").unwrap(),
            ["ctrl", "shift", "t"]
        );
        assert_eq!(key_names("ctrlleft+winleft").unwrap(), ["ctrl", "super"]);
        assert_eq!(key_names("ArrowDown").unwrap(), ["down"]);
        assert_eq!(key_names("BackSpace").unwrap(), ["backspace"]);
        assert_eq!(key_names("ctrl++").unwrap(), ["ctrl", "plus"]);
        assert_eq!(key_names("+").unwrap(), ["plus"]);
        assert!(key_names("ctrl+").is_err());
        assert!(key_names("").is_err());
    }

    #[test]
    fn the_outline_shows_states_keys_ids_and_values() {
        let obj = |n: u32| {
            Some(Obj {
                bus: ":1.17".into(),
                path: format!("/org/a11y/atspi/accessible/{n}"),
            })
        };
        let nodes = vec![
            Node {
                obj: obj(5),
                role: 16,
                name: "Prefs".into(),
                states: 1 << 8,
                ..Node::default()
            },
            Node {
                obj: obj(62),
                role: 7,
                name: "Wrap \"long\" lines".into(),
                states: (1 << 8) | (1 << 4),
                depth: 1,
                parent: Some(0),
                ..Node::default()
            },
            Node {
                role: 52,
                states: 0,
                depth: 1,
                parent: Some(0),
                value: Some("80".into()),
                keys: Some("<Alt>n".into()),
                ..Node::default()
            },
        ];
        assert_eq!(
            outline(&nodes, 80),
            "- dialog \"Prefs\" [id=1.17/5]\n  - checkbox \"Wrap \\\"long\\\" lines\" [checked] [id=1.17/62]\n  - spinbutton [disabled] [<Alt>n]: \"80\"\n"
        );
    }
}
