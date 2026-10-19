//! Chrome over CDP: page snapshots as ref-tagged element lists, actions by ref.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zeroize::Zeroizing;

use super::{rpc, timing};
use crate::sys;

/// The `browser_*` tools [`Browser::run`] answers; any other is refused before Chrome starts.
pub(super) const TOOLS: [&str; 12] = [
    "browser_close",
    "browser_start",
    "browser_end",
    "browser_fetch",
    "browser_tabs",
    "browser_navigate",
    "browser_snapshot",
    "browser_show",
    "browser_click",
    "browser_fill",
    "browser_type",
    "browser_read",
];
const LAUNCH_WAIT: Duration = Duration::from_secs(60);
const LOAD_WAIT: Duration = Duration::from_secs(15);
const CHALLENGE_WAIT: Duration = Duration::from_secs(8);
const SETTLE: Duration = Duration::from_secs(2);
const TAB_WAIT: Duration = Duration::from_secs(2);
const SPA_WAIT: Duration = Duration::from_secs(3);
const SPA_POLL: Duration = Duration::from_millis(150);
const XHR_STALE: Duration = Duration::from_millis(1500);
const CLOSE_WAIT: Duration = Duration::from_secs(2);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const ENABLE_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(250);
const NAV_START: Duration = Duration::from_millis(200);
const POKE: Duration = Duration::from_millis(100);
const READ_CHARS: usize = 20_000;
const SHELL_CHARS: usize = 300;
const PAGE_TYPES: [&str; 7] = [
    "",
    "text/html",
    "text/plain",
    "text/xml",
    "application/xhtml+xml",
    "application/xml",
    "application/json",
];
const RUNNING_MARKER: &str = "cocoonbox-was-running";
const AGENT_MARKER: &str = "cocoonbox-agent-windows";
const AGENT_PAGE: &str = "about:blank#cocoonbox-agent";
const XPROP: &str = "xprop";
const XPROP_WAIT: Duration = Duration::from_secs(2);
const CHROME_APPS: [&str; 2] = ["Google Chrome", "Chromium"];
const MAX_WINDOWS: usize = 8;
const NO_PAGE_FOCUS: &str = "focus a card field on the checkout page first: the keyboard focus is in no web page of the agent's Chrome, so it may be in the address bar, a find bar, DevTools, another app or a Chrome the browser tools do not drive; use browser_navigate to open the checkout in the agent's Chrome, then focus the card field; nothing was typed";
const SHARE_TAB: &str = "a read-only share reads only its own window, so it takes no tab";
const SHARE_WINDOW: &str = "a read-only share opens no Chrome window: it reads only the window its session opened with write";
const TEXT_LEN_JS: &str = "document.body ? document.body.textContent.length : 0";
const TAB_JS: &str = "[document.visibilityState, document.title, location.href]";
const NO_ANSWER: &str = "the page does not answer, so it may show a dialog that opened before this call: close the dialog on the screen and retry";
const NOT_A_FIELD: &str = "focus a card field on the checkout page first: the focused element of the page is not a text field; nothing was typed";

const SNAPSHOT_JS: &str = r#"((plans) => {
  const MAX = 300;
  const refs = new Map();
  window.__cbRefs = refs;
  const out = [`Page: ${document.title} (${location.href})`];
  let truncated = false;
  let headings = 0;
  const ACTIVE_TAGS = new Set(['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'SUMMARY']);
  const ACTIVE_ROLES = new Set(['button', 'link', 'checkbox', 'radio', 'tab', 'menuitem',
    'menuitemcheckbox', 'menuitemradio', 'combobox', 'option', 'switch', 'searchbox',
    'textbox', 'slider', 'spinbutton', 'treeitem']);
  const TEXT_TAGS = new Set(['P', 'LI', 'TD', 'TH', 'LABEL', 'DT', 'DD', 'CAPTION',
    'FIGCAPTION', 'BLOCKQUOTE', 'PRE']);
  const SKIP = new Set(['SCRIPT', 'STYLE', 'NOSCRIPT', 'TEMPLATE', 'svg', 'IFRAME']);
  const CARD = /\bcc-(number|csc|exp)|card.?(number|num|no\b)|cvc|cvv|csc|security.?code/i;
  const clip = (s, n) => {
    s = (s || '').replace(/\s+/g, ' ').trim();
    return s.length > n ? s.slice(0, n - 1) + '…' : s;
  };
  const shown = (el) => {
    if (el.getAttribute('aria-hidden') === 'true') return null;
    const st = getComputedStyle(el);
    if (st.display === 'none' || st.visibility === 'hidden') return null;
    if (st.display === 'contents') return st;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 ? st : null;
  };
  const active = (el) =>
    (ACTIVE_TAGS.has(el.tagName) && !(el.tagName === 'A' && !el.hasAttribute('href'))
      && !(el.tagName === 'INPUT' && el.type === 'hidden'))
    || ACTIVE_ROLES.has(el.getAttribute('role'))
    || el.isContentEditable && el.hasAttribute('contenteditable')
    || el.hasAttribute('onclick');
  const roleOf = (el) => {
    const r = el.getAttribute('role');
    if (r) return r;
    switch (el.tagName) {
      case 'A': return 'link';
      case 'BUTTON': case 'SUMMARY': return 'button';
      case 'SELECT': return 'combobox';
      case 'TEXTAREA': return 'textbox';
      case 'INPUT':
        return ({ checkbox: 'checkbox', radio: 'radio', submit: 'button', button: 'button',
          reset: 'button', image: 'button', range: 'slider' })[el.type] || 'textbox';
    }
    return el.isContentEditable ? 'textbox' : el.tagName.toLowerCase();
  };
  const ICON = /(?:^|\s)(?:fa|bi|mdi|ti|icon)-(?!solid|regular|light|brands|lg|sm|xs|fw|2x|3x)([a-z][a-z0-9-]*)/;
  const iconName = (el) => {
    const tip = el.getAttribute('data-tip') || el.getAttribute('data-tooltip') || el.getAttribute('data-title');
    if (tip) return tip;
    const inner = el.querySelector('[title],[aria-label]');
    if (inner) return inner.getAttribute('aria-label') || inner.getAttribute('title');
    for (const c of [el, ...el.querySelectorAll('[class]')]) {
      const m = ICON.exec(typeof c.className === 'string' ? c.className : c.getAttribute('class') || '');
      if (m) return m[1].replace(/-/g, ' ');
    }
    return '';
  };
  const nameOf = (el) => clip(el.getAttribute('aria-label')
    || (el.labels && el.labels[0] && el.labels[0].innerText)
    || el.getAttribute('placeholder') || el.getAttribute('alt') || el.getAttribute('title')
    || (el.tagName === 'INPUT' ? '' : el.innerText) || (plans ? iconName(el) : '') || '', 80);
  const named = [];
  const pad = (d) => '  '.repeat(Math.min(d, 8));
  const walk = (root, depth, inside = false, pointer = false) => {
    for (const el of root.children) {
      if (out.length > MAX) { truncated = true; return; }
      const st = SKIP.has(el.tagName) ? null : shown(el);
      if (!st) continue;
      const act = active(el);
      const clickable = plans && !act && !inside && !pointer && st.cursor === 'pointer';
      if (act || clickable) {
        const ref = 'e' + (refs.size + 1);
        refs.set(ref, el);
        const role = clickable ? 'clickable' : roleOf(el);
        const name = nameOf(el);
        if (plans) named.push({ i: out.length, el, key: `${role}\u0000${name}`, name });
        let line = `${pad(depth)}- ${role} "${name}" [ref=${ref}]`;
        if (el.disabled) line += ' disabled';
        if (el.checked) line += ' checked';
        if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
          if (el.type === 'password') line += ' value=<redacted>';
          else if (el.value && CARD.test([el.autocomplete, el.name, el.id, nameOf(el)].join(' '))) line += ' value="[masked]"';
          else if (el.value) line += ` value="${clip(el.value, 60)}"`;
        }
        if (plans && el.tagName === 'SELECT') line += ` options="${clip([...el.options].filter((o) => !o.disabled).map((o) => o.label).filter(Boolean).join(' | '), 300)}"`;
        const href = el.tagName === 'A' ? el.getAttribute('href') : null;
        if (href && !href.startsWith('javascript:')) line += ` href="${clip(href, 100)}"`;
        out.push(line);
        if (clickable) walk(el, depth + 1, true, true);
        continue;
      }
      if (inside) {
        walk(el, depth + 1, true, true);
        continue;
      }
      const tag = /^H([1-6])$/.exec(el.tagName);
      const level = tag ? tag[1] : el.getAttribute('role') === 'heading' && (el.getAttribute('aria-level') || '2');
      const nested = el.querySelector('a[href],button,input,select,textarea,[role],[contenteditable],[onclick]');
      if (level) {
        headings++;
        out.push(`${pad(depth)}- heading "${clip(el.innerText, 150)}" [level=${level}]`);
        if (nested) walk(el, depth + 1, false, st.cursor === 'pointer');
        continue;
      }
      if (!nested && (TEXT_TAGS.has(el.tagName) || el.children.length === 0)) {
        const t = clip(el.innerText, 200);
        if (t) out.push(`${pad(depth)}- text "${t}"`);
        continue;
      }
      walk(el, depth + 1, false, st.cursor === 'pointer');
      if (el.shadowRoot) walk(el.shadowRoot, depth + 1, false, st.cursor === 'pointer');
    }
  };
  if (document.body) walk(document.body, 0);
  const refd = new Set(refs.values());
  const groups = new Map();
  for (const n of named) groups.set(n.key, [...(groups.get(n.key) || []), n]);
  for (const same of groups.values()) {
    if (same.length < 2) continue;
    for (const n of same) {
      for (let a = n.el.parentElement; a && a !== document.body; a = a.parentElement) {
        if (same.some((m) => m !== n && a.contains(m.el)) || refd.has(a)) break;
        const t = (a.innerText || '').split('\n').map((x) => x.trim()).find((x) => x && x !== n.name);
        if (t) { out[n.i] += ` in "${clip(t, 60)}"`; break; }
      }
    }
  }
  if (!headings && !truncated) out.splice(1, 0, '(this page has no headings)');
  if (truncated) out.push(`[snapshot truncated at ${MAX} lines; use browser_read for page text]`);
  return out.join('\n');
})"#;

const POINT_JS: &str = r#"((ref) => {
  const el = window.__cbRefs && window.__cbRefs.get(ref);
  if (!el || !el.isConnected) throw new Error(`unknown or stale ref ${ref}; take a new browser_snapshot`);
  el.scrollIntoView({ block: 'center', inline: 'center' });
  const r = el.getBoundingClientRect();
  return { x: r.left + r.width / 2, y: r.top + r.height / 2,
    label: (el.getAttribute('aria-label') || el.innerText
      || (el.tagName === 'INPUT' && ['submit', 'button', 'reset'].includes(el.type) ? el.value : '')
      || el.getAttribute('placeholder') || el.getAttribute('name') || el.tagName).replace(/\s+/g, ' ').trim().slice(0, 60) };
})"#;

const FOCUS_JS: &str = r#"((ref, clear) => {
  const el = window.__cbRefs && window.__cbRefs.get(ref);
  if (!el || !el.isConnected) throw new Error(`unknown or stale ref ${ref}; take a new browser_snapshot`);
  el.scrollIntoView({ block: 'center' });
  el.focus();
  if (clear) {
    if (typeof el.select === 'function') el.select();
    else document.execCommand('selectAll');
  }
  return true;
})"#;

const FIELD_JS: &str = r#"(() => {
  if (!document.hasFocus()) return '';
  let el = document.activeElement;
  for (;;) {
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    if (!el || (el.tagName !== 'IFRAME' && el.tagName !== 'FRAME')) break;
    if (!el.contentDocument) return 'frame';
    el = el.contentDocument.activeElement;
  }
  if (!el || el.disabled || el.readOnly) return 'other';
  if (el.isContentEditable || el.tagName === 'TEXTAREA') return 'field';
  return el.tagName === 'INPUT' && ['text', 'tel', 'number', 'password'].includes(el.type) ? 'field' : 'other';
})()"#;

const CHOOSE_JS: &str = r#"((ref, text) => {
  const el = window.__cbRefs && window.__cbRefs.get(ref);
  if (!el || !el.isConnected) throw new Error(`unknown or stale ref ${ref}; take a new browser_snapshot`);
  if (el.tagName !== 'SELECT') return null;
  const norm = (s) => (s || '').replace(/\s+/g, ' ').trim().toLowerCase();
  const want = norm(text);
  const opts = [...el.options].filter((o) => !o.disabled);
  const part = opts.filter((o) => norm(o.label).includes(want));
  const opt = opts.find((o) => norm(o.label) === want) || opts.find((o) => norm(o.value) === want) || (part.length === 1 ? part[0] : null);
  if (!opt) throw new Error(`no option of ${ref} is "${text}"; its options: ${opts.map((o) => o.label).filter(Boolean).join(', ')}`);
  el.scrollIntoView({ block: 'center' });
  el.focus();
  if (el.multiple) opt.selected = true;
  else Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(el, opt.value);
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return opt.label;
})"#;

const READ_JS: &str = r#"((ref) => {
  const el = ref ? window.__cbRefs && window.__cbRefs.get(ref) : document.body;
  if (!el) throw new Error(`unknown or stale ref ${ref}; take a new browser_snapshot`);
  return el.innerText || '';
})"#;

const FILL_JS: &str = r#"(async (origin, username, secret, kind, submit) => {
  if (location.origin !== origin) return { error: `the active tab is on ${location.origin}, not ${origin}` };
  const shown = (el) => {
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';
  };
  const inputs = [...document.querySelectorAll('input')].filter((el) => !el.disabled && !el.readOnly && shown(el));
  const textual = (el) => ['text', 'email', 'tel', 'number'].includes(el.type);
  const userish = (el) => /user|mail|login|account|phone|mobile|账号|帐号|邮箱|手机/i
    .test([el.autocomplete, el.type, el.name, el.id, el.placeholder, el.getAttribute('aria-label')].join(' '));
  const set = (el, v) => {
    el.focus();
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(el, v);
    el.dispatchEvent(new Event('input', { bubbles: true }));
    el.dispatchEvent(new Event('change', { bubbles: true }));
  };
  const focused = inputs.includes(document.activeElement) ? document.activeElement : null;
  const filled = [];
  let last = null;
  if (kind === 'code') {
    const field = (focused && textual(focused) ? focused : null)
      || inputs.find((el) => el.autocomplete === 'one-time-code') || inputs.find(textual);
    if (field) { set(field, secret); filled.push('code'); last = field; }
  } else {
    let pass = focused && focused.type === 'password' ? focused : null;
    let user = focused && textual(focused) ? focused : null;
    if (user && !pass) pass = inputs.slice(inputs.indexOf(user) + 1).find((el) => el.type === 'password') || null;
    if (!user && !pass) pass = inputs.find((el) => el.type === 'password') || null;
    if (pass && !user) user = inputs.slice(0, inputs.indexOf(pass)).filter(textual).pop() || null;
    if (!user && !pass) user = inputs.find((el) => textual(el) && userish(el)) || null;
    if (user && username) { set(user, username); filled.push('username'); last = user; }
    if (pass && secret) { set(pass, secret); filled.push('password'); last = pass; }
  }
  if (!last) return { error: 'no sign-in field on this page' };
  if (!submit) return { filled, submitted: '' };
  await new Promise((r) => setTimeout(r, 100));
  const named = (el) => el.textContent.length < 40
    && /^(登\s*录|登入|log\s*in|sign\s*in|continue|next|submit|verify|下一步|继续|提交|验证|确[定认])$/i
      .test((el.innerText || el.value || el.getAttribute('aria-label') || '').trim());
  const clickable = (el) => !el.disabled && shown(el)
    && (el.matches('button,input,[role=button],a') || getComputedStyle(el).cursor === 'pointer');
  let btn = last.form && [...last.form.elements].find((el) => ['submit', 'image'].includes(el.type) && clickable(el));
  for (let a = last.parentElement; a && !btn; a = a.parentElement) {
    btn = [...a.querySelectorAll('button,input,[role=button],a,div,span')].find((el) =>
      last.compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING && named(el) && clickable(el));
  }
  if (btn) {
    btn.scrollIntoView({ block: 'center', inline: 'center' });
    const r = btn.getBoundingClientRect();
    const x = r.left + r.width / 2, y = r.top + r.height / 2;
    if (btn.contains(document.elementFromPoint(x, y))) return { filled, submitted: 'click', x, y };
  }
  last.focus();
  return { filled, submitted: 'enter' };
})"#;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Owns the Chrome process lifecycle and one window per agent; an agent's calls run in order.
pub struct Browser {
    bin: String,
    profile: PathBuf,
    port: u16,
    http: reqwest::Client,
    life: RwLock<()>,
    /// Held while Chrome starts; true once this toolhost has looked for agent windows an earlier one left.
    start: Mutex<bool>,
    windows: Mutex<HashMap<String, Slot>>,
    last: Mutex<String>,
    fetch_wait: Duration,
    /// A box that runs GUI plans: the tools also act on a tab the user has open, and a navigation waits for content that scripts load.
    plans: bool,
}

impl Browser {
    /// Creates a browser handle; Chrome starts on first use. `plans` turns on the behaviour of a box that runs GUI plans.
    pub fn new(bin: String, profile: PathBuf, port: u16, plans: bool) -> Self {
        Self {
            bin,
            profile,
            port,
            http: reqwest::Client::new(),
            life: RwLock::new(()),
            start: Mutex::new(false),
            windows: Mutex::new(HashMap::new()),
            last: Mutex::new(String::new()),
            fetch_wait: LOAD_WAIT,
            plans,
        }
    }

    /// Dispatches a `browser_*` tool in the window of `agent_id`.
    pub async fn run(&self, agent_id: &str, tool: &str, args: Value) -> Result<String, String> {
        if !TOOLS.contains(&tool) {
            return Err(format!("unknown browser tool {tool}"));
        }
        if tool == "browser_close" {
            let _life = self.life.write().await;
            return self.close().await;
        }
        let _life = self.life.read().await;
        match tool {
            "browser_start" => {
                if args["if_was_running"].as_bool() == Some(true)
                    && tokio::fs::symlink_metadata(self.profile.join(RUNNING_MARKER))
                        .await
                        .is_err()
                {
                    return Ok("not running".to_owned());
                }
                self.running().await?;
                return Ok("running".to_owned());
            }
            "browser_end" => return Ok(self.end(agent_id).await),
            "browser_fetch" => return self.fetch(args).await,
            "browser_tabs" if self.plans => return self.tabs().await,
            _ => {}
        }
        if self.plans
            && let Some(tab) = args["tab"].as_str()
        {
            if args["workspace_only"] == true {
                return Err(SHARE_TAB.to_owned());
            }
            let mut cdp = self.tab(tab).await?;
            timing::mark("connect");
            return self.act(&mut cdp, tool, args).await;
        }
        if args["workspace_only"] == true {
            let slot = self
                .windows
                .lock()
                .await
                .get(agent_id)
                .map(|s| Arc::clone(&s.window));
            let slot = slot.ok_or(SHARE_WINDOW)?;
            let window = slot.lock().await;
            let mut cdp = Cdp::open(&window.as_ref().ok_or(SHARE_WINDOW)?.ws_url).await?;
            return self.act(&mut cdp, tool, args).await;
        }
        let (slot, evicted) = self.slot(agent_id).await;
        if let Some(evicted) = evicted {
            self.shut(evicted).await;
        }
        let mut window = slot.lock().await;
        let mut cdp = self.connect(&mut window).await?;
        timing::mark("connect");
        if let Some(t) = window.as_ref() {
            t.ws_url.clone_into(&mut *self.last.lock().await);
        }
        self.act(&mut cdp, tool, args).await
    }

    /// Runs a page tool of `run` in the tab of `cdp` and reports the dialogs the page opened.
    async fn act(&self, cdp: &mut Cdp, tool: &str, args: Value) -> Result<String, String> {
        cdp.enable_page().await?;
        let mut out = self.page_tool(cdp, tool, args).await?;
        for d in &cdp.dialogs {
            let _ = write!(out, "\n{d}");
        }
        Ok(out)
    }

    async fn page_tool(&self, cdp: &mut Cdp, tool: &str, args: Value) -> Result<String, String> {
        match tool {
            "browser_navigate" => {
                let a: NavigateArgs = rpc::parse(args)?;
                let front = self.plans && a.front;
                let url = web_url(a.url);
                if self.plans {
                    cdp.call("Network.enable", json!({})).await?;
                }
                let nav = cdp.call("Page.navigate", json!({"url": url})).await?;
                timing::mark("navigate");
                if let Some(err) = nav["errorText"].as_str().filter(|e| !e.is_empty()) {
                    return Err(format!("navigate {url}: {err}"));
                }
                cdp.wait_loaded().await;
                timing::mark("load");
                if self.plans {
                    cdp.settle(tokio::time::Instant::now() + SPA_WAIT).await;
                    timing::mark("settle");
                }
                if front {
                    cdp.call("Page.bringToFront", json!({})).await?;
                    timing::mark("front");
                }
                cdp.snapshot(self.plans).await
            }
            "browser_snapshot" => cdp.snapshot(self.plans).await,
            "browser_show" => {
                cdp.call("Page.bringToFront", json!({})).await?;
                let title = cdp.eval("document.title || location.href").await?;
                Ok(title.as_str().unwrap_or_default().to_owned())
            }
            "browser_click" => {
                let a: RefArgs = rpc::parse(args)?;
                cdp.click_ref(&a.element).await
            }
            "browser_fill" if self.plans => {
                let a: FillArgs = rpc::parse(args)?;
                let mut filled = Vec::with_capacity(a.fields.len());
                for f in &a.fields {
                    let chosen = cdp
                        .eval(&format!(
                            "{CHOOSE_JS}({}, {})",
                            js_str(&f.element),
                            js_str(&f.text)
                        ))
                        .await?;
                    if chosen.is_null() {
                        cdp.eval(&format!("{FOCUS_JS}({}, true)", js_str(&f.element)))
                            .await?;
                        cdp.call("Input.insertText", json!({"text": f.text}))
                            .await?;
                    }
                    filled.push(f.element.as_str());
                }
                let mut out = format!("filled {}", filled.join(", "));
                if let Some(element) = &a.click {
                    out = format!("{out}; {}", cdp.click_ref(element).await?);
                }
                Ok(format!("{out}\n{}", cdp.snapshot(true).await?))
            }
            "browser_type" => {
                let a: TypeArgs = rpc::parse(args)?;
                if self.plans {
                    let chosen = cdp
                        .eval(&format!(
                            "{CHOOSE_JS}({}, {})",
                            js_str(&a.element),
                            js_str(&a.text)
                        ))
                        .await?;
                    if let Some(label) = chosen.as_str() {
                        return Ok(format!("selected \"{label}\" in {}", a.element));
                    }
                }
                cdp.eval(&format!("{FOCUS_JS}({}, {})", js_str(&a.element), a.clear))
                    .await?;
                cdp.call("Input.insertText", json!({"text": a.text}))
                    .await?;
                if a.submit {
                    cdp.press_enter().await?;
                }
                Ok(format!(
                    "typed {} characters into {}{}",
                    a.text.chars().count(),
                    a.element,
                    if a.submit { " and pressed Enter" } else { "" }
                ))
            }
            "browser_read" => {
                let a: ReadArgs = rpc::parse(args)?;
                let arg = a
                    .element
                    .as_deref()
                    .map_or_else(|| "null".to_owned(), js_str);
                let text = cdp.eval(&format!("{READ_JS}({arg})")).await?;
                Ok(capped(text.as_str().unwrap_or_default()))
            }
            _ => Err(format!("unknown browser tool {tool}")),
        }
    }

    /// Lists the tabs open outside the agent windows, with their ids for `tab`.
    async fn tabs(&self) -> Result<String, String> {
        let tabs = self.user_tabs().await?;
        if tabs.is_empty() {
            return Ok("no tab is open outside the agent windows".to_owned());
        }
        let mut out = String::from(
            "Tabs the user has open; pass an id, or \"active\" for the one in front, as tab:",
        );
        for t in &tabs {
            let _ = write!(out, "\n{t}");
        }
        Ok(out)
    }

    /// The user's tab `tab`: an id from `browser_tabs`, or "active" for the one tab in front, else the one
    /// of those titled as the X active window; Chrome is not started for it.
    async fn tab(&self, tab: &str) -> Result<Cdp, String> {
        let tabs = self.user_tabs().await?;
        if tab != "active" {
            return tabs
                .into_iter()
                .find(|t| t.id == tab)
                .map(|t| t.cdp)
                .ok_or_else(|| format!("no tab {tab} is open; browser_tabs lists the tabs"));
        }
        let mut shown: Vec<Tab> = tabs.into_iter().filter(|t| t.shown).collect();
        if shown.len() > 1
            && let Some(title) = active_title().await
            && shown.iter().filter(|t| t.title == title).count() == 1
        {
            shown.retain(|t| t.title == title);
        }
        match shown.len() {
            1 => Ok(shown.remove(0).cdp),
            0 => Err("no tab of the user is in front; browser_tabs lists the tabs".to_owned()),
            n => {
                let mut err = format!("{n} tabs are in front, one per window; pass one id as tab:");
                for t in &shown {
                    let _ = write!(err, "\n{t}");
                }
                Err(err)
            }
        }
    }

    /// The page tabs outside the agent windows, which are known by their first history entry.
    async fn user_tabs(&self) -> Result<Vec<Tab>, String> {
        let targets = self
            .targets()
            .await
            .map_err(|_| "Chrome is not running in the box".to_owned())?;
        let mut tabs = Vec::new();
        for t in targets.into_iter().filter(Target::is_tab) {
            let Ok(mut cdp) = Cdp::open(&t.ws_url).await else {
                continue;
            };
            let Ok(history) = cdp.call("Page.getNavigationHistory", json!({})).await else {
                continue;
            };
            if history["entries"][0]["url"] == AGENT_PAGE {
                continue;
            }
            let info = tokio::time::timeout(TAB_WAIT, cdp.eval(TAB_JS))
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or_default();
            tabs.push(Tab {
                shown: info[0] == "visible",
                title: info[1].as_str().unwrap_or_default().to_owned(),
                url: info[2].as_str().unwrap_or(&t.url).to_owned(),
                id: t.id,
                cdp,
            });
        }
        Ok(tabs)
    }

    /// Reads `url` in a hidden tab, out of every window, and closes the tab.
    async fn fetch(&self, args: Value) -> Result<String, String> {
        let a: NavigateArgs = rpc::parse(args)?;
        let url = web_url(a.url);
        self.running().await?;
        let mut cdp = self.browser().await?;
        let created = cdp
            .call(
                "Target.createTarget",
                json!({"url": "about:blank", "hidden": true}),
            )
            .await?;
        let id = created["targetId"].as_str().unwrap_or_default().to_owned();
        timing::mark("create");
        let read = cdp.fetch(&id, &url, self.fetch_wait).await;
        cdp.session = None;
        let _ = cdp
            .call("Target.closeTarget", json!({"targetId": id}))
            .await;
        timing::mark("close");
        read
    }

    /// Origin of the active tab; Chrome is not started for it.
    pub async fn origin(&self) -> Result<String, String> {
        let _life = self.life.read().await;
        let mut cdp = self.attach().await?;
        let origin = cdp.eval("location.origin").await?;
        Ok(origin.as_str().unwrap_or_default().to_owned())
    }

    /// Succeeds when the keyboard focus is in a text field of an http or https page, or in a frame of one that CDP
    /// cannot read; Chrome is not started for it.
    pub async fn page_field(&self) -> Result<(), String> {
        let _life = self.life.read().await;
        let targets = self.targets().await.map_err(|_| NO_PAGE_FOCUS.to_owned())?;
        let mut frame = false;
        for t in targets.iter().filter(|t| t.is_web()) {
            let Ok(mut cdp) = Cdp::open(&t.ws_url).await else {
                continue;
            };
            match cdp
                .eval(FIELD_JS)
                .await
                .ok()
                .as_ref()
                .and_then(Value::as_str)
            {
                Some("field") => return Ok(()),
                Some("other") => return Err(NOT_A_FIELD.to_owned()),
                Some("frame") => frame = true,
                _ => {}
            }
        }
        if frame {
            Ok(())
        } else {
            Err(NO_PAGE_FOCUS.to_owned())
        }
    }

    /// Fills the sign-in form of the active tab when its origin equals `origin`; errors never carry a value.
    pub async fn fill(&self, args: &Value) -> Result<String, String> {
        let origin = args["origin"].as_str().unwrap_or_default();
        if origin.is_empty() || origin == "null" {
            return Err("fill needs the origin of the tab".to_owned());
        }
        let (kind, secret) = match (args["password"].as_str(), args["code"].as_str()) {
            (Some(p), None) => ("password", p),
            (None, Some(c)) => ("code", c),
            _ => return Err("fill needs either a password or a code".to_owned()),
        };
        let username = args["username"].as_str().unwrap_or_default();
        let submit = args["submit"].as_bool().unwrap_or(false);
        let _life = self.life.read().await;
        let mut cdp = self.attach().await?;
        let user = js_secret(username);
        let secret = js_secret(secret);
        let mut expr = Zeroizing::new(String::with_capacity(
            FILL_JS.len() + origin.len() * 6 + user.len() + secret.len() + 64,
        ));
        for part in [FILL_JS, "(", &js_str(origin), ",", &user, ",", &secret, ","] {
            expr.push_str(part);
        }
        let _ = write!(expr, "{},{submit})", js_str(kind));
        let r = cdp.eval(&expr).await?;
        if let Some(e) = r["error"].as_str() {
            return Err(e.to_owned());
        }
        let submitted = match r["submitted"].as_str() {
            Some("click") => {
                cdp.click(
                    r["x"].as_f64().unwrap_or(0.0),
                    r["y"].as_f64().unwrap_or(0.0),
                )
                .await?;
                true
            }
            Some("enter") => {
                cdp.press_enter().await?;
                true
            }
            _ => false,
        };
        Ok(json!({"filled": r["filled"], "submitted": submitted}).to_string())
    }

    /// The tab the user sees in the X active window, else the window the browser tools
    /// last acted on, else the first tab Chrome lists.
    async fn attach(&self) -> Result<Cdp, String> {
        let targets = self
            .targets()
            .await
            .map_err(|_| "Chrome is not running in the box".to_owned())?;
        if let Some(cdp) = self.shown(&targets).await {
            return Ok(cdp);
        }
        let last = self.last.lock().await.clone();
        if !last.is_empty()
            && let Ok(cdp) = Cdp::open(&last).await
        {
            return Ok(cdp);
        }
        let page = targets
            .into_iter()
            .find(Target::is_tab)
            .ok_or("no tab is open in the box's Chrome")?;
        Cdp::open(&page.ws_url).await
    }

    /// The visible tab titled as the X active window, when exactly one matches.
    async fn shown(&self, targets: &[Target]) -> Option<Cdp> {
        let title = active_title().await?;
        let infos = self
            .browser()
            .await
            .ok()?
            .call("Target.getTargets", json!({}))
            .await
            .ok()?;
        let ids: Vec<&str> = infos["targetInfos"]
            .as_array()?
            .iter()
            .filter(|i| i["type"] == "page" && i["title"] == title.as_str())
            .filter_map(|i| i["targetId"].as_str())
            .collect();
        let mut found = None;
        for t in targets.iter().filter(|t| ids.contains(&t.id.as_str())) {
            let Ok(mut cdp) = Cdp::open(&t.ws_url).await else {
                continue;
            };
            if cdp.eval("document.visibilityState").await == Ok(json!("visible"))
                && found.replace(cdp).is_some()
            {
                return None;
            }
        }
        found
    }

    /// Attaches to the window in `window`, opening a new one when it is missing or was closed.
    async fn connect(&self, window: &mut Option<Target>) -> Result<Cdp, String> {
        if let Some(t) = window.as_ref()
            && let Ok(cdp) = Cdp::open(&t.ws_url).await
        {
            return Ok(cdp);
        }
        self.running().await?;
        let created = self
            .browser()
            .await?
            .call(
                "Target.createTarget",
                json!({"url": AGENT_PAGE, "newWindow": true, "background": true}),
            )
            .await?;
        let _ = tokio::fs::write(self.profile.join(AGENT_MARKER), b"").await;
        let id = created["targetId"].as_str().unwrap_or_default();
        let t = self
            .targets()
            .await?
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("new window {id} is not listed"))?;
        let cdp = Cdp::open(&t.ws_url).await?;
        *window = Some(t);
        Ok(cdp)
    }

    /// The window of `agent_id`, and past `MAX_WINDOWS` agents the slot of the least recently used other one, taken out to close.
    async fn slot(&self, agent_id: &str) -> (Arc<Mutex<Option<Target>>>, Option<Slot>) {
        let mut windows = self.windows.lock().await;
        let slot = windows.entry(agent_id.to_owned()).or_insert_with(|| Slot {
            window: Arc::default(),
            used: Instant::now(),
        });
        slot.used = Instant::now();
        let window = Arc::clone(&slot.window);
        if windows.len() <= MAX_WINDOWS {
            return (window, None);
        }
        let oldest = windows
            .iter()
            .filter(|(id, _)| *id != agent_id)
            .min_by_key(|(_, s)| s.used)
            .map(|(id, _)| id.clone());
        (window, oldest.and_then(|id| windows.remove(&id)))
    }

    /// Closes the window of `agent_id`; Chrome is not started for it.
    async fn end(&self, agent_id: &str) -> String {
        let Some(slot) = self.windows.lock().await.remove(agent_id) else {
            return "no window".to_owned();
        };
        self.shut(slot).await.to_owned()
    }

    async fn shut(&self, slot: Slot) -> &'static str {
        let Some(t) = slot.window.lock().await.take() else {
            return "no window";
        };
        if let Ok(mut cdp) = self.browser().await {
            cdp.close_windows(&[&t.id]).await;
        }
        "closed"
    }

    async fn running(&self) -> Result<(), String> {
        let mut swept = self.start.lock().await;
        let launched = self.targets().await.is_err();
        if launched {
            self.launch().await?;
        }
        if (launched || !*swept)
            && tokio::fs::remove_file(self.profile.join(AGENT_MARKER))
                .await
                .is_ok()
        {
            self.close_restored().await;
        }
        *swept = true;
        Ok(())
    }

    async fn browser(&self) -> Result<Cdp, String> {
        let url = format!("http://127.0.0.1:{}/json/version", self.port);
        let version: Value = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|e| format!("get {url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("decode {url}: {e}"))?;
        Cdp::open(version["webSocketDebuggerUrl"].as_str().unwrap_or_default()).await
    }

    /// Closes Chrome through CDP so it flushes its profile; reports whether it was running
    /// and records it in the profile for `browser_start` with `if_was_running`.
    /// Agent windows close first, so the restored session holds only the user's.
    async fn close(&self) -> Result<String, String> {
        let marker = self.profile.join(RUNNING_MARKER);
        let windows: Vec<_> = self
            .windows
            .lock()
            .await
            .drain()
            .map(|(_, s)| s.window)
            .collect();
        if self.targets().await.is_err() {
            let _ = tokio::fs::remove_file(&marker).await;
            return Ok("not running".to_owned());
        }
        let mut ids = Vec::new();
        for w in windows {
            if let Some(t) = w.lock().await.take() {
                ids.push(t.id);
            }
        }
        let mut cdp = self.browser().await?;
        cdp.close_windows(&ids.iter().map(String::as_str).collect::<Vec<_>>())
            .await;
        let _ = tokio::fs::remove_file(self.profile.join(AGENT_MARKER)).await;
        let _ = cdp.call("Browser.close", json!({})).await;
        // the CDP port closes before the profile is flushed; SingletonLock goes last on a clean exit
        let lock = self.profile.join("SingletonLock");
        let deadline = tokio::time::Instant::now() + LAUNCH_WAIT;
        while tokio::time::Instant::now() < deadline {
            if self.targets().await.is_err() && tokio::fs::symlink_metadata(&lock).await.is_err() {
                tokio::fs::write(&marker, b"")
                    .await
                    .map_err(|e| format!("write {}: {e}", marker.display()))?;
                return Ok("closed".to_owned());
            }
            tokio::time::sleep(POLL).await;
        }
        Err(format!(
            "chrome did not exit within {}s of Browser.close",
            LAUNCH_WAIT.as_secs()
        ))
    }

    async fn targets(&self) -> Result<Vec<Target>, String> {
        let url = format!("http://127.0.0.1:{}/json/list", self.port);
        self.http
            .get(&url)
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .map_err(|e| format!("get {url}: {e}"))?
            .json()
            .await
            .map_err(|e| format!("decode {url}: {e}"))
    }

    /// Starts Chrome and waits for its CDP while it lives; a session restore blocks CDP until every window is back.
    async fn launch(&self) -> Result<(), String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg(format!("--remote-debugging-port={}", self.port))
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--user-data-dir={}", self.profile.display()))
            .args([
                "--force-renderer-accessibility",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-session-crashed-bubble",
                "--hide-crash-restore-bubble",
                "--password-store=basic",
                "--start-maximized",
                "--restore-last-session",
            ])
            .env("LANG", "en_US.UTF-8")
            .env("LANGUAGE", "en_US")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        sys::unprotect(&mut cmd);
        let mut chrome = cmd
            .spawn()
            .map_err(|e| format!("launch {}: {e}", self.bin))?;
        let deadline = tokio::time::Instant::now() + LAUNCH_WAIT;
        while tokio::time::Instant::now() < deadline {
            if self.targets().await.is_ok() {
                return Ok(());
            }
            if let Ok(Some(status)) = chrome.try_wait() {
                return Err(format!(
                    "chrome exited with {status} before exposing CDP on port {}",
                    self.port
                ));
            }
            tokio::time::sleep(POLL).await;
        }
        Err(format!(
            "chrome did not expose CDP on port {} within {}s",
            self.port,
            LAUNCH_WAIT.as_secs()
        ))
    }

    /// Closes the agent windows a run that ended with them open left in Chrome, known by their first history entry.
    async fn close_restored(&self) {
        let Ok(targets) = self.targets().await else {
            return;
        };
        let mut restored = Vec::new();
        for t in targets.iter().filter(|t| t.is_tab()) {
            if let Ok(mut cdp) = Cdp::open(&t.ws_url).await
                && let Ok(history) = cdp.call("Page.getNavigationHistory", json!({})).await
                && history["entries"][0]["url"] == AGENT_PAGE
            {
                restored.push(t.id.as_str());
            }
        }
        if !restored.is_empty()
            && let Ok(mut cdp) = self.browser().await
        {
            cdp.close_windows(&restored).await;
        }
    }
}

struct Slot {
    window: Arc<Mutex<Option<Target>>>,
    used: Instant,
}

#[derive(Deserialize)]
struct Target {
    #[serde(default)]
    id: String,
    #[serde(rename = "type")]
    kind: String,
    url: String,
    #[serde(rename = "webSocketDebuggerUrl", default)]
    ws_url: String,
}

impl Target {
    fn is_tab(&self) -> bool {
        self.kind == "page" && !self.url.starts_with("devtools://")
    }

    fn is_web(&self) -> bool {
        matches!(self.kind.as_str(), "page" | "iframe")
            && (self.url.starts_with("https://") || self.url.starts_with("http://"))
    }
}

/// A tab the user has open, attached.
struct Tab {
    id: String,
    title: String,
    url: String,
    shown: bool,
    cdp: Cdp,
}

impl std::fmt::Display for Tab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "- tab {} \"{}\" ({})", self.id, self.title, self.url)?;
        if self.shown {
            f.write_str(" [in front]")?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct NavigateArgs {
    url: String,
    #[serde(default)]
    front: bool,
}

#[derive(Deserialize)]
struct RefArgs {
    #[serde(rename = "ref")]
    element: String,
}

#[derive(Deserialize)]
struct TypeArgs {
    #[serde(rename = "ref")]
    element: String,
    text: String,
    #[serde(default)]
    clear: bool,
    #[serde(default)]
    submit: bool,
}

#[derive(Deserialize)]
struct FillArgs {
    fields: Vec<FillField>,
    #[serde(default)]
    click: Option<String>,
}

#[derive(Deserialize)]
struct FillField {
    #[serde(rename = "ref")]
    element: String,
    text: String,
}

#[derive(Deserialize)]
struct ReadArgs {
    #[serde(rename = "ref", default)]
    element: Option<String>,
}

#[derive(Default)]
struct Load {
    started: bool,
    stopped: bool,
}

/// The XHR and fetch requests of a page, from the Network events of its connection.
#[derive(Default)]
struct Xhr {
    /// When each request in flight was seen to start.
    flight: HashMap<String, Instant>,
    /// Whether one started or ended since the last poll of [`Cdp::settle`].
    moved: bool,
}

impl Xhr {
    fn note(&mut self, v: &Value) {
        let id = v["params"]["requestId"].as_str().unwrap_or_default();
        match v["method"].as_str() {
            Some("Network.requestWillBeSent")
                if matches!(v["params"]["type"].as_str(), Some("XHR" | "Fetch")) =>
            {
                self.moved |= self.flight.insert(id.to_owned(), Instant::now()).is_none();
            }
            Some("Network.loadingFinished" | "Network.loadingFailed") => {
                self.moved |= self.flight.remove(id).is_some();
            }
            _ => {}
        }
    }
}

struct Cdp {
    ws: Ws,
    next_id: u64,
    /// The flat session of an attached target, which then receives every call.
    session: Option<String>,
    xhr: Xhr,
    /// The page's frame loads since [`Cdp::watch_load`], from its Page events.
    load: Load,
    page_events: bool,
    /// The type and message of the dialog the page opened, from its Page events.
    dialog: Option<(String, String)>,
    /// What became of the dialogs the page opened during this connection.
    dialogs: Vec<String>,
}

impl Cdp {
    async fn open(ws_url: &str) -> Result<Self, String> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| format!("connect cdp {ws_url}: {e}"))?;
        Ok(Self {
            ws,
            next_id: 0,
            session: None,
            xhr: Xhr::default(),
            load: Load::default(),
            page_events: false,
            dialog: None,
            dialogs: Vec::new(),
        })
    }

    async fn send(&mut self, method: &str, params: Value) -> Result<u64, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"id": id, "method": method});
        msg["params"] = params;
        if let Some(session) = &self.session {
            msg["sessionId"] = session.as_str().into();
        }
        self.ws
            .send(WsMessage::text(msg.to_string()))
            .await
            .map_err(|e| format!("cdp send {method}: {e}"))?;
        Ok(id)
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.handle_dialog().await?;
        let id = self.send(method, params).await?;
        let wait = async {
            while let Some(frame) = self.ws.next().await {
                let frame = frame.map_err(|e| format!("cdp read: {e}"))?;
                let WsMessage::Text(text) = frame else {
                    continue;
                };
                let mut v: Value =
                    serde_json::from_str(&text).map_err(|e| format!("cdp decode: {e}"))?;
                if v["id"].as_u64() != Some(id) {
                    self.note(&v);
                    self.handle_dialog().await?;
                    continue;
                }
                if let Some(err) = v.get("error") {
                    return Err(format!("cdp {method}: {}", err["message"]));
                }
                return Ok(v["result"].take());
            }
            Err(format!("cdp connection closed during {method}"))
        };
        tokio::time::timeout(CALL_TIMEOUT, wait)
            .await
            .map_err(|_| format!("cdp {method} timed out"))?
    }

    async fn eval(&mut self, expression: &str) -> Result<Value, String> {
        let mut r = self
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true, "awaitPromise": true}),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex["exception"]["description"]
                .as_str()
                .or_else(|| ex["text"].as_str())
                .unwrap_or("script error");
            return Err(msg.lines().next().unwrap_or(msg).to_owned());
        }
        Ok(r["result"]["value"].take())
    }

    fn note(&mut self, v: &Value) {
        self.xhr.note(v);
        match v["method"].as_str() {
            Some("Page.frameStartedLoading") => self.load.started = true,
            Some("Page.frameStoppedLoading") => self.load.stopped = true,
            Some("Page.javascriptDialogOpening") => {
                let p = &v["params"];
                self.dialog = Some((
                    p["type"].as_str().unwrap_or_default().to_owned(),
                    p["message"].as_str().unwrap_or_default().to_owned(),
                ));
            }
            _ => {}
        }
    }

    /// Turns on the Page events of the connection and forgets the loads seen so far, before an input that may open a page.
    async fn watch_load(&mut self) -> Result<(), String> {
        self.enable_page().await?;
        self.load = Load::default();
        Ok(())
    }

    /// Turns on the Page events of the connection once; a dialog open before the call blocks it, so it waits [`ENABLE_WAIT`] at most.
    async fn enable_page(&mut self) -> Result<(), String> {
        if !self.page_events {
            tokio::time::timeout(ENABLE_WAIT, self.call("Page.enable", json!({})))
                .await
                .map_err(|_| NO_ANSWER.to_owned())??;
            self.page_events = true;
        }
        Ok(())
    }

    /// Accepts the alert the page opened, else dismisses its dialog and fails.
    async fn handle_dialog(&mut self) -> Result<(), String> {
        let Some((kind, message)) = self.dialog.take() else {
            return Ok(());
        };
        let alert = kind == "alert";
        self.send("Page.handleJavaScriptDialog", json!({"accept": alert}))
            .await?;
        if alert {
            self.dialogs
                .push(format!("the page showed an alert: \"{message}\""));
            return Ok(());
        }
        let err = format!(
            "the page opened a {kind} dialog \"{message}\" and the browser tools dismissed it; to accept one, do the action on the screen"
        );
        self.dialogs.push(err.clone());
        Err(err)
    }

    /// Waits for the page an input opened: at most [`NAV_START`] for a frame to start loading, then up to [`LOAD_WAIT`]
    /// for the loads to stop and the document to be complete.
    async fn opened_page(&mut self) {
        let start = tokio::time::Instant::now();
        while self.dialog.is_none() && (!self.load.started || !self.load.stopped) {
            let wait = if self.load.started {
                LOAD_WAIT
            } else {
                NAV_START
            };
            match tokio::time::timeout_at(start + wait, self.ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(text)))) => {
                    if let Ok(v) = serde_json::from_str::<Value>(&text) {
                        self.note(&v);
                    }
                }
                Ok(Some(Ok(_))) => {}
                _ => break,
            }
        }
        if self.load.started {
            self.wait_loaded().await;
        }
    }

    /// Clicks the element of `element` and waits for the page the click opens; reports the label and the page.
    async fn click_ref(&mut self, element: &str) -> Result<String, String> {
        let p = self
            .eval(&format!("{POINT_JS}({})", js_str(element)))
            .await?;
        let (x, y) = (
            p["x"].as_f64().unwrap_or(0.0),
            p["y"].as_f64().unwrap_or(0.0),
        );
        self.watch_load().await?;
        self.click(x, y).await?;
        self.opened_page().await;
        let here = self
            .eval("document.title + ' (' + location.href + ')'")
            .await?;
        Ok(format!(
            "clicked {element} \"{}\"; page is now {}",
            p["label"].as_str().unwrap_or_default(),
            here.as_str().unwrap_or_default()
        ))
    }

    async fn wait_loaded(&mut self) {
        let deadline = tokio::time::Instant::now() + LOAD_WAIT;
        while tokio::time::Instant::now() < deadline {
            if let Ok(v) = self.eval("document.readyState").await
                && v == "complete"
            {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Attaches to page `target`, loads `url` and returns its title, address and text.
    async fn fetch(&mut self, target: &str, url: &str, wait: Duration) -> Result<String, String> {
        let attached = self
            .call(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        self.session = attached["sessionId"].as_str().map(str::to_owned);
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": 1280, "height": 800, "deviceScaleFactor": 1, "mobile": false}),
        )
        .await?;
        self.call(
            "Fetch.enable",
            json!({"patterns": [{"resourceType": "Document", "requestStage": "Response"}]}),
        )
        .await?;
        self.call("Page.enable", json!({})).await?;
        self.call("Page.setLifecycleEventsEnabled", json!({"enabled": true}))
            .await?;
        let tree = self.call("Page.getFrameTree", json!({})).await?;
        let start = tokio::time::Instant::now();
        let ready = self
            .load(url, &tree["frameTree"]["frame"]["id"], wait)
            .await?;
        if !ready {
            self.call("Page.stopLoading", json!({})).await?;
        }
        timing::mark("load");
        let here = self
            .eval("document.title + ' (' + location.href + ')'")
            .await?;
        let mut text = self.eval(&format!("{READ_JS}(null)")).await?;
        timing::mark("read");
        if ready && loading(text.as_str().unwrap_or_default()) {
            text = self.filled(text).await;
            timing::mark("fill");
        }
        let mut out = format!("Page: {}", here.as_str().unwrap_or_default());
        if !ready {
            let _ = write!(
                out,
                " [still loading after {}s; text so far]",
                start.elapsed().as_secs()
            );
        }
        out.push_str("\n\n");
        out.push_str(&capped(text.as_str().unwrap_or_default()));
        Ok(out)
    }

    /// Navigates main frame `frame` to `url` and waits for the new document's DOMContentLoaded and a nearly idle network,
    /// at most `wait` and [`SETTLE`] past the DOMContentLoaded; true once that came. Documents that are no page fail, so nothing downloads.
    async fn load(&mut self, url: &str, frame: &Value, wait: Duration) -> Result<bool, String> {
        let id = self.send("Page.navigate", json!({"url": url})).await?;
        let end = tokio::time::Instant::now() + wait;
        let mut deadline = end;
        let (mut fresh, mut ready, mut idle, mut challenge) = (false, false, false, false);
        let mut refused = None;
        let mut poke = tokio::time::interval(POKE);
        while !(ready && idle) {
            let next = tokio::select! {
                next = tokio::time::timeout_at(deadline, self.ws.next()) => match next {
                    Err(_) => break,
                    Ok(next) => next.ok_or("cdp connection closed during load")?,
                },
                _ = poke.tick() => {
                    self.send("Runtime.evaluate", json!({"expression": "0"})).await?;
                    continue;
                }
            };
            let WsMessage::Text(text) = next.map_err(|e| format!("cdp read: {e}"))? else {
                continue;
            };
            let v: Value = serde_json::from_str(&text).map_err(|e| format!("cdp decode: {e}"))?;
            if v["id"].as_u64() == Some(id) {
                if let Some(err) = v.get("error") {
                    return Err(format!("cdp Page.navigate: {}", err["message"]));
                }
                if let Some(kind) = refused {
                    return Err(format!(
                        "{url} serves {kind}, not a page: browser_fetch reads HTML, text, XML and JSON; download it with shell"
                    ));
                }
                if let Some(err) = v["result"]["errorText"].as_str().filter(|e| !e.is_empty()) {
                    return Err(format!("navigate {url}: {err}"));
                }
                continue;
            }
            if v["method"] == "Fetch.requestPaused" {
                let paused = &v["params"];
                if paused["frameId"] == *frame {
                    let cleared = challenge;
                    challenge = header(paused, "cf-mitigated") == Some("challenge");
                    if challenge {
                        deadline = deadline.min(tokio::time::Instant::now() + CHALLENGE_WAIT);
                    } else if cleared
                        && paused["responseStatusCode"]
                            .as_u64()
                            .is_some_and(|c| (200..300).contains(&c))
                    {
                        deadline = end;
                    }
                }
                let request = json!({"requestId": paused["requestId"]});
                match not_a_page(paused) {
                    Some(kind) => {
                        if paused["frameId"] == *frame {
                            refused = Some(kind);
                        }
                        let mut fail = request;
                        fail["errorReason"] = "BlockedByClient".into();
                        self.send("Fetch.failRequest", fail).await?;
                    }
                    None => {
                        self.send("Fetch.continueRequest", request).await?;
                    }
                }
                continue;
            }
            if v["method"] != "Page.lifecycleEvent" || v["params"]["frameId"] != *frame {
                continue;
            }
            match v["params"]["name"].as_str() {
                Some("init") => (fresh, ready, idle) = (true, false, false),
                _ if challenge => {}
                Some("DOMContentLoaded") if fresh => {
                    ready = true;
                    deadline = deadline.min(tokio::time::Instant::now() + SETTLE);
                }
                Some("networkAlmostIdle") if fresh => idle = true,
                _ => {}
            }
        }
        Ok(ready)
    }

    /// Waits until the page text kept its length over one [`SPA_POLL`] with no XHR or fetch ended meanwhile or in
    /// flight for less than [`XHR_STALE`], at most until `deadline`, so content that scripts load after the page is in.
    async fn settle(&mut self, deadline: tokio::time::Instant) {
        let mut last = None;
        self.xhr.moved = false;
        loop {
            let Ok(len) = tokio::time::timeout_at(deadline, self.eval(TEXT_LEN_JS)).await else {
                return;
            };
            let fresh = self.xhr.flight.values().any(|t| t.elapsed() < XHR_STALE);
            let quiet = !fresh && !std::mem::take(&mut self.xhr.moved);
            if quiet && last.is_some() && len.as_ref().ok() == last.as_ref() {
                return;
            }
            last = len.ok();
            tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + SPA_POLL)).await;
        }
    }

    /// Reads the page text again every [`SPA_POLL`] while `text`, the last read, is a [`loading`] shell, until it is none
    /// and kept its length over one poll, at most [`SPA_WAIT`]; returns the last text read.
    async fn filled(&mut self, mut text: Value) -> Value {
        let deadline = tokio::time::Instant::now() + SPA_WAIT;
        let read = format!("{READ_JS}(null)");
        loop {
            tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + SPA_POLL)).await;
            let Ok(Ok(next)) = tokio::time::timeout_at(deadline, self.eval(&read)).await else {
                return text;
            };
            let len = |v: &Value| v.as_str().map_or(0, str::len);
            let done = !loading(next.as_str().unwrap_or_default()) && len(&next) == len(&text);
            text = next;
            if done {
                return text;
            }
        }
    }

    async fn click(&mut self, x: f64, y: f64) -> Result<(), String> {
        for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}),
            )
            .await?;
        }
        Ok(())
    }

    async fn press_enter(&mut self) -> Result<(), String> {
        self.watch_load().await?;
        for kind in ["keyDown", "keyUp"] {
            self.call(
                "Input.dispatchKeyEvent",
                json!({"type": kind, "key": "Enter", "code": "Enter",
                       "windowsVirtualKeyCode": 13, "text": "\r"}),
            )
            .await?;
        }
        self.opened_page().await;
        Ok(())
    }

    /// Closes the agent windows of pages `ids` with every tab the agents opened from them.
    async fn close_windows(&mut self, ids: &[&str]) {
        if ids.is_empty() {
            return;
        }
        let Ok(infos) = self.call("Target.getTargets", json!({})).await else {
            return;
        };
        let mut pages = Vec::new();
        for info in infos["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["type"] == "page")
        {
            let window = self
                .call(
                    "Browser.getWindowForTarget",
                    json!({"targetId": info["targetId"]}),
                )
                .await
                .ok()
                .and_then(|w| w["windowId"].as_i64());
            pages.push((info, window));
        }
        let mut closing: Vec<&str> = ids.iter().flat_map(|id| window_tabs(id, &pages)).collect();
        closing.sort_unstable();
        closing.dedup();
        for t in &closing {
            let _ = self
                .call("Target.closeTarget", json!({"targetId": t}))
                .await;
        }
        // closeTarget returns before the tab is gone, and a Browser.close meanwhile saves it in the session
        let deadline = tokio::time::Instant::now() + CLOSE_WAIT;
        while tokio::time::Instant::now() < deadline {
            let Ok(infos) = self.call("Target.getTargets", json!({})).await else {
                return;
            };
            let open = infos["targetInfos"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|i| i["targetId"].as_str().is_some_and(|t| closing.contains(&t)));
            if !open {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    }

    async fn snapshot(&mut self, plans: bool) -> Result<String, String> {
        let v = self.eval(&format!("{SNAPSHOT_JS}({plans})")).await?;
        let snapshot = v.as_str().unwrap_or_default().to_owned();
        timing::mark("snapshot");
        timing::note("snapshot_bytes", snapshot.len());
        Ok(snapshot)
    }
}

/// Page `id`, the tabs in its window, and the pages opened from any of them, transitively.
fn window_tabs<'a>(id: &'a str, pages: &[(&'a Value, Option<i64>)]) -> Vec<&'a str> {
    let target = |info: &'a Value| info["targetId"].as_str().unwrap_or_default();
    let window = pages
        .iter()
        .find(|(info, _)| target(info) == id)
        .and_then(|(_, w)| *w);
    let mut owned = vec![id];
    owned.extend(
        pages
            .iter()
            .filter(|(info, w)| window.is_some() && *w == window && target(info) != id)
            .map(|(info, _)| target(info)),
    );
    loop {
        let opened: Vec<&str> = pages
            .iter()
            .map(|(info, _)| *info)
            .filter(|info| !owned.contains(&target(info)))
            .filter(|info| {
                ["openerId", "openerFrameId"]
                    .iter()
                    .any(|k| info[*k].as_str().is_some_and(|o| owned.contains(&o)))
            })
            .map(target)
            .collect();
        if opened.is_empty() {
            return owned;
        }
        owned.extend(opened);
    }
}

/// Page title of the X active window when it is a Chrome window.
async fn active_title() -> Option<String> {
    let root = xprop(&["-root", "_NET_ACTIVE_WINDOW"]).await?;
    let name = xprop(&["-id", active_window(&root)?, "_NET_WM_NAME"]).await?;
    chrome_page_title(&name)
}

async fn xprop(args: &[&str]) -> Option<String> {
    let run = Command::new(XPROP)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(XPROP_WAIT, run).await.ok()?.ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The window id in `_NET_ACTIVE_WINDOW(WINDOW): window id # 0x600003, 0x0`; none for `0x0`.
fn active_window(xprop: &str) -> Option<&str> {
    let id = xprop.split_once('#')?.1.split(',').next()?.trim();
    (id.starts_with("0x") && id != "0x0").then_some(id)
}

/// The page title in `_NET_WM_NAME(UTF8_STRING) = "<title> - Google Chrome"`.
fn chrome_page_title(xprop: &str) -> Option<String> {
    let name = xprop_string(xprop)?;
    let (title, app) = name.rsplit_once(" - ")?;
    CHROME_APPS.contains(&app).then(|| title.to_owned())
}

/// The quoted value xprop prints, undoing its `\"`, `\\` and octal `\ooo` escapes.
fn xprop_string(xprop: &str) -> Option<String> {
    let quoted = xprop.split_once(" = \"")?.1.trim_end().strip_suffix('"')?;
    let mut out = Vec::with_capacity(quoted.len());
    let mut rest = quoted.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        rest = tail;
        if b != b'\\' {
            out.push(b);
        } else if let Some(octal) = rest
            .get(..3)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)))
        {
            out.push(u8::from_str_radix(std::str::from_utf8(octal).ok()?, 8).ok()?);
            rest = &rest[3..];
        } else if let Some((&c, tail)) = rest.split_first() {
            out.push(c);
            rest = tail;
        }
    }
    String::from_utf8(out).ok()
}

/// `url` with `https://` added when it names no scheme.
fn web_url(url: String) -> String {
    if url.contains("://") || url.starts_with("about:") {
        url
    } else {
        format!("https://{url}")
    }
}

fn header<'a>(paused: &'a Value, name: &str) -> Option<&'a str> {
    paused["responseHeaders"].as_array()?.iter().find(|h| {
        h["name"]
            .as_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })?["value"]
        .as_str()
}

/// What a paused successful document response serves when it is no page to read: an attachment or a type outside [`PAGE_TYPES`].
fn not_a_page(paused: &Value) -> Option<String> {
    let status = paused["responseStatusCode"].as_u64()?;
    if !(200..300).contains(&status) {
        return None;
    }
    let kind = header(paused, "content-type").unwrap_or_default();
    let essence = kind
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if header(paused, "content-disposition").is_some_and(|d| {
        d.trim_start()
            .to_ascii_lowercase()
            .starts_with("attachment")
    }) {
        return Some(format!("an attachment ({kind})"));
    }
    (!PAGE_TYPES.contains(&essence.as_str())).then_some(essence)
}

/// Whether page `text` looks like a shell whose content scripts still load: empty, or short and saying it is loading.
fn loading(text: &str) -> bool {
    let text = text.trim();
    text.is_empty()
        || (text.chars().count() <= SHELL_CHARS && text.to_lowercase().contains("loading"))
}

/// `text` cut to [`READ_CHARS`] characters, saying so when it was longer.
fn capped(text: &str) -> String {
    let total = text.chars().count();
    let mut out: String = text.chars().take(READ_CHARS).collect();
    if total > READ_CHARS {
        let _ = write!(out, "\n[truncated: {total} characters total]");
    }
    out
}

fn js_str(s: &str) -> String {
    Value::String(s.to_owned()).to_string()
}

/// `s` as a JS string literal in a buffer sized for the worst-case escape, so it never reallocates.
fn js_secret(s: &str) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(s.len() * 6 + 2));
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
pub(super) mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const MAILBOX: &str = "<!doctype html><title>Mail</title><style>.row{cursor:pointer}</style><ul id=inbox></ul><div id=cards></div>\
        <form><label>To <input name=to></label><label>Body <textarea name=body></textarea></label></form><script>\
        for (let n = 1; n <= 45; n++) { const li = document.createElement('li'); li.className = 'row';\
        li.innerHTML = '<b>Sender ' + n + '</b> <span>subject ' + n + '</span> <span>a preview of message ' + n + '</span> <time>Oct ' + n + '</time> <button data-tip=Star><i class=\"fa-solid fa-star\"></i></button>';\
        li.addEventListener('click', (e) => { if (e.target.tagName !== 'BUTTON') document.title = 'opened ' + n; }); inbox.append(li); }\
        for (let n = 1; n <= 8; n++) { const d = document.createElement('div'); d.innerHTML = '<p>Card ' + n + '</p><button>Resolve</button>'; cards.append(d); }\
        </script>";

    /// Serves Chrome's CDP endpoints on the returned port until a `Browser.close` arrives.
    async fn fake_chrome() -> u16 {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        let version = json!({
            "webSocketDebuggerUrl": format!("ws://{}/devtools/browser/fake", ws.local_addr().unwrap()),
        })
        .to_string();
        let (closed, mut stopped) = tokio::sync::watch::channel(false);
        tokio::spawn(async move {
            let (stream, _) = ws.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                let v: Value = serde_json::from_str(&text).unwrap();
                if v["method"] == "Browser.close" {
                    let reply = json!({"id": v["id"], "result": {}}).to_string();
                    ws.send(WsMessage::text(reply)).await.unwrap();
                    closed.send(true).unwrap();
                    return;
                }
            }
        });
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => return,
                    Ok((mut s, _)) = http.accept() => {
                        let mut buf = [0u8; 1024];
                        let n = s.read(&mut buf).await.unwrap_or(0);
                        let body = if buf[..n].starts_with(b"GET /json/version") { version.as_str() } else { "[]" };
                        let head = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                        let _ = s.write_all(format!("{head}{body}").as_bytes()).await;
                    }
                }
            }
        });
        port
    }

    /// Serves CDP on the returned port only after `delay`; every new window is page 1, whose script evaluations return "Page: slow".
    pub(in crate::toolhost) async fn slow_chrome(delay: Duration) -> u16 {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = socket.local_addr().unwrap().port();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = ws.local_addr().unwrap();
        let list = json!([{
            "id": "1",
            "type": "page",
            "url": "about:blank",
            "webSocketDebuggerUrl": format!("ws://{addr}/devtools/page/1"),
        }])
        .to_string();
        let version = json!({"webSocketDebuggerUrl": format!("ws://{addr}/devtools/browser/fake")})
            .to_string();
        tokio::spawn(async move {
            while let Ok((stream, _)) = ws.accept().await {
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                        let v: Value = serde_json::from_str(&text).unwrap();
                        let reply = json!({"id": v["id"], "result": {"targetId": "1", "result": {"value": "Page: slow"}}});
                        ws.send(WsMessage::text(reply.to_string())).await.unwrap();
                    }
                });
            }
        });
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let http = socket.listen(1024).unwrap();
            while let Ok((mut s, _)) = http.accept().await {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let body = if buf[..n].starts_with(b"GET /json/version") {
                    &version
                } else {
                    &list
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(format!("{head}{body}").as_bytes()).await;
            }
        });
        port
    }

    /// Serves CDP on the returned port for `targets` of (type, url, value); every script evaluation in a target returns its value.
    pub(in crate::toolhost) async fn web_chrome(targets: &[(&str, &str, &str)]) -> u16 {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        let mut list = Vec::new();
        for (i, (kind, url, value)) in targets.iter().enumerate() {
            let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = ws.local_addr().unwrap();
            list.push(json!({"id": i.to_string(), "type": kind, "url": url, "webSocketDebuggerUrl": format!("ws://{addr}/devtools/page/{i}")}));
            let value = (*value).to_owned();
            tokio::spawn(async move {
                while let Ok((stream, _)) = ws.accept().await {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                        let v: Value = serde_json::from_str(&text).unwrap();
                        let reply = json!({"id": v["id"], "result": {"result": {"value": value}}});
                        ws.send(WsMessage::text(reply.to_string())).await.unwrap();
                    }
                }
            });
        }
        let list = Value::from(list).to_string();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = http.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    list.len()
                );
                let _ = s.write_all(format!("{head}{list}").as_bytes()).await;
            }
        });
        port
    }

    /// Serves CDP on the returned port for page tabs of (first history entry, visibility, title); a tab's
    /// other script evaluations return `Page: tab <index>`.
    async fn tabs_chrome(tabs: &[(&str, &str, &str)]) -> u16 {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        let mut list = Vec::new();
        for (i, (first, shown, title)) in tabs.iter().enumerate() {
            let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = ws.local_addr().unwrap();
            let url = format!("https://t{i}.test/");
            list.push(json!({"id": format!("T{i}"), "type": "page", "url": url, "webSocketDebuggerUrl": format!("ws://{addr}/devtools/page/T{i}")}));
            let history = json!({"entries": [{"url": first}]});
            let info = json!([shown, title, url]);
            tokio::spawn(async move {
                while let Ok((stream, _)) = ws.accept().await {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                        let v: Value = serde_json::from_str(&text).unwrap();
                        let result = match v["method"].as_str().unwrap() {
                            "Page.getNavigationHistory" => history.clone(),
                            _ if v["params"]["expression"] == TAB_JS => {
                                json!({"result": {"value": info}})
                            }
                            _ => json!({"result": {"value": format!("Page: tab {i}")}}),
                        };
                        let reply = json!({"id": v["id"], "result": result});
                        ws.send(WsMessage::text(reply.to_string())).await.unwrap();
                    }
                }
            });
        }
        let list = Value::from(list).to_string();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = http.accept().await {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    list.len()
                );
                let _ = s.write_all(format!("{head}{list}").as_bytes()).await;
            }
        });
        port
    }

    /// Serves CDP on the returned port with a hidden tab whose page text is `text`, logging every message;
    /// its navigation loads only with `loads`, else only the blank page's events come again. A page under
    /// `/spa` starts an XHR that ends at the 3rd text length read, after which the text grows; one under
    /// `/spa-hang` never ends it, and the text of one under `/spa-grow` grows at every read. The text of a page
    /// under `/shell` is a loading shell at the first two reads and `feed item one` after.
    async fn fetch_chrome(text: String, loads: bool) -> (u16, Arc<std::sync::Mutex<Vec<Value>>>) {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        let version = json!({
            "webSocketDebuggerUrl": format!("ws://{}/devtools/browser/fake", ws.local_addr().unwrap()),
        })
        .to_string();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((stream, _)) = ws.accept().await {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let (log, text) = (Arc::clone(&seen), text.clone());
                tokio::spawn(async move {
                    let (mut retrying, mut late, mut held) = (false, false, false);
                    let (mut spa, mut hang, mut grow, mut reads) = (false, false, false, 0);
                    let (mut shell, mut texts) = (false, 0);
                    while let Some(Ok(WsMessage::Text(msg))) = ws.next().await {
                        let v: Value = serde_json::from_str(&msg).unwrap();
                        log.lock().unwrap().push(v.clone());
                        let method = v["method"].as_str().unwrap();
                        let read = method == "Runtime.evaluate" && v["params"]["expression"] != "0";
                        if retrying && !late && (read || method == "Page.stopLoading") {
                            late = true;
                            held = true;
                            let paused = json!({"method": "Fetch.requestPaused", "sessionId": "s1", "params": {
                                "requestId": "r2", "frameId": "hidden", "responseStatusCode": 302, "responseHeaders": []}});
                            ws.send(WsMessage::text(paused.to_string())).await.unwrap();
                        }
                        if method == "Page.stopLoading" {
                            held = false;
                        }
                        if held && method == "Runtime.evaluate" {
                            continue;
                        }
                        if method == "Runtime.evaluate" && v["params"]["expression"] == TEXT_LEN_JS
                        {
                            reads += 1;
                            if spa && !hang && reads == 3 {
                                let done = json!({"method": "Network.loadingFinished", "sessionId": "s1",
                                    "params": {"requestId": "x1"}});
                                ws.send(WsMessage::text(done.to_string())).await.unwrap();
                            }
                            let len = if grow {
                                reads
                            } else if spa && reads > 3 && !hang {
                                50
                            } else {
                                5
                            };
                            let reply = json!({"id": v["id"], "sessionId": "s1", "result": {"result": {"value": len}}});
                            ws.send(WsMessage::text(reply.to_string())).await.unwrap();
                            continue;
                        }
                        let result = match method {
                            "Target.createTarget" => json!({"targetId": "hidden"}),
                            "Target.attachToTarget" => json!({"sessionId": "s1"}),
                            "Page.getFrameTree" => {
                                json!({"frameTree": {"frame": {"id": "hidden"}}})
                            }
                            "Page.navigate" => json!({"frameId": "hidden", "loaderId": "l2"}),
                            "Runtime.evaluate"
                                if v["params"]["expression"]
                                    .as_str()
                                    .unwrap()
                                    .starts_with("document.title") =>
                            {
                                json!({"result": {"value": "Doc (https://a.test/)"}})
                            }
                            "Runtime.evaluate" if shell && read => {
                                texts += 1;
                                let page = if texts < 3 {
                                    "Loading your feed…"
                                } else {
                                    "feed item one"
                                };
                                json!({"result": {"value": page}})
                            }
                            "Runtime.evaluate" => json!({"result": {"value": text}}),
                            _ => json!({}),
                        };
                        let zip = v["params"]["url"]
                            .as_str()
                            .is_some_and(|u| u.ends_with(".zip"));
                        let challenge = v["params"]["url"]
                            .as_str()
                            .is_some_and(|u| u.contains("/challenge"));
                        let retries = v["params"]["url"]
                            .as_str()
                            .is_some_and(|u| u.ends_with("/challenge-retry"));
                        if v["method"] == "Page.navigate" {
                            let url = v["params"]["url"].as_str().unwrap();
                            (spa, hang, grow) = (
                                url.contains("/spa"),
                                url.ends_with("/spa-hang"),
                                url.ends_with("/spa-grow"),
                            );
                            shell = url.ends_with("/shell");
                            if spa {
                                let xhr = json!({"method": "Network.requestWillBeSent", "sessionId": "s1",
                                    "params": {"requestId": "x1", "type": "XHR"}});
                                ws.send(WsMessage::text(xhr.to_string())).await.unwrap();
                            }
                            let kind = if zip {
                                "application/zip"
                            } else {
                                "text/html; charset=utf-8"
                            };
                            let mut paused = json!({"method": "Fetch.requestPaused", "sessionId": "s1", "params": {
                                "requestId": "r1", "frameId": "hidden", "responseStatusCode": 200,
                                "responseHeaders": [{"name": "Content-Type", "value": kind}]}});
                            if challenge {
                                paused["params"]["responseStatusCode"] = 403.into();
                                paused["params"]["responseHeaders"]
                                    .as_array_mut()
                                    .unwrap()
                                    .push(json!({"name": "cf-mitigated", "value": "challenge"}));
                            }
                            ws.send(WsMessage::text(paused.to_string())).await.unwrap();
                            retrying = retries;
                            if retries {
                                tokio::time::sleep(Duration::from_secs(3)).await;
                                let mut redirect = paused.clone();
                                redirect["params"]["responseStatusCode"] = 302.into();
                                redirect["params"]["responseHeaders"] = json!([]);
                                paused["params"]["responseStatusCode"] = 429.into();
                                for p in [redirect, paused] {
                                    ws.send(WsMessage::text(p.to_string())).await.unwrap();
                                }
                            }
                        }
                        let mut result = result;
                        if zip {
                            result["errorText"] = "net::ERR_BLOCKED_BY_CLIENT".into();
                        }
                        let reply =
                            json!({"id": v["id"], "sessionId": v["sessionId"], "result": result});
                        ws.send(WsMessage::text(reply.to_string())).await.unwrap();
                        if v["method"] != "Page.navigate" {
                            continue;
                        }
                        let names: &[&str] = if loads {
                            &["init", "DOMContentLoaded", "load", "networkAlmostIdle"]
                        } else {
                            &["DOMContentLoaded", "load", "networkAlmostIdle"]
                        };
                        for name in names {
                            let event = json!({"method": "Page.lifecycleEvent", "sessionId": "s1",
                                "params": {"frameId": "hidden", "name": name}});
                            ws.send(WsMessage::text(event.to_string())).await.unwrap();
                        }
                    }
                });
            }
        });
        tokio::spawn(async move {
            while let Ok((mut s, _)) = http.accept().await {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let body = if buf[..n].starts_with(b"GET /json/version") {
                    version.as_str()
                } else {
                    "[]"
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(format!("{head}{body}").as_bytes()).await;
            }
        });
        (port, log)
    }

    /// Serves CDP on the returned port with one page, 1, which a new window opens as, logging every method called;
    /// every script evaluation returns "complete".
    async fn page_chrome() -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        let addr = ws.local_addr().unwrap();
        let list = json!([{"id": "1", "type": "page", "url": AGENT_PAGE, "webSocketDebuggerUrl": format!("ws://{addr}/devtools/page/1")}])
            .to_string();
        let version = json!({"webSocketDebuggerUrl": format!("ws://{addr}/devtools/browser/fake")})
            .to_string();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&log);
        tokio::spawn(async move {
            while let Ok((stream, _)) = ws.accept().await {
                let log = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                    while let Some(Ok(WsMessage::Text(text))) = ws.next().await {
                        let v: Value = serde_json::from_str(&text).unwrap();
                        log.lock()
                            .unwrap()
                            .push(v["method"].as_str().unwrap().to_owned());
                        let reply = json!({"id": v["id"], "result": {"targetId": "1", "result": {"value": "complete"}}});
                        ws.send(WsMessage::text(reply.to_string())).await.unwrap();
                    }
                });
            }
        });
        tokio::spawn(async move {
            while let Ok((mut s, _)) = http.accept().await {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).await.unwrap_or(0);
                let body = if buf[..n].starts_with(b"GET /json/version") {
                    &version
                } else {
                    &list
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(format!("{head}{body}").as_bytes()).await;
            }
        });
        (port, log)
    }

    /// A stand-in Chrome binary in `dir` that lives 5 s.
    fn alive_chrome(dir: &std::path::Path) -> String {
        let chrome = dir.join("chrome");
        std::fs::write(&chrome, "#!/bin/sh\nsleep 5\n").unwrap();
        std::fs::set_permissions(&chrome, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        chrome.display().to_string()
    }

    #[tokio::test]
    async fn a_start_after_agent_windows_were_left_open_scans_once() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(AGENT_MARKER);
        std::fs::write(&marker, b"").unwrap();
        let port = slow_chrome(Duration::from_millis(300)).await;
        let b = Browser::new(alive_chrome(dir.path()), dir.path().into(), port, false);
        assert_eq!(
            b.run("state", "browser_start", json!({})).await.unwrap(),
            "running"
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn a_chrome_already_running_is_swept_only_at_its_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(AGENT_MARKER);
        std::fs::write(&marker, b"").unwrap();
        let port = slow_chrome(Duration::ZERO).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        for swept in [true, false] {
            assert_eq!(
                b.run("state", "browser_start", json!({})).await.unwrap(),
                "running"
            );
            assert_eq!(!marker.exists(), swept);
            std::fs::write(&marker, b"").unwrap();
        }
    }

    #[tokio::test]
    async fn a_browser_tool_waits_for_a_start_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(RUNNING_MARKER), b"").unwrap();
        let port = slow_chrome(Duration::from_millis(800)).await;
        let b = std::sync::Arc::new(Browser::new(
            alive_chrome(dir.path()),
            dir.path().into(),
            port,
            false,
        ));
        let start = {
            let b = std::sync::Arc::clone(&b);
            tokio::spawn(async move {
                b.run("state", "browser_start", json!({"if_was_running": true}))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let asked = tokio::time::Instant::now();
        assert_eq!(
            b.run("a", "browser_snapshot", json!({})).await.unwrap(),
            "Page: slow"
        );
        assert!(
            asked.elapsed() >= Duration::from_millis(500),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(start.await.unwrap().unwrap(), "running");
    }

    #[tokio::test]
    async fn a_chrome_that_exits_before_its_cdp_fails_the_start_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let b = Browser::new("false".into(), dir.path().into(), port, false);
        let asked = tokio::time::Instant::now();
        let err = b.run("a", "browser_start", json!({})).await.unwrap_err();
        assert!(
            err.starts_with("chrome exited with exit status: 1"),
            "{err}"
        );
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "{:?}",
            asked.elapsed()
        );
    }

    #[tokio::test]
    async fn fetch_reads_a_hidden_tab_capped_and_closes_it() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = fetch_chrome("x".repeat(READ_CHARS + 5), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        let out = b
            .run("a", "browser_fetch", json!({"url": "a.test"}))
            .await
            .unwrap();
        let want = format!(
            "Page: Doc (https://a.test/)\n\n{}\n[truncated: {} characters total]",
            "x".repeat(READ_CHARS),
            READ_CHARS + 5
        );
        assert_eq!(out, want);
        assert!(b.windows.lock().await.is_empty());
        let log = log.lock().unwrap();
        assert_eq!(
            log[0],
            json!({"id": 1, "method": "Target.createTarget", "params": {"url": "about:blank", "hidden": true}})
        );
        let metrics = log
            .iter()
            .find(|m| m["method"] == "Emulation.setDeviceMetricsOverride")
            .unwrap();
        assert_eq!(
            (&metrics["params"]["width"], &metrics["sessionId"]),
            (&json!(1280), &json!("s1"))
        );
        let go = log
            .iter()
            .find(|m| m["method"] == "Fetch.continueRequest")
            .unwrap();
        assert_eq!(go["params"], json!({"requestId": "r1"}));
        assert_eq!(go["sessionId"], "s1");
        let navigate = log.iter().find(|m| m["method"] == "Page.navigate").unwrap();
        assert_eq!(navigate["params"]["url"], "https://a.test");
        assert_eq!(navigate["sessionId"], "s1");
        let close = log.last().unwrap();
        assert_eq!(close["method"], "Target.closeTarget");
        assert_eq!(close["params"]["targetId"], "hidden");
        assert!(close.get("sessionId").is_none(), "{close}");
    }

    #[tokio::test]
    async fn fetch_past_its_wait_returns_the_text_so_far_and_closes_the_tab() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = fetch_chrome("partial".into(), false).await;
        let mut b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        b.fetch_wait = Duration::from_secs(1);
        let asked = tokio::time::Instant::now();
        let out = b
            .run("a", "browser_fetch", json!({"url": "https://a.test/"}))
            .await
            .unwrap();
        assert!(
            (Duration::from_secs(1)..Duration::from_secs(3)).contains(&asked.elapsed()),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(
            out,
            "Page: Doc (https://a.test/) [still loading after 1s; text so far]\n\npartial"
        );
        assert_eq!(
            log.lock().unwrap().last().unwrap()["method"],
            "Target.closeTarget"
        );
    }

    #[tokio::test]
    async fn fetch_does_not_stop_on_a_cloudflare_challenge() {
        let dir = tempfile::tempdir().unwrap();
        let (port, _) = fetch_chrome("Just a moment...".into(), true).await;
        let mut b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        b.fetch_wait = Duration::from_secs(1);
        let out = b
            .run(
                "a",
                "browser_fetch",
                json!({"url": "https://a.test/challenge"}),
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            "Page: Doc (https://a.test/) [still loading after 1s; text so far]\n\nJust a moment..."
        );
    }

    #[tokio::test]
    async fn fetch_gives_up_on_a_challenge_that_never_clears_before_the_load_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (port, _) = fetch_chrome("Just a moment...".into(), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        let start = std::time::Instant::now();
        let out = b
            .run(
                "a",
                "browser_fetch",
                json!({"url": "https://a.test/challenge-retry"}),
            )
            .await
            .unwrap();
        assert!(start.elapsed() < LOAD_WAIT);
        assert_eq!(
            out,
            format!(
                "Page: Doc (https://a.test/) [still loading after {}s; text so far]\n\nJust a moment...",
                CHALLENGE_WAIT.as_secs()
            )
        );
    }

    #[tokio::test]
    async fn fetch_fails_a_document_that_is_no_page_so_nothing_downloads() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = fetch_chrome(String::new(), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        let err = b
            .run("a", "browser_fetch", json!({"url": "https://a.test/f.zip"}))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "https://a.test/f.zip serves application/zip, not a page: browser_fetch reads HTML, text, XML and JSON; download it with shell"
        );
        let log = log.lock().unwrap();
        let fail = log
            .iter()
            .find(|m| m["method"] == "Fetch.failRequest")
            .unwrap();
        assert_eq!(
            fail["params"],
            json!({"requestId": "r1", "errorReason": "BlockedByClient"})
        );
        assert_eq!(log.last().unwrap()["method"], "Target.closeTarget");
    }

    #[test]
    fn only_a_successful_document_outside_the_page_types_or_an_attachment_is_no_page() {
        let paused = |status: u64, headers: Value| json!({"responseStatusCode": status, "responseHeaders": headers});
        let typed = |kind: &str| json!([{"name": "content-type", "value": kind}]);
        assert_eq!(
            not_a_page(&paused(200, typed("text/html; charset=utf-8"))),
            None
        );
        assert_eq!(not_a_page(&paused(200, typed("Application/JSON"))), None);
        assert_eq!(not_a_page(&paused(200, json!([]))), None);
        assert_eq!(not_a_page(&paused(302, typed("application/zip"))), None);
        assert_eq!(not_a_page(&json!({"responseErrorReason": "Failed"})), None);
        assert_eq!(
            not_a_page(&paused(200, typed("application/pdf"))),
            Some("application/pdf".to_owned())
        );
        let attachment = json!([
            {"name": "Content-Type", "value": "text/plain"},
            {"name": "Content-Disposition", "value": "attachment; filename=a.txt"}
        ]);
        assert_eq!(
            not_a_page(&paused(200, attachment)),
            Some("an attachment (text/plain)".to_owned())
        );
    }

    #[test]
    fn the_active_window_title_names_a_chrome_page_in_either_locale() {
        assert_eq!(
            active_window("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x600003, 0x0\n"),
            Some("0x600003")
        );
        assert_eq!(
            active_window("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x0\n"),
            None
        );
        let title = Some("登录 – \"q\" \\ back slash".to_owned());
        let utf8 =
            "_NET_WM_NAME(UTF8_STRING) = \"登录 – \\\"q\\\" \\\\ back slash - Google Chrome\"\n";
        let c = r#"_NET_WM_NAME(UTF8_STRING) = "\347\231\273\345\275\225 \342\200\223 \"q\" \\ back slash - Google Chrome""#;
        assert_eq!(chrome_page_title(utf8), title);
        assert_eq!(chrome_page_title(c), title);
        assert_eq!(
            chrome_page_title("_NET_WM_NAME(UTF8_STRING) = \"a - b - Chromium\""),
            Some("a - b".to_owned())
        );
        assert_eq!(
            chrome_page_title("_NET_WM_NAME(UTF8_STRING) = \"*notes.txt - Mousepad\""),
            None
        );
    }

    #[tokio::test]
    async fn navigate_brings_the_tab_to_the_front_only_when_asked_on_a_box_with_plans() {
        for (plans, front, brought) in [
            (true, false, false),
            (true, true, true),
            (false, true, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (port, log) = page_chrome().await;
            let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, plans);
            b.run(
                "a",
                "browser_navigate",
                json!({"url": "a.test", "front": front}),
            )
            .await
            .unwrap();
            let log = log.lock().unwrap();
            let at = |method: &str| log.iter().position(|m| m == method);
            assert!(at("Page.navigate").is_some(), "{log:?}");
            assert_eq!(at("Page.bringToFront").is_some(), brought, "{log:?}");
            assert!(
                !brought || at("Page.bringToFront") > at("Page.navigate"),
                "{log:?}"
            );
        }
    }

    #[tokio::test]
    async fn past_the_window_cap_a_new_agent_evicts_the_least_recently_used_one() {
        let dir = tempfile::tempdir().unwrap();
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), 1, false);
        for i in 0..MAX_WINDOWS {
            assert!(b.slot(&format!("a{i}")).await.1.is_none());
        }
        assert!(b.slot("a0").await.1.is_none());
        let (_, evicted) = b.slot("new").await;
        assert!(evicted.is_some());
        let windows = b.windows.lock().await;
        assert_eq!(windows.len(), MAX_WINDOWS);
        assert!(windows.contains_key("a0") && windows.contains_key("new"));
        assert!(!windows.contains_key("a1"));
    }

    #[tokio::test]
    async fn a_read_share_opens_no_window_and_evicts_none() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = page_chrome().await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        for i in 0..MAX_WINDOWS {
            assert!(b.slot(&format!("a{i}")).await.1.is_none());
        }
        let read = json!({"workspace_only": true});
        for tool in ["browser_snapshot", "browser_read"] {
            assert_eq!(
                b.run("guest", tool, read.clone()).await.unwrap_err(),
                SHARE_WINDOW
            );
        }
        assert!(log.lock().unwrap().is_empty(), "{:?}", log.lock().unwrap());
        assert!(!dir.path().join(AGENT_MARKER).exists());
        let windows = b.windows.lock().await;
        assert_eq!(windows.len(), MAX_WINDOWS);
        assert!(!windows.contains_key("guest"));
    }

    #[tokio::test]
    async fn a_read_share_reads_the_window_its_session_opened_with_write() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = page_chrome().await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        b.run("guest", "browser_snapshot", json!({})).await.unwrap();
        let opened = log.lock().unwrap().len();
        b.run("guest", "browser_snapshot", json!({"workspace_only": true}))
            .await
            .unwrap();
        let log = log.lock().unwrap();
        assert!(
            !log[opened..].iter().any(|m| m == "Target.createTarget"),
            "{log:?}"
        );
    }

    #[test]
    fn window_tabs_are_the_agent_window_and_every_page_opened_from_it() {
        let pages = [
            json!({"targetId": "agent"}),
            json!({"targetId": "user"}),
            json!({"targetId": "sametab"}),
            json!({"targetId": "blank", "openerFrameId": "agent"}),
            json!({"targetId": "popup", "openerId": "blank"}),
            json!({"targetId": "usertab", "openerId": "user"}),
        ];
        let windows = [Some(1), Some(2), Some(1), Some(3), Some(4), Some(2)];
        let pages: Vec<_> = pages.iter().zip(windows).collect();
        assert_eq!(
            window_tabs("agent", &pages),
            ["agent", "sametab", "blank", "popup"]
        );
        assert_eq!(window_tabs("gone", &pages), ["gone"]);
    }

    #[test]
    fn js_secret_is_a_string_literal_of_its_input_without_reallocating() {
        for s in ["plain", "quo\"te\\back", "line\nbreak\u{2028}", "密码", ""] {
            let lit = js_secret(s);
            assert_eq!(serde_json::from_str::<String>(&lit).unwrap(), s);
            assert_eq!(lit.capacity(), s.len() * 6 + 2);
        }
    }

    #[tokio::test]
    async fn fill_refuses_bad_arguments_and_never_starts_chrome() {
        let dir = tempfile::tempdir().unwrap();
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), 1, false);
        let both = json!({"origin": "https://a.test", "password": "p", "code": "1"});
        for (args, want) in [
            (json!({"password": "p"}), "fill needs the origin of the tab"),
            (
                json!({"origin": "null", "password": "p"}),
                "fill needs the origin of the tab",
            ),
            (both, "fill needs either a password or a code"),
            (
                json!({"origin": "https://a.test", "password": "p"}),
                "Chrome is not running in the box",
            ),
        ] {
            assert_eq!(b.fill(&args).await.unwrap_err(), want);
        }
        assert_eq!(
            b.origin().await.unwrap_err(),
            "Chrome is not running in the box"
        );
    }

    #[tokio::test]
    async fn only_a_focused_text_field_of_a_web_page_takes_a_card_value() {
        let dir = tempfile::tempdir().unwrap();
        let page = |value| ("page", "https://shop.test/checkout", value);
        let frame = |value| ("iframe", "https://js.stripe.test/elements", value);
        for (targets, want) in [
            (vec![], NO_PAGE_FOCUS),
            (vec![page("")], NO_PAGE_FOCUS),
            (
                vec![
                    page(""),
                    (
                        "page",
                        "devtools://devtools/bundled/devtools_app.html",
                        "field",
                    ),
                    ("page", "chrome://newtab/", "field"),
                    ("browser_ui", "https://omnibox.test/", "field"),
                ],
                NO_PAGE_FOCUS,
            ),
            (vec![page("field")], ""),
            (vec![page("other")], NOT_A_FIELD),
            (vec![page("frame"), frame("field")], ""),
            (vec![page("frame"), frame("other")], NOT_A_FIELD),
            (vec![page("frame")], ""),
        ] {
            let b = Browser::new(
                "/nonexistent/chrome".into(),
                dir.path().into(),
                web_chrome(&targets).await,
                false,
            );
            let got = b.page_field().await.err().unwrap_or_default();
            assert_eq!(got, want, "{targets:?}");
        }
        let idle = Browser::new("/nonexistent/chrome".into(), dir.path().into(), 1, false);
        assert_eq!(idle.page_field().await.unwrap_err(), NO_PAGE_FOCUS);
    }

    #[tokio::test]
    async fn close_records_whether_chrome_ran_and_start_follows_the_record() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(RUNNING_MARKER);
        let running = Browser::new(
            "/nonexistent/chrome".into(),
            dir.path().into(),
            fake_chrome().await,
            false,
        );
        assert_eq!(
            running
                .run("state", "browser_close", json!({}))
                .await
                .unwrap(),
            "closed"
        );
        assert!(marker.exists());
        let start = running
            .run("state", "browser_start", json!({"if_was_running": true}))
            .await;
        assert!(start.unwrap_err().contains("launch /nonexistent/chrome"));

        let idle = Browser::new("/nonexistent/chrome".into(), dir.path().into(), 1, false);
        assert_eq!(
            idle.run("state", "browser_close", json!({})).await.unwrap(),
            "not running"
        );
        assert!(!marker.exists());
        let start = idle
            .run("state", "browser_start", json!({"if_was_running": true}))
            .await;
        assert_eq!(start.unwrap(), "not running");
    }

    #[tokio::test]
    async fn the_tab_argument_acts_on_a_user_tab_and_never_on_an_agent_window() {
        let dir = tempfile::tempdir().unwrap();
        let tabs = |port| Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        let one = tabs(
            tabs_chrome(&[
                (AGENT_PAGE, "visible", "agent"),
                ("https://mail.test/", "visible", "Inbox"),
                ("https://docs.test/", "hidden", "Doc"),
            ])
            .await,
        );
        assert_eq!(
            one.run("a", "browser_tabs", json!({})).await.unwrap(),
            "Tabs the user has open; pass an id, or \"active\" for the one in front, as tab:\n\
             - tab T1 \"Inbox\" (https://t1.test/) [in front]\n\
             - tab T2 \"Doc\" (https://t2.test/)"
        );
        for (tab, want) in [
            ("active", Ok("Page: tab 1")),
            ("T2", Ok("Page: tab 2")),
            ("T0", Err("no tab T0 is open; browser_tabs lists the tabs")),
        ] {
            let got = one.run("a", "browser_snapshot", json!({"tab": tab})).await;
            assert_eq!(got.as_deref().map_err(String::as_str), want, "{tab}");
        }
        assert_eq!(
            one.run(
                "a",
                "browser_read",
                json!({"tab": "T1", "workspace_only": true})
            )
            .await
            .unwrap_err(),
            SHARE_TAB
        );
        let two = tabs(
            tabs_chrome(&[
                ("https://a.test/", "visible", "A"),
                ("https://b.test/", "visible", "B"),
            ])
            .await,
        );
        let err = two
            .run("a", "browser_snapshot", json!({"tab": "active"}))
            .await
            .unwrap_err();
        assert!(err.starts_with("2 tabs are in front"), "{err}");
        let none = tabs(tabs_chrome(&[(AGENT_PAGE, "visible", "agent")]).await);
        assert_eq!(
            none.run("a", "browser_tabs", json!({})).await.unwrap(),
            "no tab is open outside the agent windows"
        );
    }

    #[tokio::test]
    async fn without_plans_the_tab_argument_and_the_tab_list_do_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let port = slow_chrome(Duration::ZERO).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let plain = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, false);
        assert_eq!(
            plain
                .run("a", "browser_snapshot", json!({"tab": "active"}))
                .await
                .unwrap(),
            "Page: slow"
        );
        assert_eq!(
            plain.run("a", "browser_tabs", json!({})).await.unwrap_err(),
            "unknown browser tool browser_tabs"
        );
        let plans = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        assert_eq!(
            plans
                .run("a", "browser_snapshot", json!({"tab": "active"}))
                .await
                .unwrap_err(),
            "no tab of the user is in front; browser_tabs lists the tabs"
        );
    }

    #[tokio::test]
    async fn settle_ends_once_the_xhr_ended_and_the_text_stopped_growing() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = fetch_chrome("feed".into(), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        let mut cdp = b.browser().await.unwrap();
        cdp.call("Page.navigate", json!({"url": "https://a.test/spa"}))
            .await
            .unwrap();
        let asked = tokio::time::Instant::now();
        cdp.settle(asked + SPA_WAIT).await;
        assert!(asked.elapsed() < XHR_STALE, "{:?}", asked.elapsed());
        let reads = log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["params"]["expression"] == TEXT_LEN_JS)
            .count();
        assert_eq!(reads, 5, "5, 5 in flight, 5 ended, 50, 50");
    }

    #[tokio::test]
    async fn settle_stops_waiting_for_an_xhr_past_its_stale_age_or_for_text_past_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let (port, _) = fetch_chrome("loading".into(), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        for (page, waits) in [
            (
                "spa-hang",
                XHR_STALE..XHR_STALE + Duration::from_millis(500),
            ),
            ("spa-grow", SPA_WAIT..SPA_WAIT + Duration::from_millis(500)),
        ] {
            let mut cdp = b.browser().await.unwrap();
            cdp.call(
                "Page.navigate",
                json!({"url": format!("https://a.test/{page}")}),
            )
            .await
            .unwrap();
            let asked = tokio::time::Instant::now();
            cdp.settle(asked + SPA_WAIT).await;
            assert!(
                waits.contains(&asked.elapsed()),
                "{page}: {:?}",
                asked.elapsed()
            );
        }
    }

    #[tokio::test]
    async fn fetch_reads_a_page_with_text_once_at_its_load() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = fetch_chrome("article".into(), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        let out = b
            .run("a", "browser_fetch", json!({"url": "https://a.test/spa"}))
            .await
            .unwrap();
        assert_eq!(out, "Page: Doc (https://a.test/)\n\narticle");
        let log = log.lock().unwrap();
        assert!(
            !log.iter().any(
                |m| m["method"] == "Network.enable" || m["params"]["expression"] == TEXT_LEN_JS
            )
        );
        let read = format!("{READ_JS}(null)");
        let reads = log
            .iter()
            .filter(|m| m["params"]["expression"] == read.as_str())
            .count();
        assert_eq!(reads, 1);
    }

    #[tokio::test]
    async fn fetch_reads_a_loading_shell_again_until_its_content_is_in_or_its_wait_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (port, log) = fetch_chrome(String::new(), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        let asked = tokio::time::Instant::now();
        let out = b
            .run("a", "browser_fetch", json!({"url": "https://a.test/shell"}))
            .await
            .unwrap();
        assert!(asked.elapsed() < SPA_WAIT, "{:?}", asked.elapsed());
        assert_eq!(out, "Page: Doc (https://a.test/)\n\nfeed item one");
        let read = format!("{READ_JS}(null)");
        let reads = log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["params"]["expression"] == read.as_str())
            .count();
        assert_eq!(reads, 4, "shell, shell, feed, feed");
        let (port, _) = fetch_chrome("Loading…".into(), true).await;
        let b = Browser::new("/nonexistent/chrome".into(), dir.path().into(), port, true);
        let asked = tokio::time::Instant::now();
        let out = b
            .run("a", "browser_fetch", json!({"url": "https://a.test/"}))
            .await
            .unwrap();
        assert!(
            (SPA_WAIT..SPA_WAIT + Duration::from_millis(500)).contains(&asked.elapsed()),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(out, "Page: Doc (https://a.test/)\n\nLoading…");
    }

    /// A wrapper in `dir` that runs `CHROME_BIN` headless; none when no Chrome is set and CB_NO_BROWSER allows the skip.
    fn headless_chrome(dir: &std::path::Path) -> Option<String> {
        let Some(chrome) = std::env::var("CHROME_BIN").ok().filter(|b| !b.is_empty()) else {
            assert!(
                cfg!(target_os = "macos") || std::env::var_os("CB_NO_BROWSER").is_some(),
                "no Chrome found: set CHROME_BIN, or CB_NO_BROWSER=1 to skip"
            );
            eprintln!("SKIP: no Chrome; set CHROME_BIN to run");
            return None;
        };
        let wrapper = dir.join("chrome");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec \"{chrome}\" --headless=new --use-mock-keychain \"$@\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(
            &wrapper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        Some(wrapper.display().to_string())
    }

    /// Serves `page` for every request on the returned port.
    async fn page_server(page: &'static str) -> u16 {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = http.accept().await {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    page.len()
                );
                let _ = s.write_all(format!("{head}{page}").as_bytes()).await;
            }
        });
        port
    }

    #[tokio::test]
    async fn on_a_box_with_plans_listener_rows_icon_buttons_and_repeated_names_get_refs() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bin) = headless_chrome(dir.path()) else {
            return;
        };
        let url = format!("http://127.0.0.1:{}/", page_server(MAILBOX).await);
        let mut sizes = Vec::new();
        for plans in [true, false] {
            let cdp = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let b = Browser::new(
                bin.clone(),
                dir.path().join(format!("m{plans}")),
                cdp,
                plans,
            );
            let snap = b
                .run("a", "browser_navigate", json!({"url": url}))
                .await
                .unwrap();
            sizes.push(snap.len());
            let rows = snap.matches("clickable \"Sender ").count();
            if !plans {
                assert_eq!(rows, 0, "{snap}");
                assert!(snap.contains("button \"\" [ref="), "{snap}");
                continue;
            }
            assert_eq!(rows, 45, "{snap}");
            assert!(!snap.contains("snapshot truncated"), "{snap}");
            assert!(snap.contains("textbox \"To\""), "{snap}");
            assert_eq!(snap.matches("button \"Star\"").count(), 45, "{snap}");
            let resolves: Vec<&str> = snap
                .lines()
                .filter(|l| l.contains("button \"Resolve\""))
                .collect();
            assert_eq!(resolves.len(), 8, "{snap}");
            assert!(
                resolves.iter().any(|l| l.ends_with(r#" in "Card 3""#)),
                "{snap}"
            );
            let row = snap
                .lines()
                .find(|l| l.contains("clickable \"Sender 31 "))
                .and_then(|l| l.split("[ref=").nth(1))
                .and_then(|r| r.split(']').next())
                .unwrap()
                .to_owned();
            let out = b
                .run("a", "browser_click", json!({"ref": row}))
                .await
                .unwrap();
            assert!(out.contains("opened 31"), "{out}");
            b.run("a", "browser_close", json!({})).await.unwrap();
        }
        assert!(sizes[0] <= sizes[1], "{sizes:?}");
    }

    #[tokio::test]
    async fn on_a_box_with_plans_browser_type_picks_a_select_option_that_snapshots_list() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bin) = headless_chrome(dir.path()) else {
            return;
        };
        let url = format!(
            "http://127.0.0.1:{}/",
            page_server(
                "<!doctype html><title>order</title><form><label>Customer <input name=c></label><label>Item <select name=item>\
                 <option></option><option>lamp</option><option value=dk>desk</option><option disabled>sofa</option></select></label></form>",
            )
            .await
        );
        for plans in [true, false] {
            let cdp = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let b = Browser::new(
                bin.clone(),
                dir.path().join(format!("p{plans}")),
                cdp,
                plans,
            );
            let snap = b
                .run("a", "browser_navigate", json!({"url": url}))
                .await
                .unwrap();
            assert!(
                snap.contains("combobox \"Item lamp desk sofa\" [ref=e2]"),
                "{snap}"
            );
            assert_eq!(snap.contains(r#" options="lamp | desk""#), plans, "{snap}");
            if plans {
                for (text, want) in [
                    ("Desk", "selected \"desk\" in e2"),
                    ("lamp", "selected \"lamp\" in e2"),
                    ("dk", "selected \"desk\" in e2"),
                ] {
                    assert_eq!(
                        b.run("a", "browser_type", json!({"ref": "e2", "text": text}))
                            .await
                            .unwrap(),
                        want
                    );
                }
                let snap = b.run("a", "browser_snapshot", json!({})).await.unwrap();
                assert!(snap.contains(r#"[ref=e2] value="dk" options="#), "{snap}");
                let err = b
                    .run("a", "browser_type", json!({"ref": "e2", "text": "sofa"}))
                    .await
                    .unwrap_err();
                assert_eq!(
                    err,
                    r#"Error: no option of e2 is "sofa"; its options: lamp, desk"#
                );
            }
            assert_eq!(
                b.run("a", "browser_type", json!({"ref": "e1", "text": "Ada"}))
                    .await
                    .unwrap(),
                "typed 3 characters into e1"
            );
            b.run("a", "browser_close", json!({})).await.unwrap();
        }
    }

    #[tokio::test]
    async fn browser_fill_types_the_fields_picks_options_clicks_and_returns_the_page_it_opened() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bin) = headless_chrome(dir.path()) else {
            return;
        };
        let url = format!(
            "http://127.0.0.1:{}/",
            page_server(
                "<!doctype html><title>order</title><script>if (location.search) document.title = 'saved ' + location.search</script>\
                 <form><label>Customer <input name=c value=old></label><label>Item <select name=item><option></option><option>lamp</option>\
                 <option value=dk>desk</option></select></label><button>Save order</button></form>",
            )
            .await
        );
        let cdp = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let b = Browser::new(bin, dir.path().join("p"), cdp, true);
        b.run("a", "browser_navigate", json!({"url": url}))
            .await
            .unwrap();
        let out = b
            .run(
                "a",
                "browser_fill",
                json!({"fields": [{"ref": "e1", "text": "Ada"}, {"ref": "e2", "text": "desk"}], "click": "e3"}),
            )
            .await
            .unwrap();
        assert!(
            out.starts_with(&format!(
                "filled e1, e2; clicked e3 \"Save order\"; page is now saved ?c=Ada&item=dk ({url}?c=Ada&item=dk)\n"
            )),
            "{out}"
        );
        assert!(out.contains("textbox \"Customer\""), "{out}");
        b.run("a", "browser_close", json!({})).await.unwrap();
    }

    #[tokio::test]
    async fn an_alert_is_accepted_and_reported_and_one_open_before_the_call_fails_it_soon() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bin) = headless_chrome(dir.path()) else {
            return;
        };
        let url = format!(
            "http://127.0.0.1:{}/",
            page_server(
                "<!doctype html><title>draft</title><button onclick=\"alert('Draft saved')\">Save</button>\
                 <button onclick=\"setTimeout(() => alert('Saved later'), 1500)\">Later</button>",
            )
            .await
        );
        let cdp = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let b = Browser::new(bin, dir.path().join("p"), cdp, true);
        b.run("a", "browser_navigate", json!({"url": url}))
            .await
            .unwrap();
        let asked = Instant::now();
        let out = b
            .run("a", "browser_click", json!({"ref": "e1"}))
            .await
            .unwrap();
        assert!(
            asked.elapsed() < Duration::from_secs(5),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(
            out,
            format!(
                "clicked e1 \"Save\"; page is now draft ({url})\nthe page showed an alert: \"Draft saved\""
            )
        );
        let out = b
            .run("a", "browser_click", json!({"ref": "e2"}))
            .await
            .unwrap();
        assert!(!out.contains("alert"), "{out}");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let asked = Instant::now();
        assert_eq!(
            b.run("a", "browser_snapshot", json!({})).await.unwrap_err(),
            NO_ANSWER
        );
        assert!(
            (ENABLE_WAIT..ENABLE_WAIT + Duration::from_secs(1)).contains(&asked.elapsed()),
            "{:?}",
            asked.elapsed()
        );
        b.run("a", "browser_close", json!({})).await.unwrap();
    }

    #[tokio::test]
    async fn a_confirm_prompt_or_beforeunload_dialog_is_dismissed_and_fails_the_call_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let Some(bin) = headless_chrome(dir.path()) else {
            return;
        };
        for (kind, page, open, title) in [
            (
                "confirm",
                "<button onclick=\"document.title = confirm('Delete the draft?')\">Delete</button>",
                "browser_click",
                "false",
            ),
            (
                "prompt",
                "<button onclick=\"document.title = prompt('Name the draft?')\">Name</button>",
                "browser_click",
                "null",
            ),
            (
                "beforeunload",
                "<script>onbeforeunload = (e) => { e.preventDefault(); e.returnValue = ''; }</script>\
                 <button onclick=\"document.title = 'edited'\">Edit</button>",
                "browser_navigate",
                "edited",
            ),
        ] {
            let url = format!(
                "http://127.0.0.1:{}/",
                page_server(format!("<!doctype html><title>draft</title>{page}").leak()).await
            );
            let cdp = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let b = Browser::new(bin.clone(), dir.path().join(kind), cdp, true);
            b.run("a", "browser_navigate", json!({"url": url}))
                .await
                .unwrap();
            if kind == "beforeunload" {
                b.run("a", "browser_click", json!({"ref": "e1"}))
                    .await
                    .unwrap();
            }
            let asked = Instant::now();
            let err = b
                .run(
                    "a",
                    open,
                    json!({"ref": "e1", "url": format!("{url}?next")}),
                )
                .await
                .unwrap_err();
            assert!(
                asked.elapsed() < Duration::from_secs(5),
                "{kind}: {:?}",
                asked.elapsed()
            );
            assert!(
                err.starts_with(&format!("the page opened a {kind} dialog \""))
                    && err.ends_with(
                        "\" and the browser tools dismissed it; to accept one, do the action on the screen"
                    ),
                "{err}"
            );
            let asked = Instant::now();
            let snap = b.run("a", "browser_snapshot", json!({})).await.unwrap();
            assert!(
                asked.elapsed() < Duration::from_secs(2),
                "{kind}: {:?}",
                asked.elapsed()
            );
            assert!(
                snap.starts_with(&format!("Page: {title} ({url})\n")),
                "{snap}"
            );
            assert!(!snap.contains("dialog"), "{snap}");
            b.run("a", "browser_close", json!({})).await.unwrap();
        }
    }

    #[test]
    fn only_empty_or_short_text_that_says_it_is_loading_is_a_loading_shell() {
        for shell in ["", " \n", "Loading your feed…", "Home\nLOADING..."] {
            assert!(loading(shell), "{shell:?}");
        }
        let long = "Loading docks of the port".repeat(20);
        for page in ["article", &long, "Just a moment..."] {
            assert!(!loading(page), "{page:?}");
        }
    }
}
