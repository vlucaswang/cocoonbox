# cocoonbox-agent

The in-box agent of a cocoonbox. One binary, two subcommands:

- `serve` runs as `cbagent`. It serves `cocoonbox.agent.v1.Agents` and `Session` over h2c gRPC on `AGENT_LISTEN` and runs the agent loop. The same listener also answers MCP Streamable HTTP at `/mcp` (mode B). It keeps state in `/var/lib/cocoonbox/agent.db`.
- `toolhost` runs as the desktop user `user`. It executes every tool behind `/run/cocoonbox/toolhost.sock`.

## Layers

The agent follows the layering in `docs/boundaries.md`:

```
transports: server.rs (gRPC Agents, Session) · mcp.rs (/mcp)
        │ runner.rs (loop, mode A) and mcp.rs both call dispatch::Dispatcher
        ▼
dispatch.rs: records every call and result (journal.rs), publishes ComputerAction, injects the desktop session
        │ Tools interface (dispatch::Tools; production impl: toolhost RPC client)
        ▼
toolhost: shell · files · CDP browser · cua-driver
```

- The loop reaches models only through `llm::Model`. Its production implementation is `llm::Client`, which speaks the chat wire to the gw URL.
- `server::serve` is the composition root: it builds the Model and Tools implementations and hands them to `Runtime::open`.
- Nothing below the dispatcher knows whether mode A or mode B issued a call. The agent knows nothing about how its machine is scheduled.

## Modules

| File | Role |
|---|---|
| `server.rs` | gRPC services behind the edge check of `identity.rs` |
| `identity.rs` | Box identity from `/run/cocoonbox-identity/identity` and the `x-cocoonbox-credential` check |
| `dispatch.rs` | The one tool path: `Tools` interface, `Dispatcher` (record call and result, ComputerAction, driver-session injection) |
| `journal.rs` | Store plus event bus: every transcript and status write is persisted, then published |
| `runner.rs` | Per-agent turn queue, the main agent loop, take-over approvals, cancel |
| `computer.rs` | Computer subagent: a vision model driving the desktop through `computer_state` / `computer_act` |
| `llm/` | Streaming gw client. `openai.rs` is chat completions (SSE) and serves every model by default. `anthropic.rs` is Messages, used for `anthropic/` models only when `GW_ANTHROPIC_WIRE=messages`. `sse.rs` is the decoder |
| `store.rs` | SQLite tables: agents, turns, transcript entries (global monotonic `seq`), client-nonce dedupe |
| `bus.rs` | Event fan-out. Offsets start at boot time in µs. A bounded replay ring serves `Events(from_offset)` |
| `mcp.rs` | MCP Streamable HTTP at `/mcp` for external agents (mode B), next to gRPC on the same listener |
| `screen.rs` | Raw RFB relay to `127.0.0.1:5900` after the `open` frame |
| `toolhost/` | `rpc.rs` (u32-length-prefixed JSON, one call per connection), `shell.rs`, `files.rs`, `browser.rs` (Chrome over CDP), `cua.rs` (cua-driver socket client) |
| `prompts.rs`, `tools.rs` | System prompts and tool schemas |

## Behaviour

- **Turns.** `Send` persists the user entry, then queues the turn. Each agent runs its turns in order. A `client_nonce` seen before returns the first turn id, and `Create` is deduplicated the same way. `Cancel` aborts the running turn: the in-flight model request is dropped, and the toolhost connection closes, which kills the shell command. The turn then records a `TurnEnd` with `ok=false`.
- **Transcript and streaming.** Each text or reasoning block streams live as `TextDelta` / `ReasoningDelta` events carrying `(turn_id, block)`. `block` numbers the blocks of a turn. When a block completes, it is recorded once as a `TranscriptEntry` whose `TextBlock` / `ReasoningBlock` carries the same `block`. Deltas take event offsets but are not replayed; a reconnecting client gets the full blocks. Tool calls and tool results are recorded as they happen. Subagent text, reasoning and tool calls carry `subagent: "computer"`, and the subagent's report is the `delegate_computer` result, which carries the final screenshot.
- **Images.** A main model with vision (`anthropic/`, `google/gemini`, `openai/gpt-4o`, `openai/gpt-5` or a `-vl` slug) gets a read-only `screenshot` tool, and it receives the delegate's final screenshot. Any other main model gets a text note instead. The vision subagent receives screenshots as image parts that follow the tool results.
- **Shell.** Each agent has one persistent bash session with cwd `/workspace`. Commands are sourced from a script file with stdin set to `/dev/null`. Output is capped to about 30 KB (head and tail). The default timeout is 120 s, up to 600 s. A timeout or cancel kills the session's process group, and the next call starts a fresh session.
- **File paths.** A relative path resolves under `/workspace`. An absolute path is used as given, so the uid `user` is the access boundary.
- **Browser.** The toolhost attaches to Chrome on `127.0.0.1:9222`, or launches it with the `/home/user/.config/cocoonbox-chrome` profile. A snapshot is a ref-tagged list (`- link "Learn more" [ref=e1]`) of at most 300 lines. It includes open shadow roots but not iframes. Clicks are CDP mouse events at the element's center.
- **Computer.** Each `delegate_computer` task runs in its own cua-driver session (`cu-…`). Every call of the task carries that label. `end_session` runs when the task ends, or from a detached task if a cancel drops it. `computer_state` returns the desktop screenshot, the window list and the AT-SPI elements of one window. `computer_act` targets an element by index from the latest state, or screen pixels. Keyboard input with neither goes to the frontmost app window at the moment of the action, and `foreground: true` escalates delivery. A fresh screenshot follows every action, and a `ComputerAction` event is published before each input. For `anthropic/` models on the chat wire, the system prompt and the newest user message carry `cache_control`. Screenshots are pruned in batches (above 6, keep 3) so the cached prefix survives most steps.

## MCP (mode B)

External agents, such as local coding agents, use the box through `POST|GET|DELETE /mcp`, with protocol 2025-03-26, 2025-06-18 or 2025-11-25. Every request needs the edge credential, which the edge adds; refusals are 401 and, before the box has an identity, 503.

- `initialize` returns an `Mcp-Session-Id`. The session appears as an agent `mcp:<clientInfo.name>` whose status is RUNNING while the session lives.
- Each tool call and its result go into that agent's transcript and onto `Session.Events`. `computer_act` also publishes a `ComputerAction` event.
- Tools: `shell`, `read_file`, `write_file`, `edit_file`, `list_dir`, `browser_navigate`, `browser_snapshot`, `browser_click`, `browser_type`, `browser_read`, `computer_state`, `computer_act`, `screenshot`. They run in the toolhost exactly as the loop's calls do.
- Screenshots come back as MCP `image` content (PNG). `max_edge` scales a screenshot, and pixel coordinates then refer to the scaled image: cua-driver maps them back to the screen.
- Each MCP session owns one cua-driver session label. It is ended by `DELETE`, by closing the GET stream (a later call starts a new label), or after 10 min without activity, which also closes the MCP session.
- The toolhost holds one input lock per display, so a `computer_act` from mode A and one over MCP never interleave.
- An `mcp:*` agent whose session has closed is deleted, together with its transcript, once it has gone untouched for 24 h.
- Unknown methods get JSON-RPC `-32601` with HTTP 200. Without a session this applies to everything except `tools/*`, which needs `Mcp-Session-Id`.
- POST answers with `application/json`. GET is an SSE stream that carries only keepalive comments, because the server sends no requests of its own.

## State (object storage)

`cocoonbox.agent.v1.State` is called by the api only. It is served on the same listener behind the same edge check.

- **Plan.** The toolhost walks `/home/user` and `/workspace` as `user`, applying the exclude list in `state::excludes()`: caches, Chrome cache dirs and runtime files, `-shm` files and restore leftovers.
  - It streams every file to `serve`, which splits the bytes with FastCDC (256 KiB / 1 MiB / 4 MiB).
  - Chunk id = hex HMAC-SHA256. Each chunk is compressed with zstd, then sealed with AES-256-GCM using a random nonce and the id as associated data. Both keys are derived from the per-request user key, which is never stored.
  - SQLite files are read through the online backup API when possible, and their `-wal` and `-journal` sidecars are then left out. `agent.db` goes through `Store::backup_to`.
  - Under LIVE, a file that changes during two reads is recorded in `skipped`. QUIESCED pauses new turns (it waits up to 30 s for a running one and answers `UNAVAILABLE` otherwise) and closes Chrome through CDP; Chrome restarts afterwards.
  - New chunks and the sealed manifest are spooled under `/var/lib/cocoonbox/state` until Upload. Chunks the last upload already held under the same key are not spooled.
- **Upload.** It PUTs the requested chunks (8 concurrent, 3 attempts) and streams `TransferProgress`. It may span several calls: only the call that carries `manifest` writes it (last), finishes with `manifest_id == snapshot_id`, and unpins the snapshot. A Plan without `parent` spools every chunk; with one, chunks the last upload already held are not spooled.
- **Restore.** A bidi stream: the first message carries the key and the manifest URL, later ones pages of chunk URLs (2048 at most). Fetching starts with the first page, and the next page is read only when the queue runs low. After the last page, every chunk the manifest references must have had a URL (`no url for chunk` otherwise).
  - It downloads and verifies every chunk (authentication and id) before touching anything. Then it pauses turns, closes Chrome and writes each root into a staging dir as `user`.
  - Each top-level entry is swapped in with one rename; entries missing from the snapshot are removed, except excluded top-level dirs such as `.cache`.
  - The desktop session (`cocoonbox-session.service`, which also stops and starts cua-driver) is stopped for the swaps and started after them, under the contract's polkit grant. Xorg and VNC stay up.
  - `agent.db` is restored through the SQLite backup API, then open turns are marked interrupted.

## Limits

- Queued turns and pending take-over approvals live in memory. After a restart, every unfinished turn gets a `TurnEnd` with the error "interrupted: the agent restarted".
- Screenshots are stored in `agent.db` with every subagent tool result, about 50–100 KB each, and there is no retention policy yet.
- The browser snapshot skips iframes. A click is a CDP mouse event at the element's center, so a covered element gets the click on whatever covers it.
- On XFCE, a background element click on the Applications toggle has no effect. The subagent is told to trust only screenshots and to retry once with `foreground: true`.
- Delta events are not coalesced: one gw SSE chunk produces one event. A slow client that falls behind the live channel gets `DATA_LOSS` and reconnects with `from_offset`.
- The main agent's history from earlier turns keeps no images.
- Directory mtimes are not restored. If the desktop session cannot be stopped (grant missing), Restore aborts after staging and leaves `.cb-restore-*` dirs behind.
- A QUIESCED plan waits a fixed 30 s for a running turn; that path has no test.
- Plan spools every new chunk to disk, so the first snapshot needs free space about the size of the compressed state.

## Environment

These keys come from `/etc/cocoonbox/agent.env` (see `docs/contracts.md`): `GW_BASE_URL`, `GW_API_KEY`, `MODEL_MAIN`, `MODEL_COMPUTER` and `AGENT_LISTEN`.

Optional overrides with contract defaults, used by tests and non-standard layouts. Boxes set none of them; the identity pair moves the trust anchor, so only whoever writes the agent's environment, already root, can change it:

| Key | Default |
|---|---|
| `CB_STATE_DIR` | `/var/lib/cocoonbox` |
| `CB_IDENTITY_PATH` | `/run/cocoonbox-identity/identity` |
| `CB_IDENTITY_OWNER` | `0` (root): the uid that must own the identity file and its directory |
| `TOOLHOST_SOCKET` | `/run/cocoonbox/toolhost.sock` |
| `VNC_ADDR` | `127.0.0.1:5900` |
| `CB_WORKSPACE` | `/workspace` |
| `CHROME_BIN` | `google-chrome` |
| `CHROME_PROFILE` | `~/.config/cocoonbox-chrome` |
| `CUA_DRIVER_SOCKET` | `~/.cache/cua-driver/cua-driver.sock` |
| `SHUTTLE_SOCKET` | unset; when set, shuttle's desk endpoint, which answers the toolhost's desk verbs, model calls and runs |
| `GW_ANTHROPIC_WIRE` | `chat` (`messages` switches `anthropic/` models to `/v1/messages`) |

## Build

```
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
./build-release.sh        # cargo build --release --locked with path remapping; prints the sha256
```

`build-release.sh` remaps the target dir, `CARGO_HOME` and the source dir, so the same commit builds to the same bytes in any directory.

`protoc` must be on `PATH`. The protos are read from `../proto`.
