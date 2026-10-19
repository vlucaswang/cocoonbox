//! The desk on shuttle: its point lookups for grounding, the `desk` command, and the driver's key names.

use serde_json::{Value, json};

use desk_core::text::cli_args;

use super::rpc::{Call, Client};

const DEFAULT_SOCKET: &str = "/run/cocoonbox/toolhost.sock";
const CLI_USAGE: &str = "usage: desk VERB [key=value ...|JSON]; verbs: windows, snapshot, find, read, press, act, at, key, type, click, focus, close, launch, shot, screen";
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
    use super::*;

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
