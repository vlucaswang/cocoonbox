//! The desk on shuttle: its point lookups for grounding, the `desk` command, the window-list wait of the
//! Android desk, and the driver's key names.

use std::time::Duration;

use serde_json::{Value, json};

use desk_core::text::cli_args;

use super::rpc::{Call, Client};

const DEFAULT_SOCKET: &str = "/run/cocoonbox/toolhost.sock";
const CLI_USAGE: &str = "usage: desk VERB [key=value ...|JSON]; verbs: windows, snapshot, find, read, press, act, at, key, type, click, focus, close, launch, shot, screen";
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

/// The desk shuttle serves, when the box runs it.
pub struct Desk {
    shuttle: Option<Client>,
}

impl Desk {
    /// A desk whose point lookups go to `shuttle`.
    pub fn new(shuttle: Option<Client>) -> Self {
        Self { shuttle }
    }

    /// The element of the focused window at the screen point `(x, y)`, as the `at` op answers.
    pub async fn at_point(&self, x: i32, y: i32) -> Result<Value, String> {
        let shuttle = self.shuttle.as_ref().ok_or("no desk on this box")?;
        let call = Call {
            agent_id: String::new(),
            tool: "desk_at".to_owned(),
            args: json!({"x": x, "y": y}),
        };
        let out = shuttle.desk(&call, super::rpc::DESK_DEADLINE).await?;
        if out.is_error {
            return Err(out.text);
        }
        serde_json::from_str(&out.text).map_err(|e| format!("at: {e}"))
    }
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

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
}
