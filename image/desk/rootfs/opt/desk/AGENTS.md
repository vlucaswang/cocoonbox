# Desk scripts on this box

This box is a Linux desktop (Ubuntu 24.04, XFCE, 1280x800, user `user`, home `/home/user`).
LibreOffice 24.2 (Calc, Writer, Impress) is installed; office files open in it, `.txt` in Mousepad.
Desk scripts drive it from inside: you write a script, the box runs it next to the screen, and
you get its result, its logs and a screenshot back. You plan; the script only executes.

- Runtime: Node 24, which runs TypeScript with its types stripped (no build step).
- API: `import * as desk from '/opt/desk/lib/desk.mts'`. Every call is async: `await` it.
  The source is short; read `/opt/desk/lib/desk.mts` for the exact signatures.
- The same calls work from a shell: `desk VERB key=value ...` prints JSON, for example
  `desk snapshot window=Preferences text=true` or `desk press id=1.27/379`.

<!-- routing -->

## The desk API

Windows and elements come from the accessibility tree (AT-SPI), roles named as in ARIA.

| Call | Does |
|---|---|
| `windows()` | top-level windows `{id, title, role, pid, active}`; `focused()` the active one; a modal dialog the screen does not show also has `modal` (`hidden`, `off the screen` or `behind "TITLE"`) and `blocks`, the title of the window it blocks |
| `wait(since, timeout_ms)` | waits until the window list differs from `since` (a `windows()` result), at most `timeout_ms`: `{changed, windows}` |
| `snapshot(window?, {pid?, keys?})` | the elements of one window (default: the focused one) in tree order: `{ref, id, parent, depth, role, name, states, value?, bounds?}`; `states` holds `checked`, `disabled`, `focused`, `showing`, `selected`, `expanded` … |
| `outline(window?)` | the same tree as text, one `- role "name" [checked] [id=ID]: "value"` line per element |
| `find(name, {role?, window?})` | the elements named `name` (else containing it), ignoring case and punctuation |
| `read(id, {text?})` | one element read again; `text: true` returns its whole text as `value` |
| `press(id)` | presses an element as a user would: selects a tab or list row, opens the menus above a hidden menu entry, clicks a shown control, else runs its accessibility action |
| `act(id, 'click' \| 'action' \| 'select' \| 'focus')` | one of those ways only |
| `at(x, y, window?)` | the element at a screen point and the elements above it |
| `key('ctrl+shift+s')`, `typeText('text')`, `click(x, y, {button?, count?})` | desktop input to the focused window; `typeText` turns each newline into Return and returns once the focused field holds the text (at most 0.1 s more) |
| `focus(window)`, `close(window)`, `launch(app)` | bring a window to the front, ask it to close as its close button does, start an app |
| `screen()`, `shot({x?, y?, w?, h?})` | the screen size; a PNG `Buffer` of a screen rectangle |
| `decide(state, {key: yes(q) \| choice(q, options)})` | a fast judgment over text or JSON state (about 0.3 s): `yes` answers `{probability}`, `choice` answers `{choice, confidence, probabilities}` |
| `ask(prompt, png \| [png])` | a vision model's answer about screenshots |
| `ground(png, target, {left?, top?, width, height})` | the screen point of a described element in a screenshot of that area |

Element ids stay valid while the element exists; take a new snapshot after a window changes.

Shapes that scripts get wrong:

- `snapshot()` returns `{window, nodes, truncated, ms}`: loop over `s.nodes`. A node has `bounds`
  only when you ask for them: `snapshot(title, {bounds: true})`.
- `press`, `read` and `act` take a node's `id` (such as `1.17/62`, `[id=1.17/62]` in an outline).
  A node's `ref` is only its position in that one snapshot, and no call takes it. To act on a named
  element: `const {found} = await desk.find('Save', {role: 'button'}); await desk.press(found[0].id)`.
- "UnknownObject" or "bad element id": the element is gone (a list refreshed, a menu closed, a
  folder changed). `find` it again.
- `windows()` also lists untitled panel and desktop windows, and an app's title changes as it
  works (Thunar `inbox - Thunar` becomes `docs - Thunar`; Mousepad adds `*` to an unsaved file).
  A call that takes a window title looks it up each time; a title that no longer matches finds the
  window that ends with its app part (` - Thunar`) when it is the app's only or active window, else
  the error lists them; or look the title up again: `(await desk.windows()).find((w) => w.title.endsWith(' - Thunar'))`.

## GUI plans and saved flows

`gui_plan` runs `/opt/desk/lib/gui_plan.mts` on this API: a list of steps (app, menu, click,
check, set, type, key, wait, verify, close), each resolved on the accessibility tree and checked,
stopping at the first surprise. A passing plan that starts with an app step is saved as
`/workspace/flows/<name>.json` (a git repository, with the element each step hit) and listed in
`/workspace/flows/INDEX`; `gui_plan {flow, params}` replays it without a model call while the
saved elements still match. `gui_plan {save: name}` alone saves the plan that just ran, or the
steps that passed before an escalation, without running them again. Every run's steps and timings
go to `/workspace/flows/.runs.jsonl`. Saved flows and scripts in `/workspace/flows` follow the user
to each of their boxes of this kind, and `cocoonbox flows` exports and imports them on the Mac.
`gui_plan {flows: true}` lists the saved flows. When a saved flow escalates, handle the reason,
then continue with `gui_plan` from the failing step while that screen is still up, so the fixed
run is saved.

## Run a script

Call `script_run` with `code` (an ES module) or `script` (a file in the box, relative to
`/workspace`), and `params`. When the module exports `main(params)`, it is called with them and
its return value comes back as `result`. `timeout_s` defaults to 90, at most 110; the run is
stopped after it. The reply is JSON `{result, logs, error?: {kind: exception|timeout|interrupted,
message}, duration_ms, exit_code, stderr, snapshot?, screenshot_path}` and a screenshot taken after
the run. `logs` is the script's stdout. Set `globalThis.$snapshot` to a string, such as an outline,
to get it back as `snapshot`. Nothing persists between runs.

From a shell in the box: `node /workspace/flows/x.mts`; `main` is not called for you there.

```ts
// /workspace/flows/save-note.mts
import * as desk from '/opt/desk/lib/desk.mts'

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))
// poll a condition every 50 ms, fail after `ms`
async function until(what: string, ok: () => Promise<boolean>, ms = 5000) {
  for (const end = Date.now() + ms; !(await ok()); await sleep(50)) if (Date.now() > end) throw new Error(`no ${what} within ${ms} ms`)
}
const title = async () => (await desk.focused())?.title ?? ''

export async function main(p: { path?: string; text?: string } = {}) {
  const { path = '/home/user/note.txt', text = 'hello' } = p
  const win = async () => (await desk.windows()).find((w) => w.title.endsWith('Mousepad'))
  if (!(await win())) await desk.launch('mousepad')
  await until('Mousepad window', async () => !!(await win()))
  await desk.focus((await win())!.title)
  const before = await title()
  await desk.key('ctrl+n')
  await until('new document', async () => (await title()) !== before)
  await desk.typeText(text)
  await desk.key('ctrl+s')
  await until('Save As dialog', async () => (await title()) === 'Save As')
  await desk.key('home')
  await desk.key('shift+end')
  await desk.typeText(path)
  for (let i = 0; i < 3 && (await title()) === 'Save As'; i++) {
    await desk.key('return')
    await until('saved file', async () => (await title()) !== 'Save As', 400).catch(() => {})
  }
  return { saved: await title() }
}
```

Save a script that works as `/workspace/flows/<name>.mts` and replay it with new params:
`script_run {script: "flows/save-note.mts", params: {path: "/home/user/b.txt", text: "bye"}}`.

## Write scripts that hold up

- Find elements on the accessibility tree first; ground with a screenshot only what has no name.
- After an action that changes state, look again (`read`, a snapshot, `decide` or `ask`) instead
  of assuming it worked.
- Wait for the condition the next step needs (a window title, an element state) by polling it;
  a fixed sleep is either too short or wasted time.
- Open dialogs by their keyboard shortcut (`ctrl+s`, `ctrl+shift+s`): more reliable than toolbar clicks.
- Get one step working, then join the steps into a flow.

## Finish the work

- Do what the user, a reviewer or the brief asks; name a doubt in your answer instead of refusing.
- A number you add takes the format of the numbers next to it (decimals, separators, units).
- Before the final answer, check the whole output against each point of the request, not only the
  part you changed last.
- `shell` stops a command after 120 s by default and resets its session: pass `timeout_s` (at most
  600) for a longer install, build or conversion.

## Traps on this desktop

- A modal dialog opened by an element's accessibility action blocks its app, and the first key
  then deadlocks it. `press` clicks a shown control instead; for a dialog behind a hidden entry,
  use its shortcut.
- When an app hangs, its accessibility calls fail after 3 s with "AT-SPI peer … is not responding"
  and the app is skipped for 10 s. Close its window with `close` (or kill its pid) and reopen it;
  more input does nothing.
- A dialog that ignores clicks and keys while its app still answers sits under a newer dialog of
  the same app: close the newest dialog first.
- GTK file dialogs: type absolute paths (`/home/user/...`); `~` is not expanded. The first Return
  may only close the path-completion popup; press it again while the dialog is still in front.
- Save As for a file that already has a name selects only its stem, so typed text keeps the old
  extension. Press Home, then Shift+End, before typing a new path.
- Thunar by keyboard: `ctrl+l`, an absolute path and Return open a folder; `ctrl+s` (Select by
  Pattern, such as `*.png`) selects files; `ctrl+x` then `ctrl+v` in the target folder moves them;
  `ctrl+shift+n` makes a folder; `F2` renames the selected file, and with several files selected
  it opens Rename Multiple Files (search and replace, numbering) to rename them all at once.
- Thunar moves: do them in one `gui_plan` (Select by Pattern, `ctrl+x`, the path bar, `ctrl+v`).
  In a script, give the file view the focus after the pattern dialog and before `ctrl+x`: `act`
  its `table` element with `'focus'` (a click changes the selection). First close a leftover
  "File Operation Progress" or "already exists" dialog.
- LibreOffice 24.2 crashes once the accessibility tree of a dialog such as Format Cells has been read
  in several of its openings (the 6th to 9th, also with one snapshot per opening). Work in its
  dialogs by keys (`ctrl+Page_Down` for the next tab), read one only when no key does the step, and
  save the document before you open its dialogs again and again.
- Menu toggles such as Mousepad's Word Wrap publish no checked state; check them in the app's
  preferences dialog or with `gsettings`.
- A snapshot of a Chrome window holds the browser's own controls and the start of the page;
  page content goes to the `browser_*` tools.
- Apps a script launches keep running after it ends, also after a timeout.
- A document with layout (slides, a spreadsheet, a formatted text) keeps it only when edited in its
  app; a library that rebuilds the file from code drops what it does not model.
- The box's user can see the screen: keep windows tidy and close the dialogs you open.
