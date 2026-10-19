//! `computer_act` on a described target: the plan executor's AT-SPI resolve, pressed by action, then a
//! unique OCR match of the target's visible text, or of the label its description names, outside window
//! titles; neither finding it, or either failing, leaves the grounding to the caller.

use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::act;
use super::desk::Desk;
use super::rpc::{self, Call, Client, DESK_DEADLINE, Output};
use super::script::Scripts;
use super::timing;

const OCR_SCALE: usize = 2;
const SETTLE: Duration = Duration::from_millis(600);
const OCR_TIMEOUT: Duration = Duration::from_secs(10);
const TESSERACT: &str = "tesseract";
const CROP: [f64; 2] = [320.0, 200.0];
const FIELD_WORDS: [&str; 5] = ["field", "input", "box", "entry", "textbox"];
const TEXT_ROLES: [&str; 3] = ["text", "label", "static"];
const TITLE_WORDS: [&str; 4] = ["tab", "title", "panel", "taskbar"];
const MIN_PREFIX: usize = 3;
const SAYS: [&str; 6] = [
    "that shows ",
    "that says ",
    "labelled ",
    "labeled ",
    "named ",
    "called ",
];
const PLACES: [&str; 13] = [
    " in ",
    " on ",
    " at ",
    " of ",
    " from ",
    " inside ",
    " next to ",
    " near ",
    " under ",
    " that ",
    " which ",
    " with ",
    " for ",
];
const ARTICLES: [&str; 3] = ["the", "a", "an"];
const ROLE_WORDS: [&str; 22] = [
    "button", "menu", "tab", "link", "field", "box", "icon", "item", "dropdown", "checkbox",
    "option", "entry", "input", "toggle", "list", "clip", "folder", "file", "label", "textbox",
    "combobox", "tool",
];

#[derive(Deserialize)]
struct Target {
    description: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct TargetArgs {
    action: String,
    target: Target,
    #[serde(default)]
    max_edge: Option<u32>,
}

/// One OCR word: its text folded to lowercase letters and digits, its line key and its box in screen pixels.
struct Word {
    norm: String,
    line: String,
    rect: [f64; 4],
}

/// Runs `computer_act` with `target`: `locate` only finds it, `click` also presses it; returns the
/// tier that found it, the point in the caller's screenshot pixels and a crop around it.
pub async fn act(
    shuttle: &Client,
    owner: &str,
    desk: &Desk,
    scripts: &Scripts,
    workspace: &Path,
    args: Value,
) -> Result<Output, String> {
    let k = act::scale(shuttle, owner, &args["session"]).await?;
    let a: TargetArgs = rpc::parse(args)?;
    let press = match a.action.as_str() {
        "click" => true,
        "locate" => false,
        other => return Err(format!("target takes action click or locate, not {other}")),
    };
    let start = Instant::now();
    let mut found = scripts
        .locate(workspace, &a.target.description, press)
        .await
        .unwrap_or_else(|e| json!({"found": false, "why": e}));
    timing::mark("a11y");
    let a11y_ms = start.elapsed().as_millis();
    let element = format!(
        "{} \"{}\"",
        found["role"].as_str().unwrap_or_default(),
        found["name"].as_str().unwrap_or_default()
    );
    let place = placed(&found, libreoffice);
    if found["found"] == true && (press || place.is_ok()) {
        let rect = place.ok();
        let point = rect.map(|[x, y, w, h]| [(x + w / 2.0) / k, (y + h / 2.0) / k]);
        let mut out = json!({
            "tier": "a11y",
            "how": found["how"],
            "element": element,
            "window": found["window"],
            "point": point,
            "scale": k,
            "pressed": press.then(|| found["via"].clone()),
            "ms": start.elapsed().as_millis(),
        });
        if let Err(why) = place {
            out["no_point"] = why.into();
        }
        if found["served"].is_array() {
            out["served"] = found["served"].take();
        }
        return Ok(finish(shuttle, owner, &mut out, rect).await);
    }
    let mut out = match place.err().filter(|_| found["found"] == true) {
        Some(why) => json!({"a11y": {"element": element, "no_point": why, "ms": a11y_ms}}),
        None => json!({"a11y": {"why": found["why"], "ms": a11y_ms}}),
    };
    if found["served"].is_array() {
        out["a11y"]["served"] = found["served"].take();
    }
    let texts = match a.target.text.as_deref().filter(|t| !norm(t).is_empty()) {
        Some(text) => vec![text.to_owned()],
        None => texts_from(&a.target.description),
    };
    if texts.is_empty() {
        return not_found(shuttle, owner, out, a.max_edge, start).await;
    }
    let t0 = Instant::now();
    let words = match ocr(shuttle, owner).await {
        Ok(words) => words,
        Err(e) => {
            out["ocr"] = json!({"error": e, "ms": t0.elapsed().as_millis()});
            return not_found(shuttle, owner, out, a.max_edge, start).await;
        }
    };
    timing::mark("ocr");
    let titles = if names_titles(&a.target.description) {
        Vec::new()
    } else {
        titled(shuttle, owner, desk, &words, &texts).await
    };
    timing::mark("titles");
    let (text, hits) = first_read(&words, &texts, |r| !titles.contains(r))
        .unwrap_or((texts[0].as_str(), Vec::new()));
    out["ocr"] = json!({"text": text, "matches": hits.len(), "ms": t0.elapsed().as_millis()});
    if a.target.text.is_none() {
        out["ocr"]["from"] = "description".into();
    }
    if !titles.is_empty() {
        out["ocr"]["titles"] = titles.len().into();
    }
    let [rect] = hits.as_slice() else {
        return not_found(shuttle, owner, out, a.max_edge, start).await;
    };
    let (cx, cy) = (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
    if let Some(why) = label_of_field(desk, &a.target.description, text, cx, cy).await {
        out["ocr"]["refused"] = why.into();
        return not_found(shuttle, owner, out, a.max_edge, start).await;
    }
    if press {
        ask(
            shuttle,
            owner,
            "desk_click",
            json!({"x": cx.round(), "y": cy.round()}),
        )
        .await?;
        tokio::time::sleep(SETTLE).await;
    }
    out = json!({
        "tier": "ocr",
        "text": text,
        "text_from": if a.target.text.is_some() { "text" } else { "description" },
        "point": [cx / k, cy / k],
        "scale": k,
        "pressed": press,
        "ocr_ms": t0.elapsed().as_millis(),
        "a11y": out["a11y"],
        "ms": start.elapsed().as_millis(),
    });
    Ok(finish(shuttle, owner, &mut out, Some(*rect)).await)
}

/// Whether `pid` runs LibreOffice, whose toolbar extents can sit a menu bar's height above the drawn controls.
pub(super) fn libreoffice(pid: i64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|c| c.trim_end() == "soffice.bin")
}

/// RGB rows of `width` x `height` as a PNG.
pub(super) fn png_rgb(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut w| w.write_image_data(rgb))
        .map_err(|e| format!("encode PNG: {e}"))?;
    Ok(out)
}

/// The box to point at for the found element, else why none: hidden, unbounded, or a LibreOffice control without a `shift`.
fn placed(found: &Value, office: impl Fn(i64) -> bool) -> Result<[f64; 4], &'static str> {
    if found["shown"] == false {
        return Err("it does not show, like an entry of a closed menu");
    }
    let [x, y, w, h] = rect_of(&found["bounds"]).ok_or("it has no bounds")?;
    let role = found["role"].as_str().unwrap_or_default();
    if role.starts_with("menu") || !found["pid"].as_i64().is_some_and(office) {
        return Ok([x, y, w, h]);
    }
    let dy = found["shift"]
        .as_f64()
        .ok_or("LibreOffice's extents can sit a menu bar's height above the drawn control")?;
    Ok([x, y + dy, w, h])
}

/// The result line and a full-resolution crop of [`CROP`] around `rect`, else no image.
async fn finish(shuttle: &Client, owner: &str, out: &mut Value, rect: Option<[f64; 4]>) -> Output {
    let image_b64 = match rect {
        Some([x, y, w, h]) => {
            let (cx, cy) = (x + w / 2.0, y + h / 2.0);
            let region = json!({"x": cx - CROP[0] / 2.0, "y": cy - CROP[1] / 2.0, "w": CROP[0], "h": CROP[1]});
            ask(shuttle, owner, "desk_look", json!({"region": region}))
                .await
                .ok()
                .and_then(|o| o.image_b64)
        }
        None => None,
    };
    if let Some(b64) = &image_b64 {
        out["crop"] = json!({"w": CROP[0], "h": CROP[1], "bytes": b64.len() / 4 * 3});
    }
    Output {
        text: out.to_string(),
        is_error: false,
        image_b64,
        ..Output::default()
    }
}

/// No tier found the target: the reasons, and the whole screen so the caller grounds it.
async fn not_found(
    shuttle: &Client,
    owner: &str,
    mut out: Value,
    max_edge: Option<u32>,
    start: Instant,
) -> Result<Output, String> {
    out["tier"] = "none".into();
    out["ms"] = json!(start.elapsed().as_millis());
    Ok(Output {
        text: format!(
            "{out}\nnot found by its accessibility name or a unique match of its text; give x,y from the screenshot"
        ),
        is_error: true,
        image_b64: ask(shuttle, owner, "desk_look", json!({"max_edge": max_edge}))
            .await?
            .image_b64,
        ..Output::default()
    })
}

/// The screen's words by tesseract on a copy scaled up [`OCR_SCALE`] times, boxes in screen pixels.
async fn ocr(shuttle: &Client, owner: &str) -> Result<Vec<Word>, String> {
    let shot = ask(shuttle, owner, "desk_shot", json!({})).await?;
    let png = B64
        .decode(shot.image_b64.ok_or("the screen shot has no image")?)
        .map_err(|e| format!("decode the screen shot: {e}"))?;
    let (w, h, rgb) = rgb_of(&png)?;
    let (sw, sh, big) = upscale(w as usize, h as usize, &rgb, OCR_SCALE);
    let path = std::env::temp_dir().join(format!("{}.png", crate::new_id("cb-ocr")));
    let png = png_rgb(sw, sh, &big)?;
    tokio::fs::write(&path, png)
        .await
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    let run = Command::new(TESSERACT)
        .arg(&path)
        .args(["-", "--psm", "11", "tsv"])
        .kill_on_drop(true)
        .output();
    let done = tokio::time::timeout(OCR_TIMEOUT, run).await;
    let _ = tokio::fs::remove_file(&path).await;
    let output = done
        .map_err(|_| format!("tesseract took over {} s", OCR_TIMEOUT.as_secs()))?
        .map_err(|e| format!("run {TESSERACT}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tesseract failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(parse_tsv(
        &String::from_utf8_lossy(&output.stdout),
        OCR_SCALE as f64,
    ))
}

fn parse_tsv(tsv: &str, scale: f64) -> Vec<Word> {
    tsv.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            let (text, conf) = (f.get(11)?.trim(), f.get(10)?.parse::<f64>().ok()?);
            let n = norm(text);
            if n.is_empty() || conf < 0.0 {
                return None;
            }
            let num = |i: usize| f.get(i).and_then(|v| v.parse::<f64>().ok());
            Some(Word {
                norm: n,
                line: f[1..5].join("."),
                rect: [
                    num(6)? / scale,
                    num(7)? / scale,
                    num(8)? / scale,
                    num(9)? / scale,
                ],
            })
        })
        .collect()
}

/// The labels a description names, most likely first: quoted text, the text after "that shows" or "labelled",
/// the words before the first place phrase without articles and role words, and their capitalised run.
fn texts_from(description: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |t: &str| {
        let t = t.trim().trim_end_matches(',').trim();
        if !norm(t).is_empty() && !out.iter().any(|o| norm(o) == norm(t)) {
            out.push(t.to_owned());
        }
    };
    for pair in description
        .split(['"', '\u{201c}', '\u{201d}'])
        .skip(1)
        .step_by(2)
    {
        add(pair);
    }
    let lower = description.to_lowercase();
    for say in SAYS {
        if let Some(i) = lower.find(say) {
            add(&description[i + say.len()..]);
        }
    }
    let head = PLACES
        .iter()
        .filter_map(|p| lower.find(p))
        .min()
        .map_or(description, |i| &description[..i]);
    let mut words: Vec<&str> = head.split_whitespace().collect();
    while words
        .first()
        .is_some_and(|w| ARTICLES.contains(&w.to_lowercase().as_str()))
    {
        words.remove(0);
    }
    while words
        .last()
        .is_some_and(|w| ROLE_WORDS.contains(&w.to_lowercase().as_str()))
    {
        words.pop();
    }
    add(&words.join(" "));
    let capital = words
        .iter()
        .skip_while(|w| !w.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit()))
        .take_while(|w| w.starts_with(|c: char| c.is_uppercase() || c.is_ascii_digit()))
        .copied()
        .collect::<Vec<_>>();
    add(&capital.join(" "));
    out
}

/// The first of `texts` read on the screen and its matches that `keep` keeps.
fn first_read<'a>(
    words: &[Word],
    texts: &'a [String],
    keep: impl Fn(&[f64; 4]) -> bool,
) -> Option<(&'a str, Vec<[f64; 4]>)> {
    texts
        .iter()
        .map(|t| {
            (
                t.as_str(),
                matches(words, t)
                    .into_iter()
                    .filter(&keep)
                    .collect::<Vec<_>>(),
            )
        })
        .find(|(_, hits)| !hits.is_empty())
}

/// The matches of `texts` that show a window title, not content: in a desktop panel, a title bar, or a
/// tab such as a browser's.
async fn titled(
    shuttle: &Client,
    owner: &str,
    desk: &Desk,
    words: &[Word],
    texts: &[String],
) -> Vec<[f64; 4]> {
    let mut out = Vec::new();
    for rect in texts.iter().flat_map(|t| matches(words, t)) {
        let (cx, cy) = (rect[0] + rect[2] / 2.0, rect[1] + rect[3] / 2.0);
        let title = ask(shuttle, owner, "desk_title_at", json!({"x": cx, "y": cy}))
            .await
            .ok()
            .and_then(|o| serde_json::from_str::<Value>(&o.text).ok())
            .is_some_and(|t| t["title"] == true);
        if title
            || desk
                .at_point(cx.round() as i32, cy.round() as i32)
                .await
                .is_ok_and(|at| in_tab(&at))
        {
            out.push(rect);
        }
    }
    out
}

/// Whether the `at` answer is a tab or the label of one, whose name starts the tab's, as Chrome adds the
/// tab's memory use; not content of the page the tab holds.
fn in_tab(at: &Value) -> bool {
    let found = &at["found"];
    let label = found["name"].as_str().unwrap_or_default();
    found["role"] == "tab"
        || at["path"]
            .as_array()
            .and_then(|p| p.last())
            .is_some_and(|p| {
                p["role"] == "tab"
                    && !label.is_empty()
                    && p["name"].as_str().is_some_and(|t| t.starts_with(label))
            })
}

/// Whether `description` names a place where window titles show, so a match there can be the target.
fn names_titles(description: &str) -> bool {
    description
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| TITLE_WORDS.contains(&w))
}

/// The boxes of every run of consecutive words on one line that spells `label`; its last word may be cut off.
fn matches(words: &[Word], label: &str) -> Vec<[f64; 4]> {
    let want: Vec<String> = label
        .split_whitespace()
        .map(norm)
        .filter(|t| !t.is_empty())
        .collect();
    if want.is_empty() {
        return Vec::new();
    }
    (0..words.len().saturating_sub(want.len() - 1))
        .filter_map(|i| {
            let run = &words[i..i + want.len()];
            let line = run.iter().all(|w| w.line == run[0].line);
            let spelled = run.iter().zip(&want).enumerate().all(|(j, (w, t))| {
                w.norm == *t
                    || (j + 1 == want.len()
                        && w.norm.len().min(t.len()) >= MIN_PREFIX
                        && (t.starts_with(&w.norm) || w.norm.starts_with(t.as_str())))
            });
            (line && spelled).then(|| {
                let x0 = run.iter().map(|w| w.rect[0]).fold(f64::MAX, f64::min);
                let y0 = run.iter().map(|w| w.rect[1]).fold(f64::MAX, f64::min);
                let x1 = run
                    .iter()
                    .map(|w| w.rect[0] + w.rect[2])
                    .fold(f64::MIN, f64::max);
                let y1 = run
                    .iter()
                    .map(|w| w.rect[1] + w.rect[3])
                    .fold(f64::MIN, f64::max);
                [x0, y0, x1 - x0, y1 - y0]
            })
        })
        .collect()
}

/// Why a match of `text` at `(x, y)` is a field's label rather than the field the description names, if it
/// is: the description names a field in words outside the text, and the point holds plain text.
async fn label_of_field(
    desk: &Desk,
    description: &str,
    text: &str,
    x: f64,
    y: f64,
) -> Option<String> {
    let own: Vec<String> = text.split_whitespace().map(norm).collect();
    let d = description.to_lowercase();
    let words: Vec<&str> = d
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !own.iter().any(|o| o == t))
        .collect();
    if !FIELD_WORDS.iter().any(|w| words.contains(w)) {
        return None;
    }
    let at = desk
        .at_point(x.round() as i32, y.round() as i32)
        .await
        .ok()?;
    let found = at.get("found").filter(|f| !f.is_null())?;
    let role = found["role"].as_str().unwrap_or_default();
    TEXT_ROLES.contains(&role).then(|| {
        format!(
            "the text is {role} \"{}\" beside a field, not the field",
            found["name"].as_str().unwrap_or_default()
        )
    })
}

/// Shuttle's answer to desk call `tool` with `args` as agent `owner`; an error answer is an error.
async fn ask(shuttle: &Client, owner: &str, tool: &str, args: Value) -> Result<Output, String> {
    let call = Call {
        agent_id: owner.to_owned(),
        tool: tool.to_owned(),
        args,
    };
    let out = shuttle.desk(&call, DESK_DEADLINE).await?;
    if out.is_error {
        return Err(out.text);
    }
    Ok(out)
}

/// The size and RGB rows of PNG `png`.
fn rgb_of(png: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec
        .read_info()
        .map_err(|e| format!("decode the screen shot: {e}"))?;
    let mut buf = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or("the screen shot is too large")?
    ];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("decode the screen shot: {e}"))?;
    let channels = info.color_type.samples();
    let rgb = buf[..info.buffer_size()]
        .chunks_exact(channels)
        .flat_map(|px| match channels {
            1 | 2 => [px[0]; 3],
            _ => [px[0], px[1], px[2]],
        })
        .collect();
    Ok((info.width, info.height, rgb))
}

fn rect_of(v: &Value) -> Option<[f64; 4]> {
    let b = v.as_array()?;
    let n = |i: usize| b.get(i).and_then(Value::as_f64);
    Some([n(0)?, n(1)?, n(2)?, n(3)?])
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// `rgb` of `w` x `h` scaled up `f` times, bilinear.
fn upscale(w: usize, h: usize, rgb: &[u8], f: usize) -> (u32, u32, Vec<u8>) {
    let (bw, bh) = (w * f, h * f);
    let mut out = vec![0u8; bw * bh * 3];
    for y in 0..bh {
        let sy = ((y as f64 + 0.5) / f as f64 - 0.5).max(0.0);
        let (y0, fy) = (sy.floor() as usize, sy.fract());
        let y1 = (y0 + 1).min(h - 1);
        for x in 0..bw {
            let sx = ((x as f64 + 0.5) / f as f64 - 0.5).max(0.0);
            let (x0, fx) = (sx.floor() as usize, sx.fract());
            let x1 = (x0 + 1).min(w - 1);
            for c in 0..3 {
                let p = |xx: usize, yy: usize| f64::from(rgb[(yy * w + xx) * 3 + c]);
                let top = p(x0, y0) * (1.0 - fx) + p(x1, y0) * fx;
                let bottom = p(x0, y1) * (1.0 - fx) + p(x1, y1) * fx;
                out[(y * bw + x) * 3 + c] = (top * (1.0 - fy) + bottom * fy).round() as u8;
            }
        }
    }
    (bw as u32, bh as u32, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(text: &str, line: &str, x: f64) -> Word {
        Word {
            norm: norm(text),
            line: line.to_owned(),
            rect: [x, 10.0, 20.0, 8.0],
        }
    }

    #[test]
    fn matches_a_label_on_one_line_and_a_cut_last_word() {
        let words = [
            word("Default", "1.1.1.1", 0.0),
            word("Paragraph", "1.1.1.1", 30.0),
            word("St", "1.1.1.1", 60.0),
            word("Save", "1.1.2.1", 0.0),
            word("Save", "1.1.3.1", 50.0),
        ];
        assert_eq!(matches(&words, "Default Paragraph").len(), 1);
        assert_eq!(matches(&words, "Save").len(), 2);
        assert!(matches(&words, "Paragraph Save").is_empty());
        assert_eq!(matches(&words, "Insert Image...").len(), 0);
    }

    #[test]
    fn a_short_label_needs_the_whole_word() {
        let words = [word("Okay", "1.1.1.1", 0.0), word("Google", "1.1.2.1", 0.0)];
        assert!(matches(&words, "OK").is_empty());
        assert!(matches(&words, "Go").is_empty());
        assert_eq!(matches(&words, "Okay").len(), 1);
        assert_eq!(matches(&words, "Goog").len(), 1);
    }

    #[test]
    fn a_libreoffice_control_outside_a_menu_is_placed_by_its_shift_else_has_no_point() {
        let office = |pid: i64| pid == 7;
        let at = |role: &str, pid: Option<i64>, shift: Value| json!({"role": role, "pid": pid, "shown": true, "bounds": [10, 20, 30, 30], "shift": shift});
        assert!(placed(&at("button", Some(7), Value::Null), office).is_err());
        assert_eq!(
            placed(&at("button", Some(7), json!(25)), office),
            Ok([10.0, 45.0, 30.0, 30.0])
        );
        assert_eq!(
            placed(&at("button", Some(7), json!(0)), office),
            Ok([10.0, 20.0, 30.0, 30.0])
        );
        assert_eq!(
            placed(&at("menuitem", Some(7), Value::Null), office),
            Ok([10.0, 20.0, 30.0, 30.0])
        );
        assert_eq!(
            placed(&at("button", Some(8), Value::Null), office),
            Ok([10.0, 20.0, 30.0, 30.0])
        );
    }

    #[test]
    fn a_hidden_or_unbounded_element_has_no_point() {
        let none = |_: i64| false;
        let closed = json!({"role": "menuitem", "shown": false, "bounds": [0, -400, 80, 20]});
        let unbounded = json!({"role": "button", "shown": true, "bounds": null});
        let drawn = json!({"role": "button", "shown": true, "bounds": [1, 2, 3, 4]});
        assert!(placed(&closed, none).is_err());
        assert!(placed(&unbounded, none).is_err());
        assert!(placed(&drawn, none).is_ok());
    }

    #[test]
    fn a_description_names_the_text_to_read() {
        let first = |d: &str| texts_from(d).into_iter().next();
        assert_eq!(
            first("the Data menu in the menu bar").as_deref(),
            Some("Data")
        );
        assert_eq!(
            first("the paragraph style box that shows Default Paragraph Style").as_deref(),
            Some("Default Paragraph Style")
        );
        assert_eq!(
            first("the slide Format dropdown that shows Screen 16:9").as_deref(),
            Some("Screen 16:9")
        );
        assert_eq!(
            first("Rename... in the right-click menu").as_deref(),
            Some("Rename...")
        );
        assert_eq!(
            first("the crab.mp4 clip in the timeline").as_deref(),
            Some("crab.mp4")
        );
        assert_eq!(
            first("the hide all link in the Property Browser").as_deref(),
            Some("hide all")
        );
        assert_eq!(first("the interview folder").as_deref(), Some("interview"));
        assert_eq!(first("the Save button").as_deref(), Some("Save"));
        assert_eq!(
            first("the button labelled \"Save as PDF\"").as_deref(),
            Some("Save as PDF")
        );
        assert!(texts_from("the slide Format dropdown").contains(&"Format".to_owned()));
        assert!(texts_from("the button").is_empty());
    }

    #[test]
    fn the_first_text_read_on_screen_decides() {
        let words = [
            word("Slide", "1.1.1.1", 0.0),
            word("Show", "1.1.1.1", 30.0),
            word("Format", "1.1.2.1", 0.0),
            word("Format", "1.1.3.1", 0.0),
        ];
        let texts = [
            "Nothing".to_owned(),
            "Slide Show".to_owned(),
            "Format".to_owned(),
        ];
        let all = |_: &[f64; 4]| true;
        let (text, hits) = first_read(&words, &texts, all).unwrap();
        assert_eq!((text, hits.len()), ("Slide Show", 1));
        let (text, hits) = first_read(&words, &texts[2..], all).unwrap();
        assert_eq!((text, hits.len()), ("Format", 2));
        assert!(first_read(&words, &texts[..1], all).is_none());
    }

    #[test]
    fn a_title_copy_does_not_count_so_the_content_copy_is_unique_and_a_title_only_text_reads_on() {
        let at = |text: &str, line: &str, y: f64| Word {
            norm: norm(text),
            line: line.to_owned(),
            rect: [100.0, y, 40.0, 10.0],
        };
        let words = [
            at("house", "1.1.1.1", 12.0),
            at("house", "1.1.2.1", 38.0),
            at("Save", "1.1.3.1", 12.0),
            at("Save", "1.1.4.1", 300.0),
        ];
        let content = |r: &[f64; 4]| r[1] > 51.0;
        let texts = ["house".to_owned(), "Save".to_owned()];
        assert_eq!(first_read(&words, &texts, |_| true).unwrap().1.len(), 2);
        let (text, hits) = first_read(&words, &texts, content).unwrap();
        assert_eq!(
            (text, hits.as_slice()),
            ("Save", &[[100.0, 300.0, 40.0, 10.0]][..])
        );
        assert!(first_read(&words, &texts[..1], content).is_none());
    }

    #[test]
    fn a_tab_or_its_label_is_a_title_but_the_page_under_a_tab_is_not() {
        let title = "hit: house @520,300";
        let chrome = json!({"found": {"role": "text", "name": title}, "path": [
            {"role": "window", "name": title}, {"role": "tablist", "name": ""},
            {"role": "tab", "name": format!("{title} - Memory usage - 51.7 MB")}]});
        let tab = json!({"found": {"role": "tab", "name": "Numbers"}, "path": [{"role": "dialog", "name": "Format Cells"}]});
        let page = json!({"found": {"role": "spinbutton", "name": "Decimal places:"}, "path": [
            {"role": "dialog", "name": "Format Cells"}, {"role": "tab", "name": "Numbers"}, {"role": "group", "name": "Options"}]});
        let label = json!({"found": {"role": "label", "name": "Show hidden files"}, "path": [
            {"role": "dialog", "name": "Preferences"}, {"role": "tab", "name": "Display"}]});
        assert!(in_tab(&chrome));
        assert!(in_tab(&tab));
        assert!(!in_tab(&page));
        assert!(!in_tab(&label));
        assert!(!in_tab(&json!({"found": null, "window": {"title": title}})));
    }

    #[test]
    fn a_description_that_names_a_tab_title_or_panel_lets_titles_match() {
        assert!(names_titles("the Numbers tab"));
        assert!(names_titles("the Untitled 1 button in the taskbar"));
        assert!(names_titles("the window title"));
        assert!(names_titles("the Applications menu in the top panel"));
        assert!(!names_titles(
            "the yellow star in the top-right corner of the game"
        ));
        assert!(!names_titles("the tabular data view"));
    }

    #[test]
    fn parse_tsv_keeps_words_and_scales_boxes_back() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   5\t1\t2\t1\t1\t1\t100\t40\t60\t20\t91.5\tCancel\n\
                   4\t1\t2\t1\t1\t0\t100\t40\t60\t20\t-1\t\n";
        let words = parse_tsv(tsv, 2.0);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].norm, "cancel");
        assert_eq!(words[0].rect, [50.0, 20.0, 30.0, 10.0]);
    }

    #[test]
    fn upscale_doubles_the_size_and_keeps_flat_colour() {
        let rgb = vec![10u8; 4 * 3 * 3];
        let (w, h, big) = upscale(4, 3, &rgb, 2);
        assert_eq!((w, h), (8, 6));
        assert!(big.iter().all(|v| *v == 10));
    }

    #[test]
    fn png_rgb_round_trips_through_rgb_of() {
        let rgb = [1, 2, 3, 4, 5, 6];
        assert_eq!(
            rgb_of(&png_rgb(2, 1, &rgb).unwrap()).unwrap(),
            (2, 1, rgb.to_vec())
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_soffice_bin_process_counts_as_libreoffice() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("soffice.bin");
        std::fs::copy("/bin/sleep", &bin).unwrap();
        let mut office = std::process::Command::new(&bin).arg("30").spawn().unwrap();
        let pid = office.id().into();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !libreoffice(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            libreoffice(pid),
            "the soffice.bin copy never showed in /proc/{pid}/comm"
        );
        assert!(!libreoffice(std::process::id().into()));
        office.kill().unwrap();
        office.wait().unwrap();
    }
}
