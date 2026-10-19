# cocoonbox — design and execution plan

Status: draft v1, 2026-09-30.

cocoonbox gives each user a persistent cloud computer with an agent living in it, driven by any model through our gateway, with a native macOS app to watch, talk to, take over, and connect accounts and the local Mac. Meta's Muse is a design reference only: no code is copied.

## 1. Stack selection

| Part | Choice | Why | Rejected |
|---|---|---|---|
| macOS app | **Swift, SwiftUI + AppKit**; live view = swift-rfb (loomhand/swift-rfb, MIT): event-driven RFB client with a Metal renderer, fed directly by Session.Screen | native idle ~30 MB vs Electron's ~130 MB / 400 MB bundle; framebuffer path stays off a webview canvas | Electron, Tauri (WKWebView startup + canvas path) |
| Mac local helper `cocoonboxd` | Swift, LaunchAgent, dials out to the box | survives the app window; runs "on my Mac" requests; approvals re-checked locally | helper inside the app process |
| In-box agent `cocoonbox-agent` | **Rust**, single static binary (tokio, tonic, rusqlite) | per-VM RSS is density: ~20 MB vs a Node host's hundreds; same language as gw and cua-driver | Node/TS AI-SDK host |
| Control plane + edge `cocoonbox-api` | **Go**, a systemd service on the bench host | sandboxd claim lifecycle behind the `machines` interface; routes app ↔ box | Rust |
| Wire | **gRPC over HTTP/2** everywhere (tonic / grpc-go / grpc-swift 2); bidi streams carry events, RFB bytes and local-exec | one connection per app, streaming both ways, protobuf contracts shared by three languages | HTTP+SSE JSON, WebSocket per channel |
| Screen executor | **trycua cua-driver v0.30.4** (MIT, Rust), `serve` on its 0600 unix socket | AT-SPI element tree + element-token clicks + pixel input, one daemon for Linux/Windows/macOS | cua computer-server (a11y stub), OSWorld server (benchmark-shaped, no auth) |
| Desktop in guest | **real Xorg** with `xserver-xorg-video-dummy` at 1280×800 + `/dev/uinput`, XFCE, AT-SPI on, x11vnc on loopback | cua-driver's focus-free pixel input needs real Xorg + uinput; TigerVNC Xvnc cannot hotplug the MPX devices (`linux-mpx-recovery.md:41-42`) | Xvnc (foreground-only input) |
| Browser | Google Chrome (.deb), CDP on loopback, `--force-renderer-accessibility` | structured channel ahead of pixels | snap chromium |
| Models | **gw v0.2.6** on the bench host, SQLite ledger, bridge-only bind | tenant caps, one endpoint for every model provider | |
| Machines | the cbsbx sandboxd instance on the bench host (§14; K3s + vk-cocoon + CocoonSet until 2026-10-01) | a claim = persistent box with hibernate/wake, warm pools, egress proxy and per-claim secrets | eru (host-local snapshots, no migration) |

Hot-path cost: none. No change to cocoon, sandboxd or gw code; all of it is configuration plus new cocoonbox repos.

## 2. Topology

```
cocoonbox.app ─┐                              bench host
cocoonboxd  ───┼─ gRPC/h2+TLS ─▶ cocoonbox-api (edge + control) ──▶ sandboxd claim ─▶ VM      
               │                  │ routes by box_id, never sees payload secrets        │
               │                  └─ gRPC ─────────────────────────────────────────────▶│ cocoonbox-agent :7443
               │                                                                        │  ├─ cua-driver (unix socket)
               │                                                                        │  ├─ Chrome CDP (loopback)
               │                                                                        │  ├─ x11vnc :5900 (loopback, via agent)
               │                                                                        │  ├─ stdio MCP servers (plugins)
               │                                                                        │  └─ proxy → host egress doors (§14.6, §16.3)
               │                                                                        └──▶ gw 10.203.0.1:18090 ─▶ OpenRouter / vendors
```

The app and `cocoonboxd` always dial out (no inbound port on the Mac). The edge authenticates the user, resolves `box_id → sandboxd claim`, and proxies through the sandboxd port relay the gRPC stream; account credentials never enter the box (§16).

## 3. In-box agent (`cocoonbox-agent`)

Tool tiers, cheapest and most reliable first:

1. **Connectors / MCP tools** from installed plugins.
2. **Shell + files**: persistent PTY sessions, read / write / edit / ls / grep under `/workspace`.
3. **Browser**: CDP; page snapshot as a ref-tagged element list (`- role "name" [ref=eN]`, capped), act by ref.
4. **Computer**: cua-driver `get_window_state` (screenshot + AT-SPI tree) and element-token actions first, pixel `click/type/press_key/hotkey/scroll/drag` last. Only the `computer` subagent holds this tool; the main agent gets a read-only screenshot when its model has vision.

Models (resolved per role in agent config, all via gw):

| Role | Default | Wire |
|---|---|---|
| main | a text model | `/v1/chat/completions` |
| computer subagent | a vision model | `/v1/messages` when the upstream is the model provider's own API, else chat |
| summarize / memory | small cheap model | chat |

Rule: the agent speaks the upstream's native wire; gw translation is not relied on (silent-distortion risk: cache_control, stop reasons, parallel tools). Images from tool results go into a following user image part for vision models and are replaced by text for text-only models.

Privilege split: the loop runs as `cbagent`; tools (shell, files, browser, cua-driver, MCP servers) run as the desktop user `user` in its session. The model's shell cannot read the loop's database or config. The loop holds no real gw key either: gw calls go through the egress proxy, which injects the per-box gw key like any other surrogate, so a key leaked by a tool is worthless and the per-box tenant cap bounds any abuse.

State: SQLite (`/var/lib/cocoonbox/agent.db`) for agents, transcript, approvals, installed plugins; `/workspace` for user files. Both survive hibernate (disk is in the snapshot).

## 4. Interfaces

Proto packages live in one repo (`cocoonbox/proto`), generated for Go, Rust and Swift. Sketch below; field lists are the contract to refine in M2.

### 4.1 Control plane — `cocoonbox.control.v1.Boxes` (edge, per user)

```proto
service Boxes {
  rpc Ensure(EnsureRequest) returns (Box);                 // create or wake; idempotent per (user, flavor)
  rpc Get(GetBoxRequest) returns (Box);
  rpc List(ListBoxesRequest) returns (ListBoxesResponse);
  rpc Watch(WatchBoxesRequest) returns (stream ListBoxesResponse); // the list at once and after every change; never wakes a box
  rpc Hibernate(BoxRef) returns (Operation);                // sandboxd hibernate
  rpc Recreate(RecreateRequest) returns (Operation);        // preserve_data: snapshot /workspace+agent.db, fresh image
  rpc WatchOperation(WatchRequest) returns (stream OperationEvent); // resumable by offset_key
}
message Box { string id = 1; State state = 2; string image = 3; Flavor flavor = 4; bool image_update_available = 5; }
enum State { ABSENT = 0; STARTING = 1; RUNNING = 2; HIBERNATING = 3; HIBERNATED = 4; FAILED = 5; }
message OperationEvent { string operation_id = 1; Phase phase = 2; string detail = 3; string offset_key = 4; int64 at_ms = 5; }
enum Phase { BACKING_UP = 0; CREATING = 1; MOVING = 2; CLEANING_UP = 3; DONE = 4; FAILED_PHASE = 5; }
```

All box RPCs address `box_id` in metadata; the edge forwards `cocoonbox.agent.v1.*` calls to that box.

### 4.2 Connect

Three different things, three permission models (Muse's split):

**(a) App ↔ box — `cocoonbox.agent.v1.Session`**
```proto
service Session {
  rpc Hello(HelloRequest) returns (HelloResponse);          // box_public_key (X25519), agent version, capabilities
  rpc Events(EventsRequest) returns (stream Event);         // channels + from_offset; transcript, agents, approvals, computer-action, box-disk
  rpc Screen(stream ScreenClientFrame) returns (stream ScreenServerFrame); // RFB bytes to/from x11vnc; take-over = client input frames
}
```

**(b) Accounts:** superseded by §16. The in-VM vault sketched here kept credentials at rest in the VM.

**(c) The user's Mac — `cocoonbox.agent.v1.LocalExec`** (`cocoonboxd` dials out)
```proto
service LocalExec {
  rpc Attach(stream LocalFrame) returns (stream LocalRequest);  // hello{computer_id, root, label}; requests: exec, read, ls, upload, download, cancel
}
```
- Permission modes `ask` (default) / `always` / `never`; `ask` prompts per (action, exact target) with `allow-once | deny | always | never`, TTL 10 min, retired at turn end.
- `cocoonboxd` re-checks every request against approvals written by the app (the box cannot forge one); read/ls/transfer confined to a chosen root; shell runs under a Seatbelt profile.
- Local screen control is a later milestone (AX/CGEvent via cua-driver's macOS app, which requires its own signed app for TCC).

### 4.3 Marketplace

Superseded by §17.

### 4.4 Agents — `cocoonbox.agent.v1.Agents`

```proto
service Agents {
  rpc Create(CreateAgentRequest) returns (Agent);           // client_nonce dedupe
  rpc List(ListAgentsRequest) returns (ListAgentsResponse);
  rpc Send(SendRequest) returns (SendAccepted);             // prompt + attachments + selected plugin capabilities; client_nonce
  rpc Transcript(TranscriptRequest) returns (TranscriptPage);
  rpc Respond(RespondRequest) returns (Empty);              // approvals, widgets, "take over done, continue"
  rpc Cancel(AgentRef) returns (Empty);
}
```

## 5. Guest image `cocoonbox-desktop:24.04`

Cocoon OCI VM image (`vmlinuz` + `initrd.img` + rootfs) built from cocoon's `os-image/ubuntu/24.04` template:
- cocoon-agent (`cocoon vm exec`) and systemd-networkd; no NetworkManager.
- Xorg + `xserver-xorg-video-dummy` (1280×800 modeline), `uinput` module loaded at boot, XFCE, `at-spi2-core`, `toolkit-accessibility=true`, screensaver/power management off, x11vnc `-localhost -rfbport 5900`.
- Google Chrome stable with CDP on loopback and a persistent profile.
- cua-driver 0.30.4 as a systemd user unit (upstream has no Linux autostart, issue #3957) with `DISPLAY`, `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`.
- `cocoonbox-agent` and `cocoonbox-toolhost` as system units; users `cbagent` (runs the agent) and `user` (the desktop, owns `/workspace`). No credential vault in the VM (§16).
- root locked, no sshd; debugging via `cocoon vm exec`.

## 6. Bench host layout (isolated env `cb`)

The layout: the cbsbx sandboxd instance under `/srv/cb/sbx` (its own cocoon root, `net none` pools, egress proxy), gw and the MinIO state store on the bridge address `10.203.0.1` (`cbc0`, which holds no VMs), cocoonbox-api behind Caddy, and the guards in `cb-host-net`. The K3s, vk-cocoon, cocoon-operator, registry and `cocoon-cb` stack that ran the boxes until M4.8 was removed on 2026-10-01.

## 7. Milestones

| # | Deliverable | Acceptance evidence | Size |
|---|---|---|---|
| M0 | isolated `cb` env, K3s, registry, vk-cocoon, operator, gw | `kubectl get node cocoon-cb` Ready; `curl 10.203.0.1:18090/health`; a real completion through gw with the new key | 0.5 d |
| M1 | guest image v1 + cua-driver spike | VM from CocoonSet boots; `cua-driver doctor` clean; `get_window_state` on Mousepad and Chrome returns a non-empty AT-SPI tree; an element-token click and a background pixel click both land (visible in x11vnc); hibernate → registry `cb/<vm>:hibernate` → wake keeps a marker file | 1–1.5 d |
| M2 | `cocoonbox-agent` v0 (Rust): gw client (chat + messages), shell/files, browser (CDP refs), computer subagent via cua-driver, Agents/Session gRPC, SQLite | from `grpcurl`: the text-model main agent runs Shell; delegates to the vision computer subagent which opens Mousepad and types text; browser task reads example.com H1 via refs | 4–6 d |
| M3 | `cocoonbox-api` (Go): Boxes on CocoonSet, WatchOperation, edge routing by box_id, user tokens | `Ensure` from zero to RUNNING; `Hibernate`/`Ensure` round trip with a resumable operation stream; agent RPCs reach the box through the edge only | 3 d |
| M4 | macOS app v0: sign-in, box state, agent list, streaming transcript, live view + take-over | RoyalVNCKit spike first (encodings against x11vnc: ZRLE/Tight); app shows the desktop at 1280×800 and the transcript live; measured: app RSS, time to first frame, input RTT | 4–5 d |
| M4.5 | sandbox line evaluation | docs/m45-sandbox-eval.md: measured lifecycle, relay, egress, density, failure model; recommendation | done |
| M4.6 | state layer (§11): user data to object storage, SaveState, Rebase | SaveState twice on a box (second incremental); Rebase v6 → next image keeps /workspace, a logged-in Chrome cookie and agent.db; sizes and timings recorded | in progress |
| M4.7 | native agents: a plugin for the local agent CLIs (skill + commands + MCP config), a `cocoonbox` CLI (login, mcp-headers, box lifecycle), a control-plane MCP in the api (status, ensure, hibernate, save, rebase) | from each CLI, with the app closed: wake the box, do a GUI task, save, hibernate; no token in any config file | next |
| M4.8 | sandboxd as the machine backend (§14), after the upstream PRs (per-claim env, dead-VMM restart) land | §14.9: M3 acceptance plus live view, MCP, Rebase, H3, identity, crash, dead-api and lost-machine runs on the sandboxd backend; default flipped; cocoonset kept as fallback | after PRs |
| M4.9 | proactive agents (§15): routines in the box, api alarms with wake-then-Tick, precheck, notices in the app, limits and kill switches, dual-meter budgets (§15.9, phases a–f) | §15.9 per phase; end to end: the HN digest routine wakes the hibernated box at 08:00, writes the digest, re-hibernates, and its notice shows in the app without a wake; per-run box-seconds and cost match sandboxd's and gw's own records | 8–10 d |
| M5 | connect (§16): host-side broker with OAuth through the Mac, connector tools for both modes, per-tool policy and approvals; M5.2 git through egress injection. `cocoonboxd` local-exec (§4.2(c)) is tracked separately | §16.8: GitHub and Calendar connected; mode B with the box hibernated; mode A write approved in the app; no token in the guest, its hibernate image or the Mac's configs | 5–6 d, M5.2 2 d |
| M6 | market (§17): pinned git catalog in the M4.7 plugin format, skills and secret-free stdio MCP servers in the box, visible in both modes; the catalog doubles as the local agents' marketplace | §17.8: a public agent-CLI plugin installs unchanged and works in a box turn and through the bridge; tampered, symlinked and secret-needing plugins refused; survives Rebase | 3–4 d, M6.2 3 d |

M0–M2 are the proof that "any model + our machines" works; M3–M4.8 make it a usable product; M5–M6 (connect, marketplace) come after the current stack runs end to end.

## 8. Decisions

Settled 2026-09-30:
1. **Repo:** monorepo `loomhand/cocoonbox`, AGPL-3.0. Layout: `proto/`, `agent/` (Rust), `api/` (Go), `macos/` (Swift), `image/`. AGPL allows reusing gw's protocol crates.
2. **Endpoint:** the bench host's public origin. Caddy path matcher `@cocoonbox path /cocoonbox.*` → `reverse_proxy h2c://127.0.0.1:18200` placed before the existing default handlers; gRPC method paths (`/cocoonbox.control.v1.Boxes/Ensure`) never collide with the host's other routes. `caddy validate` before a graceful reload.

Open:
3. **The new gw key(s)** and daily cap.
4. **Rust for the in-box agent** (recommended).

## 9. Risks

| Risk | Mitigation |
|---|---|
| cua-driver is pre-release, ~1 release/day; open Linux bugs (#3774 background `type_text` silent drop, #3871 desktop freeze on Cinnamon) | pin 0.30.4; M1 spike is the gate; fall back to `delivery_mode: foreground` for typing; keep the executor behind our own `computer` tool so the backend can change |
| Real Xorg + dummy + uinput in a cocoon guest unproven | M1 checks `/dev/uinput`, MPX device creation, x11vnc capture of the dummy screen before anything else |
| RFB client cost on the hot path (resolved: RoyalVNCKit polled at 100 Hz; replaced by swift-rfb, idle 0.03% CPU) | spike encodings early in M4; our own Metal layer; H.264 (RFB encoding 50, done before in `cocoonstack/scrcpy-rfb`) later if bandwidth demands |
| The main text model's tool-calling quality under a long tool list | per-role model map; acceptance uses one Shell call first |

## 11. State layer (user data in object storage)

Decided 2026-09-30. The box is disposable across images and nodes; the user's data is not. Hibernate and same-node checkpoints stay the fast path (memory and running programs kept); the state layer is the durable path that survives node loss and image rebase.

- **What:** `/home/user` (Chrome profile and its logins, dotfiles), `/workspace`, `agent.db`, and later the credential vault. Caches are excluded (`~/.cache`, Chrome cache dirs).
- **Format:** content-addressed, restic-style. Files are split into chunks keyed by an HMAC of the plaintext under the user's key, compressed with zstd, encrypted with AES-256-GCM; each snapshot is a manifest listing files → chunks, with a parent pointer. Unchanged chunks are never re-sent.
- **Who does what (boundaries):**
  - api (service layer): owns timing and the store. It decides when to snapshot or restore, holds per-user keys, keeps the chunk index and the manifest chain per user, and presigns every object URL.
  - agent: owns content. `State.Plan` walks and chunks the state (files read by the toolhost as `user`, `agent.db` via SQLite backup by `serve`), `State.Upload` writes only the chunks the api says are missing, `State.Restore` rebuilds the tree. It never holds storage credentials; the key lives only in memory for one request.
  - Storage: any S3-compatible bucket, one prefix per user. On the bench host, MinIO on the host bridge for now.
- **When:** every 15 min while active (incremental), and a quiesced snapshot (browser closed, writers stopped) before hibernate-to-release, before rebase, and on request from the app.
- **Rebase (image upgrade) and node loss:** new box from the current golden → `State.Restore` from the latest manifest → start the desktop. Running programs restart; files, logins and agent history carry over.
- **Not stored:** memory images. Cross-node moves no longer ship 18 GiB checkpoints; the zero-page skip in the CH fork is then a local win only (hibernate time, disk, page-cache pressure).
- **Contract:** `proto/cocoonbox/agent/v1/state.proto`. The edge does not route `State`; only the api calls it, over its internal dialer.
- **Message bounds (v14):** a per-chunk list in a control message is bounded by a page of 2048 entries (about 0.9 MB), never by the state size.
  - `State.Restore` is a bidi stream. The first message carries the key and the manifest URL; each later one carries a page of presigned GETs, presigned just before it is sent. The agent starts writing with the first page and reads the next page whenever no file has all its chunk URLs yet. After the last page it checks that every chunk the manifest references got a URL before it quiesces.
  - URL lifetime: flow control does not keep URLs inside the presign TTL. A snapshot whose chunks are listed in id order (every plan before v15) makes the agent read most pages early, so a restore must finish within `presign_ttl` (10 min) of its first page. Measured: a v11→v14 restore of 97k chunks and 8.9 GiB took 2m16s; the v15 agent restored a v14-saved, id-ordered 90k-file snapshot in 42.2 s (2026-10-02).
  - `State.Upload` keeps its unary wire format, so a pre-v14 agent can still save: the api cuts batches at 2048 puts or 512 MiB, whichever comes first.
  - The agent's State server accepts messages up to 16 MiB, set explicitly. Before v14 it used tonic's 4 MiB default, and a Rebase of a 96,955-chunk box failed with a 45 MB RestoreRequest (2026-10-01).
  - Remaining steps:
    - `PlanResponse.chunks` is still one unary list (about 8 MB at 97k chunks, under the api dialer's 64 MiB limit). Page it once every box runs v14 or later.
    - Fetch a chunk into the cache as soon as its URL arrives when its file is not ready yet, so no URL waits on the write order and the 10 min bound goes away.
- **Save and restore speed (v15, measured 2026-10-02 on the bench host's dev box):**
  - Plan reuses a file's chunks when its size, mtime and ctime all match the stamp recorded at the last upload, without reading it. ctime is set by the kernel on every write, truncate, rename and chmod, so a write that restores size and mtime (npm extracts with a fixed 1985 mtime) is still reread. Quiesced plans reuse too.
  - The stamps live with `known-chunks.json` on the box disk. A plan without a parent, or under another key, ignores them; a new machine has none, so its first plan reads every file. A file whose ctime is within 1 s of the walk is never recorded, and SQLite files read through the online backup are never reused (WAL commits leave the main file's stamp unchanged).
  - Plan reads up to 4 files at a time. `-wal` and `-journal` sidecars go in a second pass, after their database.
  - Plan lists chunks in manifest order, so Restore pages arrive in the order files are written.
  - Restore writes each file into staging as soon as all its chunk URLs are in: 32 files in flight, each with 2 chunks fetched ahead, at most 8 GETs. Only a chunk used more than once goes through a local cache. Staging is built before the quiesce, which now covers only the swap.
  - Memory bound of a restore: 32 × 3 chunks of up to 4 MiB, plus one sealed body per GET. Peak agent RSS on a 90k-file restore: 204 MiB.
  - Chrome restarts in the background after a state operation; a browser tool waits on the browser lock until CDP answers.
  - Effect on dev: a 90k-small-file Rebase went from 94.4 s (v14) to 36.1 s; a zero-change save of that tree from 40.1 s to 1.7 s; the browser stage of a Rebase from 2.2–4.0 s to under 20 ms.
- **Known gaps (accepted 2026-09-30):**
  - Directory mtimes are not restored.
  - The first snapshot spools about the compressed state size to the box disk before Upload.
  - A QUIESCED plan waits a fixed 30 s for a running turn, and that path has no test.
  - A write through a shared mmap to a page that is already dirty does not update ctime until writeback, so a plan can reuse a file whose mapped pages changed after the last read.
  - A restore that fails while filling staging leaves a partial `.cb-restore-*` directory. Both callers, Rebase and Restore, discard the new machine on failure, so it never reaches a serving box.

## 12. Box identity without secrets (replaces `EDGE_TOKEN`, audit H1(a); gw relay, H3)

Decided 2026-09-30, edge side reviewed 2026-10-01. Constraints:
- No secret shared across boxes.
- Nothing secret at rest in the guest. Disk, hibernate memory images and state uploads all count as at rest.
- No host service identifies a box by its source IP or MAC (N1: IPs are reused after clone and rebase).
- The agent learns nothing about how it is scheduled.

The adversary is the box's own user (`user`, unprivileged), or root inside one box after a guest compromise.

### 12.1 Edge → agent

| | A. Edge-signed box-bound credential | B. Per-box token in tmpfs | C. vsock transport |
|---|---|---|---|
| Mechanism | The edge signs `key_id · box_id · wake_id · exp` with ed25519 (header `x-cocoonbox-credential`). The agent verifies it with the public key from its identity file, and checks `key_id`, `box_id` and `wake_id` against that file and `exp` against its clock. | The api delivers a random per-box token at every boot and wake into `/run/cocoonbox/edge-token` (tmpfs, root:cbagent 0640). The agent compares it like today. | The agent accepts calls only on vsock from CID 2 (the host). A node-local bridge carries edge traffic onto each VM's vsock socket. |
| Secret in the guest | none | yes, in RAM, so it lands in every hibernate image until the next wake rotates it | none |
| Misrouted call (stale pod IP now held by another box) | rejected: `box_id` mismatch | rejected: token mismatch | impossible: the channel is the VM |
| Guest root compromise | can use only its own box's credentials; forging needs the edge's private key | leaks its own token until the next wake | none reusable |
| Host plumbing | none new; api writes one file per wake | same file write, plus per-box token storage in the api | per-node TCP↔vsock bridge with its own auth to the edge |
| Clock dependence | yes (`exp`); bounded by `wake_id` (below) | no | no |

**Decided: A** (ruling 2026-09-30).
- It meets all four constraints with no new host daemon. The only secret is the edge's private key, which stays in the api: a host file `/etc/cocoonbox/edge-signing.key` (root 0600) read through `LoadCredential`, since the api is a host unit. A K8s Secret applies only if the api moves into the cluster.
- It is independent of the transport, so it stays as defense in depth after the move to sandboxd (M4.8), where C comes for free.
- B is rejected: its token sits in hibernate images, which breaks the at-rest rule.

**Credential format.** `x-cocoonbox-credential: v1.<b64url(key_id|box_id|wake_id|exp_unix)>.<b64url(ed25519 signature over the first part)>`: base64url without padding, `|`-separated fields.
- `key_id` is the first 16 hex of sha256(public key).
- The edge caches one credential per (box, `wake_id`) and re-signs when less than 2 min of `exp` remain, so the hot path is a map lookup; a stream signs once at open.
- The edge strips any client-sent `x-cocoonbox-credential` on gRPC and MCP, as it does `x-cocoonbox-edge-token`. During the switch it sent both headers until no image set `EDGE_TOKEN`; the agent has since dropped the token.

**Where the agent learns its identity and the edge key.** On every boot and wake, the api writes `/run/cocoonbox-identity/identity`, `{"box_id":…,"wake_id":<random>,"key_id":…,"public_key":<ed25519>}`, through sandboxd exec (silkd over vsock, running as root in the guest).
- The directory is root 0755 and the file root 0644, both tmpfs. It is not `/run/cocoonbox`: that directory is `user:cbagent 2770` for the toolhost socket, so `user` could replace a file in it and point the agent at its own key.
- The file is not secret; integrity is what matters, and only guest root can write it.
- The agent accepts a credential only for the `key_id` in that file. It re-reads the file when a credential carries a `key_id` or `wake_id` other than its cached copy, since a wake rewrites the file under a running agent.
- The agent never derives its identity from the hostname, pod name or IP, which would mean knowing how it is scheduled.
- Until the file exists, every call, `Session.Hello` included, gets `UNAVAILABLE` "identity not provisioned". Readiness is the TCP probe on 7443, and the api reports `RUNNING` only after the identity write succeeds, so no client call arrives earlier.
- The api keeps `wake_id` in the machine's meta (a claim env entry, not secret), so it survives an api restart; a wake is a new VM incarnation.

**Clock.** A woken guest's clock may lag until NTP syncs. `wake_id` changes on every wake, so it carries the real freshness: a credential from before a wake is dead whatever the clock says. `exp` is a coarse bound (10 min, with a small skew allowance of 2 min) against replay within one wake. Replay needs a copy of the edge's traffic, which runs host-local through the sandboxd port relay.

**Rotation.** The public key arrives in the identity file, not in the image. Rotation is therefore: the api starts signing with the new key, then rewrites the identity files on the next wake. No image cut is needed.

**Rebase and hibernate.** A rebased machine gets its own `wake_id`, so the old machine's credentials die with it. A hibernate image carries the identity file; that is harmless because it is not secret, and the wake rewrites it with a new `wake_id`.

**MCP** uses the same header, so the edge treats `/mcp` exactly like gRPC.

### 12.2 Box → host (gw relay, H3)

A signature can't prove "request from box X" without a secret in X. The proof must come from the channel.
- **Decided: H3 lands with M4.8 (sandboxd).** sandboxd already provides option C for this direction: the none lane, a per-VM `<vsock>_2049` egress door, and per-claim `guest: false` secrets (#268).
  - The gw key becomes a per-claim host-only secret, injected on the host side of that door. The guest never holds it, and no IP identifies the box.
  - No vsock relay is built on the K8s path.
- **Interim on the K8s path (until M4.8):** per-box gw keys with per-box caps, set in the gw config.
  - The key in `agent.env` then belongs only to its own box and its own user, and its cap bounds any abuse.
  - It is still at rest in that one box, which is the accepted gap until M4.8.

### 12.3 Changes

- **Order:** after the v7 Rebase acceptance. Then split api / agent / infra as usual.
- **contracts.md:** drop `EDGE_TOKEN` and add `/run/cocoonbox-identity/identity` and the credential header. `GW_API_KEY` stays per box until M4.8.
- **agent:** verify ed25519 (`ed25519-dalek`, about 40 KB) with the key from the identity file, and read that file.
- **api:** sign with a per-(box, `wake_id`) cache; write the identity file through the backend's `Deliver` before reporting `RUNNING`; keep `wake_id` in the machine's meta.
- **infra:** tmpfiles entry `d /run/cocoonbox-identity 0755 root root`; remove `EDGE_TOKEN` from `agent.env` and the golden; per-box gw keys and caps.

## 13. Native agents without the app (M4.7)

Proposed 2026-10-01, for review. The user's own local coding agent drives their box with the app closed: it wakes the box, works on it through the box tools, saves and hibernates. The model subscription stays on the Mac (mode B); the box sees only MCP calls.

Constraints:
- Boundaries: the new pieces talk to the rest only through `proto/` gRPC and MCP via the edge. Nothing imports another component's code or reads its files.
- Credentials: the user token lives in the Keychain and nowhere else. It is never in a client's config file, in argv, or in a long-lived environment variable.
- Tool names and the per-box MCP endpoint are the contract in contracts.md "Remote tools over MCP"; nothing here changes them.

### 13.1 Pieces

| Piece | Where | What |
|---|---|---|
| `cocoonbox` CLI | `macos/` (Swift, a new product next to the app, on CocoonBoxKit) | login and logout (Keychain), box lifecycle commands, the stdio MCP bridge, a `headersHelper` command, per-client setup |
| Control MCP | api service layer, `https://<endpoint>/cocoonbox.mcp/control` | box lifecycle tools for the caller's own box, over the existing `Boxes` semantics |
| Plugin | `plugins/cocoonbox/` (no code) | one directory with a manifest per client, a shared `.mcp.json` that runs `cocoonbox mcp`, a skill and slash commands |

The CLI is Swift on CocoonBoxKit because it needs the Keychain and the app already solved endpoint parsing, the loopback-only plaintext rule, the HTTP/2 connection and the `Boxes` calls. That is vertical reuse inside one component (`macos/`), not coupling to another. It ships inside the app bundle (`CocoonBox.app/Contents/MacOS/cocoonbox`), and `cocoonbox install` links it into `~/.local/bin`; a standalone build works the same way for users who never open the app.

### 13.2 How a client reaches the box: a local stdio bridge

Every client runs the same MCP server entry, with no URL and no token in it:

```json
{ "mcpServers": { "cocoonbox": { "command": "cocoonbox", "args": ["mcp"] } } }
```

`cocoonbox mcp` is a stdio MCP server. It reads the token from the Keychain at start and keeps it only in memory. It forwards each JSON-RPC message to the edge over MCP Streamable HTTP with `Authorization: Bearer`: box tools go to `/cocoonbox.mcp/<box id>`, and lifecycle tools go to the control MCP. It resolves the box id itself (`Boxes.List`), so a Rebase or a new box never breaks a client's config.

Why a bridge rather than pointing clients at the HTTPS endpoint directly (what the app's Connect sheet does today):
- **One recipe for all three clients.** The three agent CLIs all run stdio servers the same way. The direct recipes differ per client: `headersHelper` or `${VAR}` for `claude`, `bearer_token_env_var` for `codex`, `${env:VAR}` for `cursor-agent`. The last two need the token in an environment variable of the shell that starts the client.
- **A hibernated box.** A client connects to its MCP servers at startup. A hibernated box has no `/mcp` to answer, so a direct client marks the server failed and never lists the box tools. The bridge answers `initialize` and `tools/list` itself (below) and wakes the box on the first box-tool call.
- **Stable identity.** The bridge passes the client's `initialize` through unchanged, so the box still records the session as `mcp:claude-code` (or `codex-mcp-client`, or the third CLI's own name) and the app shows it under External.

The cost is one local stdio hop per message, next to a 230 ms WAN round trip; the bridge holds one HTTP/2 connection for both endpoints.

**Tool list while the box is down.** The box tool names and input schemas are the contract, so the bridge can answer `tools/list` without the box. It caches the box's own `tools/list` answer (not secret) in `~/Library/Caches/cocoonbox/tools.json` whenever the box is running, falls back to a copy compiled into the CLI, and sends `notifications/tools/list_changed` when the live list differs. Lifecycle tools come from the control MCP, or from a compiled-in copy when the edge is unreachable.

**Connector tools (§16).** At `initialize` the bridge also opens a session to `/cocoonbox.mcp/connect` with the client's own `initialize`, and holds that door's GET stream. Its `tools/list` answer appends the door's tools, after waiting at most 3 s for the first list. A connector tool whose name a control or box tool already has is left out, with a line on stderr. On the door's `list_changed` the bridge lists again, and sends `notifications/tools/list_changed` when the connector tools differ from those the client last listed. A call to a listed connector tool goes to the door and wakes no box. When the door fails, the bridge serves the other tools and retries with backoff, from 1 s up to 60 s. A 401, 403, 404 or 405 from an api without the door ends the retries.

**Waking on demand.** When a box tool is called and the box is not RUNNING, the bridge calls the control tool `box_ensure`. It reports the wake as MCP progress notifications ("waking: restoring memory snapshot"), then forwards the call. A wake takes 5–17 s today. A client whose tool timeout is shorter than that, or a user who does not want implicit wakes, uses `cocoonbox mcp --no-wake`: the call then fails with an error that tells the model to call `box_ensure`.

**Direct fallback.** The Connect sheet recipes stay for users who don't install the CLI. The `claude` recipe may use `"headersHelper": "cocoonbox mcp-headers"`, which prints `{"Authorization":"Bearer …"}` to the CLI's pipe, instead of `${COCOONBOX_TOKEN}`. There is no `cocoonbox env` command: an exported token lives in every child of that shell.

### 13.3 Control MCP (api)

The api serves it itself; the edge does not proxy it to a box. Auth is the user token, which the api checks the way it checks `Boxes`, and every tool acts only on the caller's own box. Tools (names are the contract; add them to contracts.md):

| Tool | Arguments | Result |
|---|---|---|
| `box_status` | – | state, image, latest_image, last_saved_at_ms, box id |
| `box_ensure` | `wait` (default true) | final state; progress notifications from `WatchOperation` while waiting |
| `box_hibernate` | – | operation result; progress notifications |
| `box_save` | – | operation result with bytes; says the browser closes during the save |
| `box_upgrade` | `confirm: true` (required) | operation result; refused without `confirm`, with the text "running programs restart; files, logins and agent history are kept" |

These are thin wrappers over `Ensure`, `Hibernate`, `SaveState`, `Rebase` and `WatchOperation`, so the api keeps one implementation of the lifecycle. The CLI's own `cocoonbox status|ensure|hibernate|save|upgrade` commands call `Boxes` over gRPC directly, as the app does.

### 13.4 CLI

| Command | Does |
|---|---|
| `cocoonbox login [--endpoint URL] [--browser \| --token - \| --from-app] [--invite CODE]` | on a terminal (or with `--browser`), signs in through the browser and gets a device token of its own (contracts "Browser sign-in and device tokens"); with `--token -`, over SSH, or with stdin not a terminal, reads a token from stdin or a hidden prompt. Stores it in the Keychain and checks it with `Boxes.List` |
| `cocoonbox invite create [--days N]` / `invite list` | with an admin's invite-scoped token from `COCOONBOX_INVITE_TOKEN` or a file, makes a one-use invite link or lists the token's invites (contracts "Invite-scoped tokens") |
| `cocoonbox logout` | deletes the Keychain item, also when the api is unreachable; first revokes the token at `/cocoonbox.oauth/revoke` when the CLI got it through the browser. A token copied from the app (`--from-app` or the implicit copy) is the app's device and is not revoked. `cli.json` records where each endpoint's token came from |
| `cocoonbox status` / `ensure` / `hibernate` / `save` / `upgrade [--yes]` | lifecycle, with progress on stderr and `--json` for scripts |
| `cocoonbox mcp [--no-wake]` | the stdio bridge (13.2) |
| `cocoonbox mcp-headers` | the headers helper for the direct `claude` recipe |
| `cocoonbox rsh [--box B] [HOST] COMMAND…` | runs a command in the box with its stdin, stdout and stderr; rsync's and git's remote shell (13.7) |
| `cocoonbox setup claude\|codex\|cursor [--print]` | installs the plugin, or prints the manual config for that client |
| `cocoonbox install` | links the CLI into `~/.local/bin` |

**Keychain sharing with the app.** Both use service `io.github.loomhand.cocoonbox` with the endpoint URL as the account. With a Developer ID signature, app and CLI share one keychain access group, so neither prompts. Until then, builds are ad hoc signed and every rebuild has a new code hash. So `cocoonbox login` writes its own item, and `cocoonbox login --from-app` copies the app's token once, after a single "Always Allow".

### 13.5 Plugin packaging

`plugins/cocoonbox/`:
- `.claude-plugin/plugin.json`, `.codex-plugin/plugin.json` and a third manifest for `cursor-agent`, which also reads `.claude-plugin/plugin.json`; its own manifest carries its interface fields. Each points at the shared `.mcp.json` above and at `skills/`.
- `skills/cocoonbox/SKILL.md`: the computer is the user's cocoonbox cloud desktop, not this Mac. Check `box_status` first. Use `screenshot` to confirm GUI actions. Call `box_save` before long idle.
- `claude` commands `/cocoonbox:status`, `/cocoonbox:wake`, `/cocoonbox:save` and `/cocoonbox:sleep`, which tell the model to call the matching control tool.

None of these files contains a URL or a token: the endpoint comes from `cocoonbox login` and the box from `Boxes.List`. Installation, per the installed CLIs' own help (`claude` 2.1.285, `codex` 0.159.2, `cursor-agent` 2026.09.28):
- **`claude`:** `claude plugin marketplace add <path>` takes a local path, then `claude plugin install cocoonbox@<marketplace>`. `cocoonbox setup claude` runs both against the plugin directory it writes, then `claude plugin update cocoonbox@cocoonbox`: install leaves an installed plugin at its recorded version. A second run changes nothing and succeeds (`claude` 2.1.288).
- **`codex`:** `codex plugin marketplace add` takes a local or Git marketplace, then `codex plugin add cocoonbox@<marketplace>`. `cocoonbox setup codex` runs both the same way.
- **`cursor-agent`:** `agent plugin marketplace add` takes only a git repository URL, and the CLI has no plugin install command. Until the repository is public, `cocoonbox setup cursor` falls back to the MCP config: it merges the stdio entry above into `~/.cursor/mcp.json`, with no token or URL in it. `agent --plugin-dir <dir>` loads the full plugin (skill included) for one run.

CI checks the directory with `claude plugin validate`.

### 13.6 Acceptance and work split

Acceptance, from each of the three agent CLIs with the app closed and the box hibernated:
1. Ask for a GUI task. The bridge wakes the box and the task completes; the app, when opened later, shows the session under External with the client's name.
2. Ask it to save, then to hibernate; both finish and `cocoonbox status` agrees.
3. Search the three clients' config and state directories and the shell history for the token: no match. The token is also absent from argv and the environment of every client and bridge process (`ps -E`).

Split:
- **macos:** the CLI, the bridge and the plugin directory.
- **api:** the control MCP (contracts.md "Control MCP"), answered by the edge at `/cocoonbox.mcp/control`.
- **agent:** no change.

Order: the api contract first (contracts.md on main), then api and macos in parallel.

Decided 2026-10-01:
1. The bridge wakes the box on the first box-tool call by default; `--no-wake` turns that off.
2. The CLI ships inside the app bundle and as a standalone build.
3. No `box_operations` tool.

### 13.7 Remote shell: `cocoonbox rsh`

Decided 2026-10-03: offload transport is rsync/git over `cocoonbox rsh`; sshd deferred. The box has no sshd since v12, and a per-box sshd would need host keys, user keys and a port path through sandboxd. `rsh` reuses what every client already has: the user token, the HTTPS edge and the agent's gRPC.

- **Wire:** `Session.Exec`, a bidi stream (contracts.md "Remote command"). The first frame names the command and, for `-t`, a terminal. Then come stdin chunks, an explicit `eof`, signals and terminal sizes. `eof` is a frame, not only the half-close, so a Ctrl-C still reaches a command whose stdin ended. stdout and stderr chunks come back, and the exit code is the last frame.
- **Edge:** nothing new. Exec is one more `cocoonbox.agent.v1` method through the frame-by-frame proxy, authenticated like every gRPC call: user tokens only, with the owner's v1 credential.
- **Agent:** the Session service checks the principal (owner only) and asks the toolhost to run the command as `user`. The toolhost spawns a login `bash -lc` in `/workspace` with the shell tool's environment, so `~/.local/bin` and tool shims are on `PATH`. It runs in its own process group on pipes, or for `-t` as a session leader on a new pseudo-terminal. The connection carries tagged frames both ways: stdin, eof, signal and resize in; stdout, stderr and exit out. Each side moves one chunk at a time, so backpressure reaches the command. Closing the connection kills the group. The toolhost also notices the close while a write to the command's stdin is blocked: it polls the socket's read-closed state every 200 ms, only while that write waits.
- **CLI:** `cocoonbox rsh [--box B] [-t] [-l USER] [HOST] COMMAND…`, ssh-style. It reads the token and endpoint as `mcp` does.
  - It wakes a box that is not running rather than failing, since rsync gives no second chance.
  - It exits with the remote code, or 255 for its own failures.
  - Host word: rsync and git pass one first. `--box` wins and drops a first word that is `box` or names one of the user's boxes. Without `--box`, the word names the box, as `docker exec` and `kubectl exec` do as rsync shells.
  - `-t` puts the local terminal in raw mode and forwards window sizes.
  - The first SIGINT or SIGTERM goes to the remote process group, and a second ends the CLI, which kills the command.
  - git takes it as `GIT_SSH_COMMAND="cocoonbox rsh" GIT_SSH_VARIANT=simple`.
  - The CLI waits on non-blocking descriptors, because macOS rsync (openrsync) hands its remote shell a non-blocking stdin.
- **Not in the exec path:** quiesce and the in-flight count. Scheduled saves are live, and a quiesced save or Rebase must not wait on a long rsync. A Rebase during an rsync loses what was written after its save; the next rsync sends it again.
- **One connection per box:** the edge sends every call to a box over one HTTP/2 connection, and h2 frees connection credit only as a stream's data is read. With hyper's default windows, 1 MiB for the stream and 1 MiB for the connection, one Exec whose command ignored a full stdin stalled every other call to that box, until the command ended (seen on dev). The agent now grants a 64 MiB connection window over 1 MiB stream windows, so a stalled stream holds only its own window.
- **Terminal types:** a Mac terminal's TERM (`xterm-kitty`, `xterm-ghostty`) is often unknown to the box, whose image has only `ncurses-base`, and curses programs then refuse to start. As kitty's ssh kitten does, `-t` sends the local `infocmp -x "$TERM"` output with the tty, and the toolhost compiles it into `~/.terminfo` as `user` when the box lacks the entry; `~/.terminfo` lives in the home, so the state store keeps it across Rebases. Without a usable source the command gets `xterm-256color` and a one-line notice. tic reads client text, so it runs as `user`, never root, with a 256 KiB cap and a 5 s timeout.
- **Image:** v20 ships rsync, git and tmux; v19 has none of them. v21 adds `build-essential` and lists `ncurses-bin` (tic, infocmp) explicitly.

Hot path: none on existing calls. The edge adds no code. The agent adds one owner check per Exec stream and nothing to the other methods. The window change only grants more credit. A `-t` stream also runs one `infocmp` in the box (and `tic` once per new terminal type) and one local `infocmp` in the CLI.

**Acceptance (2026-10-03, dev's box on v19 with test agent 4228d59, from a Mac over the public endpoint, RTT 250 ms):**

| Check | Result |
|---|---|
| `rsh --box main uname -a`; `false`; `exit 42` | Linux …; exit 1; exit 42; runs as `user` in `/workspace`, stderr kept apart |
| 50 MiB random through `cat` | sha256 equal both ways; 15.2 s round trip |
| Throughput, 50 MiB | upload 3.3 MiB/s, download 8.0 MiB/s with the call's startup included. `ssh` to the bench host on the same path moved 5.9 and 6.5 MiB/s |
| Startup of `rsh true` | 1.22–1.26 s (10 runs): TCP and TLS to the endpoint, `Boxes.List`, the Exec stream; the TLS handshake alone took 0.50 s |
| rsync of the cocoonbox checkout, `.git` included (1025 entries, 775 files, 8.0 MB) | first run 4.8 s, 4.6 MB sent; no change 2.8 s, 64 KB sent; one file changed (`docs/design.md`) 3.0 s, 65 KB sent and 260 B of unmatched data; trees equal by sha256 |
| `GIT_SSH_COMMAND="cocoonbox rsh --box main" git clone box:/workspace/rsh-test/main` | 3.1 s; git probed `-G`, got 255 and used its simple variant; a push back worked |
| Ctrl-C of a running `sleep`, idle and with a full stdin | the remote process ended both times; the CLI exited 130 |
| A second call while an Exec held a full stdin | 1.27 s with 4228d59; it waited until the stalled command ended with 7e14603 |

Round 2 (agent de97cf9, the revised plan, same setup):

| Check | Result |
|---|---|
| Login shell | `login_shell on`; with `~/.local/bin` present it leads `PATH` (`/home/user/.local/bin:/usr/local/sbin:…`) |
| `-t` from a pty of 33×111 | `/dev/pts/3`, `TERM=xterm-256color`, `33 111`; after a local resize and SIGWINCH `44 132`; typed input echoed and read; exit 5 passed through. tmux runs in it |
| SIGINT to the CLI, remote `trap … INT` | `got-int`, exit 9, 0.28 s after the signal; SIGTERM to a remote `sleep` gives 143 and the process is gone |
| `GIT_SSH_VARIANT=simple` | no `-G` probe; clone 3.2 s; a commit made in the box arrived by `git fetch` in 1.9 s |
| A second call while an Exec held a full stdin | 1.29 s |
| 50 MiB through `cat`, upload, download; startup | equal; 15.2, 14.8 and 6.7 s; 1.22–1.27 s |

macOS rsync is openrsync (protocol 29); the box runs rsync 3.2.7. Pushing `small/` both ways through `cocoonbox rsh`:
- **Push works with:** `-a -z --delete --exclude --exclude-from --include -v -n -c --partial -P --progress --inplace -u --ignore-existing --existing --size-only --stats --timeout -H --numeric-ids --compress-level -b --suffix --backup-dir --rsync-path --no-perms --no-owner --no-group`.
- **Pull works with:** `-a -z --delete --exclude -c --stats -n -P --inplace -u --size-only`.
- **openrsync refuses** `--info=progress2`, `--mkpath`, `--checksum-choice`, `-X` and `-A`.
- **`--delete-excluded` hangs.** It hangs the same way over plain ssh to the bench host's rsync 3.2.7, so the transport is not the cause.
- **No missing parents.** rsync 3.2.7 creates only the last directory of the destination, and openrsync has no `--mkpath`, so run `cocoonbox rsh B mkdir -p DIR` first.
- **The way back.** A pull with `-b --backup-dir` (openrsync as the receiver) backs up local files the box changed, but `--delete` removes local-only files without a backup. The way back must not rely on `-b` for files that exist only on the Mac.

Upload stays under ssh's rate. One 1 MiB window in flight over a 250 ms RTT allows about 4 MiB/s, and Go's HTTP/2 server, which Caddy uses, buffers 1 MiB per stream by default. That fits the numbers but is not proven.

## 14. M4.8: sandboxd as the machine backend

Design, 2026-10-01. It builds on the M4.5 measurements (`m45-sandbox-eval.md`) and on three upstream PRs: cocoonstack/sandbox#268 (per-claim env), cocoonstack/sandbox#269 (dead-VMM restart) and cocoonstack/cocoon#274 (`vm list` verifies the VMM PID). Only `api/` and `image/` change. The agent and the app do not, except for one optional proto field (§14.8).

### 14.1 Box ↔ claim, and where it persists

- One machine = one sandboxd claim. sandboxd is multi-tenant, so production runs one sandboxd per node for every tenant, cocoonbox included (decided 2026-10-01).
- The backend stamps `metadata {cb_machine: <machine id>, cb_owner: <owner>}` on the claim. Claim metadata is immutable, and nothing else needs to be mutable.
- sandboxd's `claims.json` is the only record. There is no api-side database:

| `machines.Machine` | from the sandboxd row |
|---|---|
| `ID`, `Owner` | `metadata.cb_machine`, `metadata.cb_owner` |
| `Image` | `key.template` |
| `Flavor` | `key.size` through the tier table (4 vCPU / 8 GiB = `xlarge`); a flavor that is not a tier is refused at config load |
| `CreatedAt` | `claimed_at` |
| `State` | see 14.3 |

- `Get` / `List` = `GET /v1/sandboxes?metadata=cb_machine=…` / `?metadata=cb_owner=…`.
- The backend keeps an in-memory index of every claim. One `GET /v1/sandboxes` every 2 s refreshes it (the instance holds only cocoonbox claims), and each of the backend's own lifecycle calls updates it. So `Resolve` on the edge path stays a map lookup, as the CocoonSet informer cache is today.
- The row is node-local: a lost node takes its rows with it. The durable per-user record is the state layer (§11), not the claim (14.6).

### 14.2 Image → template and pool

- Each image release ships as a flat rootfs for `cocoon image import` into the sandboxd node, `cbdesk:vN`. `image/cut.sh` builds it from a VM booted from the previous template (docs/bench.md, New image cut).
- Additions to the image:
  - silkd and its unit;
  - `http(s)_proxy=127.0.0.1:3128` drop-ins on the agent, toolhost and session units;
  - `systemd-networkd-wait-online` masked;
  - a static `agent.env` whose gw key is a placeholder (14.5).
- Kernel: the image's own Ubuntu kernel, until `CONFIG_INPUT_UINPUT` is in the sandbox kernel.
- The default image is a pool `{template: cbdesk:vN, net: none, size: xlarge, warm: K, warmup: [wait for Xorg, session, toolhost, :5900, :7443]}`. A warm clone is then a finished desktop with the agent up.
- Each `rebase_images` entry is a pool with `warm: 0`. The key is registered and the golden is built, so a rebase claim clones from the golden instead of cold-booting, and nothing idles warm.
- `Spec.Image` names the template. Boxes on an older image keep their claim; the pool only matters for new claims.
- Disk gap: sandboxd cold-boots goldens with cocoon's default disk (10 GiB), while our flavor says 40 GiB. This needs an upstream pool field for the golden's disk size (14.7). Until then, the acceptance run uses 10 GiB and records it.

### 14.3 Lifecycle mapping

| `Machines` | sandboxd | Latency (M4.5, nested) |
|---|---|---|
| `Ensure`, no claim | `POST /v1/claim {template, size, ttl_seconds: 3600, on_expire: archive, metadata, env}` | warm 1 ms + env delivery |
| `Ensure`, hibernated or archived | `POST /v1/sandboxes/{id}/wake`; the new incarnation is then provisioned (14.5) | 0.26 s / 1.7 s + about 0.3 s to ready |
| `Ensure`, failed | `POST …/wake` (#269: a cold boot of the claim's disk) | boot time |
| `Hibernate` | `POST …/hibernate` | 23–50 s (writes the 8 GiB memory file) |
| `Delete` | `POST …/release` | — |
| `Watch` | poll the row every 500 ms while the watch is open; the index refresh between watches | — |

- `Ensure` and `Hibernate` return at once, as the interface requires. The sandboxd call runs in a backend goroutine, and the backend reports the in-flight state meanwhile.
- `Revision` is a per-machine counter in the backend. It increases on each lifecycle call and resets with the api, the same way `operations` do.

States:

| Condition | `State` |
|---|---|
| a claim or wake call in flight; restarts moved and the agent has not answered on 7443 since | `Starting` |
| a hibernate call in flight | `Hibernating` |
| `hibernated` or `archived` | `Hibernated` |
| `failed` | `Failed`, with the row's `failed` as `Reason` |
| otherwise | `Running` |
| row gone | `ErrNotFound`; `StateGone` on a watch that saw it |

Leases and tiers:
- The backend renews every claim it owns every 5 min with `{ttl_seconds: 3600, on_expire: archive}`, skipping archived ones, which sandboxd refuses to renew. A dead api therefore degrades to "archived at lease end", never "destroyed".
- sandboxd's `idle_hibernate_seconds` stays 0. When to hibernate is the service's decision, as it is today.
- `archive_after_seconds` stays off at first. The archive tier only frees local disk once `checkpoint_store: s3` exists on the node.
- sandboxd checkpoints are not used. Memory images do not leave the node (§11).

### 14.4 Edge → agent: `Address` returns a dialer

The agent is reachable only through `GET /v1/sandboxes/{id}/ports/7443` with `Upgrade: tcp`. There is no IP. The edge serves `Session.Screen` itself through `GET /v1/sandboxes/{id}/ports/5900`, the box's RFB server; no other port is relayed.

The interface change, backend-neutral:

```go
// Endpoint is how to reach a machine's agent; Key changes when the machine moves.
type Endpoint struct {
	Key  string
	Dial func(ctx context.Context) (net.Conn, error)
}
Address(ctx context.Context, id string) (Endpoint, error)
```

- sandboxd: `Key` = `sandboxd:<sandbox id>`. `Dial` opens the relay with the api's root token, which gives the **passive** relay.
  - It never wakes a hibernated box and answers 409 instead (`Unavailable`, the same as `NotRunningError` today).
  - It never stamps idle activity, so an open edge connection cannot keep a box awake. The M4.5 finding stops mattering.
  - A failed claim also answers 409.
- The edge proxy, the MCP proxy and `dialer` all use it:
  - gRPC: `grpc.NewClient("passthrough:///"+key, grpc.WithContextDialer(…))`, with the connection cache keyed by (box, `Key`).
  - MCP: one h2c `http.Transport` per `Key` with `DialContext` = `Dial`.
  - The boundary holds: the edge still depends on a resolver, never on a backend.
- Auth: the api holds the sandboxd root token in a host file read through `LoadCredential` (root 0600). The edge authorizes user → box before it dials, exactly as today. No sandbox token is stored or requested.
- Identity delivery sits behind the `machines` interface (`Deliver`, `SetMeta`, `Incarnation`; 14.5), so the api service layer and the agent do not depend on the backend.
- §12's signed credential stays as defense in depth. The relay itself is option C (the channel is the VM), so a misrouted call is impossible.

Hot-path cost against today's direct h2c to the pod IP:
- One relay upgrade per agent connection: 6–9 ms. The connection is pooled per box with a 1 min idle timeout, so this is not per call.
- +0.1 ms per message RTT (0.14 vs 0.04 ms).
- A per-stream ceiling of about 293 MiB/s.
- sandboxd CPU for the base64 framing: about 26% of a core at a full-rate Raw stream (156–171 MiB/s). At the Tight/ZRLE rates the app uses, that is about 1% or less.
- `Resolve` stays a map lookup (14.1).
- Binary relay framing is upstream work only if many full-rate viewers share a node (14.7).

### 14.5 Provisioning on sandboxd: `Deliver`, gw key, identity, meta

The service layer computes each incarnation's identity and hands it to the backend with `Deliver(ctx, id, machines.Identity, keys)`, where `keys` are the gw key and the door key as named host-only secrets. It never gets a root exec into the guest and never knows guest paths or scripts. The sandboxd backend implements the same interface:

| `machines` | sandboxd |
|---|---|
| `Deliver`: gw key | a `guest: false` env entry `GW_KEY` (below) |
| `Deliver`: identity | written by the backend to the §12 path, either through `POST /v1/sandboxes/{id}/exec` (runs as root in the guest through silkd; exec takes only the claim's own token, which the backend reads with the root by-id `GET /v1/sandboxes/{id}` at each call and never stores; the buffered exec has no stdin, so the file travels in the request `env` as `CB_STDIN`) or as guest env. How it is written is the backend's choice |
| `Meta` / `SetMeta` | `guest: true` env entries `CB_META_<key>` (#268): readable with `GET …/env` and durable in `claims.json`. Claim metadata is immutable, and the values (wake id, flags) are not secret |
| `Incarnation` | a backend concern. It changes on claim, wake, unarchive and cold restart, and needs a per-claim boot counter in the sandbox row (upstream, 14.7). Until then, the backend derives it from `claimed_at`, `restarts` and its own wake count, kept in meta |

- **H3 closes** inside the sandboxd `Deliver`.
  - The key becomes the entry `GW_KEY = Bearer <per-machine key>`. It feeds the node secret `{name: gw, header: Authorization, value_env: GW_KEY}` on the pool rule `{host 10.203.0.1, ports [18090], secret gw}`, with `egress_internal_allow: [10.203.0.1/32:18090,9000]`.
  - The agent sends the placeholder from the static `agent.env`, and sandboxd swaps in the machine's key on the host side of the vsock door.
  - The real key is never in the guest, on its disk or in any hibernate image, because `guest: false` values never enter the guest (#268).
  - The node environment carries no `GW_KEY`, so a claim without one fails closed (gw answers 401). A key can be set or rotated even while the box is paused.
- **Identity.** A cold restart (#269) loses the tmpfs file but is a new incarnation, so the service re-delivers it exactly as it does after a wake. Until then, the agent answers `UNAVAILABLE` "identity not provisioned" (§12). sandboxd itself re-delivers the claim's guest env after a cold restart (#269).

### 14.6 Network lane, crash story, node loss

- **Lane: none, with the host proxy.** Chrome reaches the internet through `127.0.0.1:3128`, which M4.5 measured with pages, updates and safe browsing. The egress lane is ruled out for a persistent box:
  - it cannot hibernate, checkpoint or cold-restart (#269);
  - its NIC would reopen the host-IP identity problem (N1).
- Consequences of the none lane:
  - No UDP. Chrome falls back from QUIC to TCP, and WebRTC media is degraded.
  - Raw TCP (git over ssh) needs the pool policy's `socks5: true` door.
  - Policy: the gw, Connect and apt cache rules, then `*` on ports 22, 80 and 443 only, so mail ports and every other port are closed. The SSRF guard keeps other cb VMs and host ports unreachable, which M4.5 measured as 502.
- **VMM crash (#269).**
  - The claim is cold-booted from its own disk within the 10 s liveness pass plus the boot, under the same id and token.
  - Lost: memory, the running turn, Chrome's open tabs and the tmpfs files, although env and identity are re-delivered. Kept: synced files, `/workspace` and `agent.db`.
  - The backend sees `restarts` move, reports `Starting` until 7443 answers, and then `Running`.
  - Open app and MCP streams see EOF and redial, as after a sandboxd restart.
  - After 3 restarts in 10 min, or 3 failed boots, the claim is `Failed`. `Ensure` then wakes it once more, since #269's wake resets the budget.
- **Host restart.** Running, hibernated and archived claims all come back, provided cocoon's `run_dir` is on persistent disk. On tmpfs, the copy-on-write disks are lost with the host (#269's finding), so cocoonbox nodes keep `run_dir` on the NVMe.
- **Node loss, or a claim that cannot come back** (VM record gone, or `Failed` after the wake):
  - The service treats a box with a state-layer head and no serving machine as a restore: a new claim, then `State.Restore` from the latest manifest, then `RUNNING`.
  - This is the same code path as Rebase. The loss window is the save cadence (15 min, or since the last quiesced save).
  - It is service-layer logic in `boxes`, never in a backend.

### 14.7 Upstream mechanism vs cocoonbox

| Upstream (cocoonstack, mechanism only) | State |
|---|---|
| per-claim env, `guest: false` secrets | sandbox#268, in review |
| dead-VMM cold restart, failed claims, `vmm_exit` / `vmm_restart` events | sandbox#269, in review |
| `vm list` verifies the VMM PID | cocoon#274, in review |
| re-deliver the claim env after a cold restart; VMM exits from cocoon daemon's event stream | in #269, stacked on #268 |
| golden disk size per pool (`storage_gib` → `cocoon vm run --storage`) | branch ready, not yet a PR |
| a per-claim boot counter in the sandbox row (claim, wake, unarchive, cold restart) | to open: one counter field; `Incarnation` derives from it |
| `CONFIG_INPUT_UINPUT` in the sandbox kernel | optional; the image kernel works meanwhile |
| binary `port_forward` framing | only if many full-rate viewers per node |
| sparse or compressed memory snapshots, sparse s3 export | backlog (§18) |

Stays in cocoonbox:
- `api/machines/sandboxd` (the index, renew loop, state mapping and the relay dialer);
- the `Endpoint` change through the edge, MCP and `dialer`;
- per-user gw keys in the keyring;
- the sandboxd `Deliver` (gw key as `guest: false` env, identity to the §12 path);
- restore-on-missing in `boxes`;
- the image additions;
- the sandboxd node config (pool, egress policy, secret, `egress_internal_allow`);
- all timing policy: idle, hibernate, save cadence.

sandboxd learns nothing about users, tiers or cocoonbox.

### 14.8 Changes and order

1. Upstream: #268 and #269 merged, the env re-delivery hook in, and a cocoon release carrying #274. Then the disk-size pool field.
2. **contracts.md:**
   - the static `agent.env` (placeholder gw key, no `EDGE_TOKEN`);
   - "Agent listen" on loopback via the relay;
   - the sandboxd section replacing "Boxes ↔ cluster" for this backend.
3. **api:**
   - `Endpoint` in `machines`, the edge, MCP and `dialer`, landing first with cocoonset unchanged in behaviour;
   - `Deliver`, `SetMeta` and `Incarnation` in `machines/sandboxd`;
   - then `machines/sandboxd`;
   - then config `backend: sandboxd | cocoonset`;
   - then restore-on-missing.
4. **image:** the rootfs build for `cocoon image import`, and the additions in 14.2 and 14.5.
5. **infra:**
   - the cocoonbox sandboxd instance on the bench host (its own port, data_dir, cocoon root with `run_dir` on the NVMe, fenced cores);
   - the pool and egress config;
   - per-user gw keys.
6. **Optional proto:** `Box.restarted_at_ms`, so the app can say "your box restarted, memory was lost". Without it, the app only sees a reconnect.
7. **Migration:** per user, SaveState on cocoonset, then `Ensure` on sandboxd, which restores through restore-on-missing. cocoonset stays configured as the fallback until the acceptance run passes. Then the default flips.

### 14.9 Acceptance run on the bench host

Every step runs through the public edge, with the app or grpcurl, on the sandboxd backend. Each step records its timings.

1. **M3 acceptance as it stands:**
   - `Ensure` from zero to `RUNNING`;
   - a `Hibernate` / `Ensure` round trip on a resumable operation stream;
   - agent RPCs reach the box only through the edge.
2. **Live view:** the app renders 1280×800 through `Session.Screen` over the relay. Record fps, time to first frame, input RTT, and sandboxd CPU per viewer.
3. **MCP:** from a local agent CLI with the app closed, wake the box, do a GUI task, save, hibernate (M4.7's run).
4. **State layer:**
   - SaveState, then Rebase vN → vN+1 on sandboxd;
   - `/workspace`, a logged-in Chrome cookie and `agent.db` are kept.
5. **H3:**
   - a gw call from the box appears in gw's log under that user's own key;
   - the key string is absent from the guest's `/proc/*/environ`, `/etc`, `/run` and disk, and from the box's hibernate memory file on the host;
   - `GET …/env` blanks it.
6. **Identity:**
   - a credential for another `box_id` or an old `wake_id` is rejected;
   - after a wake, the identity file carries the new `wake_id` before `RUNNING`.
7. **Crash:**
   - `kill -9` of the box's CH brings the box back to `RUNNING` with the marker kept, and the app reconnects by itself;
   - four kills leave it `Failed`, and `Ensure` recovers it.
8. **Dead api:**
   - with the api stopped past the lease, the box is archived;
   - with the api started, `Ensure` wakes it from the archive, state kept.
9. **Lost machine:** release the claim behind the service's back. `Ensure` restores from the state layer, and the loss window is reported.
10. **Flip:** `backend: sandboxd` is the default, and `backend: cocoonset` still serves an existing CocoonSet box.

The flip ran on 2026-09-30. On 2026-10-01 the cocoonset backend, the `backend` key and the K3s stack were removed; sandboxd is the only backend.

## 15. Proactive agents

Proposed 2026-10-01, for review. Status 2026-10-10: only the alarm half is built, as scheduled wake (15.13); routines, Tick, the lease, notices and budgets are not. Agents act without a prompt from the user:
- scheduled jobs ("every weekday at 8:00, summarize my inbox");
- event triggers (a file lands in `/workspace`, a page changes, a connector event, a box wake);
- watches that end themselves;
- follow-ups that the agent schedules for itself ("check again tomorrow").

The result reaches the user in the app. A risky action waits for the user, as take-over does today.

Constraints:
- Boundaries: the api owns timing, budget and policy. The agent owns the content: routine definitions, prompts, runs and tool gating. Infra gives only mechanism (`Ensure`, `Hibernate`, wake). No sandboxd timer carries product timing. `idle_hibernate_seconds` and the lease stay as §14.3 sets them.
- Credentials (§12, boundaries.md): a proactive run never spends a credential without a standing grant from the user. The api never stores a prompt or a transcript. The only run content it holds is a short notice summary, encrypted at rest with the user's state key (15.6).
- A routine is user data. It lives in `agent.db` and travels with the state layer (§11).

### 15.1 How others do it

| | A hosted assistant's scheduled tasks (public docs, not verified here) |
|---|---|
| Definition | a one-off or recurring prompt, about 10 active per user |
| Where it lives | the vendor's cloud |
| Machine off | not applicable (no user machine) |
| Run | a new chat turn |
| Result | push and email |
| Safety | the vendor's policy |
| Self-scheduling | yes, from chat |

### 15.2 Where a trigger lives and who wakes the box

The box keeps the routines; the api keeps an alarm clock.

- **In the box:** a `routines` table in `agent.db` holds `id, owner_agent_id, name, prompt, trigger, precheck, grant, state (active | paused | pending_approval), created_by (user | agent), next_due_ms, last_run`, plus a `routine_runs` table.
- **In the api:** a per-box row `{next_wake_at_ms, paused, spend_today}`. It holds no prompt, no routine name and no trigger text.
- **Tick** (new, api-only, like `State`): `RoutineControl.Tick(TickRequest{now_ms, policy, ack_through})` returns a stream of run events and ends with `{next_due_ms, active_runs}`.
  - The api calls it at `next_wake_at` after `Ensure` wakes the box. It also calls it every 5 min on each running box as a heartbeat, and once before every `Hibernate` to learn the next alarm.
  - `now_ms` comes from the api. A woken guest's clock may lag until NTP syncs (§12), so due times are judged against the api's clock.
  - Run events are rows in `agent.db`. The api acknowledges them with `ack_through` on the next Tick, and the agent re-sends what is not acknowledged, so a broken stream or an api restart loses no report.
- **Policy lease:** each Tick carries the policy `{lease_until_ms (now + 15 min), wake_id, paused, cancel_running, runs_left_today, min_interval_s, max_run_s}`. The api folds every budget into `runs_left_today` (15.9f), so the agent never handles money.
  - Between Ticks, the agent fires due routines itself only while it holds an unexpired lease. A minute-level routine on a running box therefore needs no api round trip per fire.
  - If the api is lost, firing stops within 15 min (fail closed).
  - The lease holds only under the `wake_id` it names (§12). After a wake, nothing fires until the first Tick, whatever the guest clock says.
  - A kill switch takes effect on the next heartbeat, or at once through a direct Tick with `paused: true`.
- **After the api wakes a box for an alarm,** it hibernates the box again when the Tick stream ends and no client is attached. User-driven boxes keep today's rule: they hibernate only on request.
- **Missed fires** (box down past the time, api down, lease lapsed): on the next Tick the agent runs each missed routine once, if the miss is inside its catch-up window (default 1 h). Otherwise it records the fire as `missed`. It never replays a backlog.
- **Coalescing:** routines due within 5 min of an alarm run in that same wake.

Event triggers:

| Trigger | Source | While hibernated |
|---|---|---|
| time (`at`, cron + IANA zone) | agent scheduler, api alarm | wakes the box |
| file under `/workspace` | toolhost inotify, reported to `serve` | none: files arrive only through a running box |
| page change | a cron routine with a precheck (15.4); no separate watcher | as time |
| connector event (mail, GitHub, Slack; M5) | a webhook to the api, which wakes the box and calls `Routines.Deliver{routine_id, event}` | wakes the box |
| box wake | the Tick after a wake carries `reason: resume`; routines with `on: resume` run | — |

Hot path: none. The alarm index is a min-heap in the api's service layer, off the edge path. The heartbeat costs one RPC per running box per 5 min.

### 15.3 In the box: a headless run

- **Owner agent:** a run is a turn on the routine's owner agent.
  - A follow-up the agent schedules continues the conversation it came from.
  - A routine the user makes in the app gets its own agent, named after the routine.
  - The user entry of the turn carries `origin: routine:<id>` (new `TranscriptEntry` field). The app shows it as a trigger, not as something the user typed.
- **Queue:** the turn goes through the existing per-agent queue (mode A loop), so it never interleaves with the user's own turn on that agent.
- **Silent runs:** a run that finds nothing new ends without a message. The prompt instructs this, and the run record still exists.
- **Tools:** the agent's `schedule` tool takes `create | update | pause | delete | list`. Its fields are `name`, `prompt`, one `trigger`, an optional `precheck`, `expires_at`, and `grant` (15.5).
  - A watch has to set `expires_at`, and the tool refuses a watch without it.
  - Routines the agent creates start `active` only inside the default grant and the frequency floor. Otherwise they start `pending_approval` (15.5).
- **App management:** `cocoonbox.agent.v1.Routines` through the edge offers `List`, `Update` (pause, resume, grant, delete), `RunNow` and `Runs`. The app and MCP clients manage routines through it.
  - `Tick` and `Deliver` sit on a separate service, `RoutineControl`, which the edge refuses by prefix as it refuses `State`.
  - Two services are needed because the agent cannot tell an api call from a client call: both carry the same edge-signed credential (`agent/src/identity.rs:56`). The edge's prefix rule (`api/edge/proxy.go:35`, `:81-83`) is the only gate.
- **Mode B:** runs have no mode B, because the model and the subscription stay on the Mac, which is usually closed while the user is away.
  - A mode-B client (a local agent CLI) can manage routines through three MCP tools: `routine_list`, `routine_create` and `routine_pause`.
  - The routines it creates then run in mode A with the box's model, under the same grant and budget.

### 15.4 Budget and limits

The api enforces every limit through the policy lease. The per-box gw key cap (§12.2) remains the hard backstop for model spend. Every default below is api config, not code; the wake floor and runs per day get tuned from the cost of a run (15.8).

| Limit | First-cut default | Why |
|---|---|---|
| routines per user | 20 active | bounds the alarm index and the prompt |
| wake floor | 1 h between alarms that wake a hibernated box | a wake plus a re-hibernate costs about 1 s up and 24 s writing 8 GiB of memory (15.8), and the write uses the node's NVMe bandwidth; minute-level routines run only on a box that is already up |
| runs per day | 48 | bounds machine time |
| run wall time | 10 min, then the turn is cancelled | a stuck GUI step must not hold the box up |
| proactive budget per day | box-seconds and model cost, per user (15.9f) | spend control separate from interactive use; the dual meter of the cost model |
| failures | backoff 1, 2, 4 … 24 h; auto-pause after 5 in a row; pause at once on an auth failure | stops loops that burn tokens |
| away guard | no app visit for 3 days and 15 unread notices → one notice "keep or pause"; no answer in 3 more days → pause all | spend while the user is gone |
| kill switch | per routine, per user ("pause all"), global (api config) | operations |

**Precheck:** a routine may carry a shell command that runs as `user` without a model, with a 30 s limit. The model turn starts only if the command's output differs from the last run's, or on exit code 0 with non-empty output. A page watch is then `curl -s … | sha256sum` or a small script, and it costs no tokens while nothing changes.

### 15.5 Safety

- **Grants:** each routine carries a grant, set or confirmed by the user in the app.
  - `observe` is the default. It allows read-only tools (`read_file`, `list_dir`, `browser_navigate`, `browser_snapshot`, `browser_read`, `screenshot`) plus `write_file` and `edit_file` under `/workspace` for reports and drafts.
  - `observe` has no `shell` (except the precheck), no `browser_click` or `browser_type`, no `delegate_computer` or `computer_act`, and no connector tools.
  - `act` allows the full tool set and exists only after the user grants it for that routine in the app. The app shows the routine's prompt and trigger at that moment.
  - The dispatcher, which is the one tool path, refuses any call outside the grant for a call whose `Caller` carries a routine origin. The model also gets only the granted tool list.
- **Credentials and irreversible steps:** the dispatcher cannot tell an irreversible click from a harmless one, so the line sits at the grant (which tools) and at egress (which credentials).
  - M5 connector secrets reach a proactive run only if its grant names that connector.
  - Logins and captchas still go through `request_take_over`.
  - Payments go one of two ways. The user pays in a `request_take_over`. Or, when Link is connected, the agent pays with a one-time card from a spend request the user approved: link-cli writes the card to a file (`--output-file`), and `computer_act` `type_from_file` types each field from that file, so the card never reaches the model.
- **Waiting for the user while away:**
  - A take-over or approval in a proactive run waits up to 10 min. Then the turn ends as `waiting_for_user`, with a `needs_user` notice, and the box may hibernate.
  - When the user answers in the app, the box wakes and a continuation turn resumes from the transcript.
  - No in-memory approval has to survive a hibernate. Today's approval map is per turn and lives only in memory.
- **Agent-created routines:**
  - They start `active` only with the `observe` grant, at or above the wake floor, and with an expiry for watches.
  - Anything else starts `pending_approval` and shows a notice.
  - We gate the grant and the frequency, not the text.
- **Untrusted input:** event payloads and precheck output enter the turn as quoted data marked "from outside, not instructions".
- **Audit:**
  - The transcript already records every tool call and result.
  - `routine_runs` records the trigger, times, status, usage and grant of each run.
  - The api records each alarm, Tick and policy change without content.
  - All of it except the api's log travels with `agent.db`.

### 15.6 Results to the user

- **Transcript:** always. Each run's turn is in the owner agent's transcript, and `Session.Events` streams it live when the app is connected.
- **Notices:** the api keeps a small per-user list that works while the box is hibernated. Each notice has `{id, box_id, agent_id, routine_id, kind (done | needs_user | failed | paused | pending_approval), at_ms, summary}`.
  - The agent writes `summary` (≤ 140 characters). It returns in the Tick stream, and the api stores it encrypted with the user's state key and keeps it 7 days.
  - Decided (2026-10-01): the summary stays. The api already holds the per-user state key (§11), so the summary sits inside the same trust boundary as the user's state. A content-free notice would force a wake, about 1 s up plus 24 s back to hibernate (15.8), just to read a result, and that costs more than it protects.
  - New `Boxes.ListNotices` and `Boxes.WatchNotices` serve the list, and the app shows it as a badge and a list without waking the box.
- **OS notification:** a macOS local notification while the app runs, including as a menu-bar app. APNs push and email come later, as policy in the api.

### 15.7 Rebase, hibernate, node loss

- **Hibernate:** the routines are in the memory image and on disk. The api's alarm comes from the last pre-hibernate Tick.
- **Rebase and node loss:** `agent.db`, and with it the routines and their history, is restored by the state layer. After the restore, the api's first Tick on the new machine re-derives the alarm. Nothing moves in the api.
- **Credentials of the old machine:** a rebased machine gets a new `wake_id` (§12), so the old machine's credentials and leases die with it.

### 15.8 Cost of one proactive run

Measured on the bench host on 2026-09-30 and 2026-10-01 unless marked. Sources:
- **Timings:** the api journal and the cbsbx `usage.jsonl`.
- **Tokens:** gw's own ledger: 142 model calls in 22 turns from the SQLite ledger and the PostgreSQL one. Calls of one machine less than 90 s apart count as one turn. Cost is gw's `cost_micros` at the OpenRouter list prices in `deploy/host/gateway.yaml`: `deepseek/deepseek-v4.1-flash` $0.30 / $1.20 per M input / output, with cache reads at 0.02×; `anthropic/claude-sonnet-5` $2 / $10.
- **Machine:** `c3-highcpu-4`, which is the box's exact shape (4 vCPU / 8 GB). C3 is the nested-virt family the bench host runs on (`c3-standard-22-lssd`). Prices come from gcloud-compute.com's copy of Google's price list, dated 2026-09-27:
  - list: $0.1701/h in us-central1 and in us-east4;
  - Spot: $0.0641/h in us-central1 and $0.0433/h in us-east4.
  
  Spot prices move daily, so re-check them before reuse. For comparison, `c3d-highcpu-4` (AMD, which needs PVM) is $0.1499/h list and $0.0366/h Spot in us-central1.

Pricing rule: a box holds its slot (4 vCPU, 8 GiB) from the wake call until its hibernate finishes. Machine cost = box-seconds × the slot price. A freed slot saves money only if another box uses it. The slot price is the planning line, as in the free-tier cost model.

| Step | Time | Source |
|---|---|---|
| wake: hibernated → `RUNNING` (memory restore, identity written) | about 1 s | wake op DONE in 1.0 s; 2026-10-01 sandboxd `wake` at 10:03:39.95, api "provisioned … as wake" at 10:03:40 |
| run, precheck with unchanged output | about 2 s (limit 30 s) | estimate: `curl` plus `sha256sum` |
| run, text turn (text-model main agent) | model calls span a median of 4.5 s and at most 197 s; with tool time, planned at 45 s | gw ledger, 12 text turns |
| run, GUI turn (vision computer subagent) | model calls span a median of 27.5 s and at most 92 s; planned at 150 s | gw ledger, 10 turns |
| hold: final Tick, then an incremental live save | about 1 s plus 1–3 s | api journal: Rebase saves "saved 1s", "2s", "3s" |
| re-hibernate: write 8 GiB of memory to the local NVMe | 24 s (23–50 s) | api "hibernating box" at 10:03:09, sandboxd `hibernate` at 10:03:32.98 |

**vCPU-seconds.** These are not measured. The guest is mostly idle while it waits on the model, and Chrome page loads are the busy part. M4.9f records them per run from the guest's `/proc/stat`. A host-side count needs per-claim CPU-seconds in sandboxd. sandboxd records `wake` and `hibernate` events in `usage.jsonl` but no CPU counter, so this goes upstream as mechanism (15.9f).

**Per run:**

| Run | Box-seconds | Machine, list | Machine, Spot | Tokens through gw | Total, list | Total, Spot |
|---|---|---|---|---|---|---|
| precheck, output unchanged (no save) | 28 | $0.0013 | $0.0003–0.0005 | $0 | $0.0013 | $0.0003–0.0005 |
| text | 73 | $0.0034 | $0.0009–0.0013 | $0.0035 mean (median 0.0014, max 0.012) | $0.0069 | $0.0044–0.0048 |
| GUI | 179 | $0.0085 | $0.0022–0.0032 | $0.051 mean (median 0.020, max 0.28) | $0.059 | $0.053–0.054 |
| any run held to the 10 min wall | 628 | $0.030 | $0.0076–0.011 | bounded by the budget (15.9f) | | |

- A run on a box that is already up for the user adds only its tokens: the user's session pays for the box anyway.
- The fixed part of a wake (wake, Tick, save, hibernate) is 28 s, or $0.0013 at list. The re-hibernate alone is 24 of a text run's 73 box-seconds.
- Tokens are half of a text run and 86% of a GUI run. Box time is nearly all of a precheck-only run.

**Per user-month** (30 days; "text" and "GUI" as above):

| Usage | Alarm wakes a day | Box-hours a month | Machine, list | Machine, Spot | Tokens | Total, list | Total, Spot |
|---|---|---|---|---|---|---|---|
| 3 text runs a day, each its own wake | 3 | 1.8 | $0.31 | $0.08–0.12 | $0.32 | $0.63 | $0.40–0.43 |
| 3 GUI runs a day | 3 | 4.5 | $0.76 | $0.19–0.29 | $4.58 | $5.34 | $4.77–4.87 |
| §15.4 caps: 48 text runs a day, 1 h floor, so at most 24 wakes | 24 | 23.6 | $4.01 | $1.02–1.51 | $5.07 | $9.08 | $6.09–6.58 |
| the same caps, GUI runs | 24 | 65.6 | $11.16 | $2.84–4.20 | $73.30 | $84.46 | $76.14–77.50 |
| the same caps, every run held to the 10 min wall | 24 | 245.6 | $41.78 | $10.63–15.74 | up to the gw key cap ($10 a day) | | |
| box up all day, for comparison | — | 730 | $124.17 | $31.61–46.79 | the same per run | | |

- Realistic use (3 text runs a day) takes 0.25% of an always-up box's machine hours, at $0.63 a month at list price. That is 3.7 min of box time a day, against the cost model's free-tier grant of 10 min a day.
- The caps from §15.4 alone allow $9 a month for text runs and $84 for GUI runs. The run cap and the wake floor bound frequency, not spend. The day budgets in 15.9f are what bound spend, and the gw key cap of $10 a day is only the backstop.
- **Disk:** every alarm wake writes 8 GiB to the node's NVMe, at about 340 MiB/s (8 GiB in 24 s).
  - At the floor that is 192 GiB a day per user; at 3 runs a day it is 24 GiB.
  - Hibernates run one at a time give one drive 3600 hibernates a day: 150 users at the floor, or 1200 at 3 runs a day. Parallel hibernates on one drive have not been measured.
  - Each hibernate also evicts the neighbours' page cache (§18).
  - So the wake floor protects node bandwidth as well as money. The zero-page skip (§18: 8.00 → 3.22 GiB on the golden) would cut this write by about 2.5×.
- **Dual meter** (cost model): box-seconds and token cost are metered separately, and each has its own day budget. A single meter leaves the other unbounded: a precheck loop burns box time with no tokens, and a GUI run burns tokens in little box time.
- The hibernated footprint (memory file and disk on the NVMe) is the same with or without routines, so it is not a per-run cost.

### 15.9 Build plan (M4.9a–f)

The contract lands on main first (boundaries.md); then the phases follow in order. Every phase is gated by build, tests and lint. Bench host acceptance runs at the end of b, c, d, e and f. Routines stay off for every user not in `routines.users` (the same pattern as `state.scheduled_users`) until 9e and 9f are done.

**Contract.** `proto/cocoonbox/agent/v1/routines.proto`, sketched here; the fields are the contract to refine:

```proto
service Routines {        // clients, through the edge
  rpc List(ListRoutinesRequest) returns (ListRoutinesResponse);
  rpc Update(UpdateRoutineRequest) returns (Routine);         // pause, resume, grant, delete
  rpc RunNow(RoutineRef) returns (RunAccepted);
  rpc Runs(RunsRequest) returns (RunsPage);                   // history, newest first
}
service RoutineControl {  // api only; the edge refuses it like State
  rpc Tick(TickRequest) returns (stream TickEvent);
}
message Policy { int64 lease_until_ms = 1; string wake_id = 2; bool paused = 3; bool cancel_running = 4;
                 int32 runs_left_today = 5; int32 min_interval_s = 6; int32 max_run_s = 7; }
message TickRequest { int64 now_ms = 1; Policy policy = 2; TickReason reason = 3; uint64 ack_through = 4; }
enum TickReason { ALARM = 0; HEARTBEAT = 1; PRE_HIBERNATE = 2; RESUME = 3; CONTROL = 4; }
message TickEvent { uint64 seq = 1; oneof kind { RunStarted started = 10; RunEnded ended = 11; Notice notice = 12; TickEnd end = 13; } }
message RunEnded { string run_id = 1; string routine_id = 2; string agent_id = 3; RunStatus status = 4;
                   int64 started_ms = 5; int64 ended_ms = 6; repeated ModelUsage usage = 7; int64 guest_cpu_ms = 8; }
message TickEnd { int64 next_due_ms = 1; int32 active_runs = 2; }
```

The rest of the contract:
- `TranscriptEntry.origin = 6`, a string (`routine:<id>`). The body is a blob, so old rows decode as `""`.
- A `routine` event (oneof field 16) on `Session.Events`.
- On `Boxes`: `ListNotices`, `WatchNotices`, `DismissNotice`, `SetRoutinesPaused` and `ProactiveUsage`.
- contracts.md: `RoutineControl` is api-only.

An alarm Tick streams until every run it started has ended. A heartbeat Tick returns once it has started what is due, and those runs report on a later Tick.

#### M4.9a Routines in the box (agent)

Scope:
- `agent/src/store.rs`:
  - `routines` and `routine_runs` go into `SCHEMA` (:12-42). `CREATE TABLE IF NOT EXISTS` covers new tables; there is no migration mechanism, and none is needed for them.
  - `restore_from` (:276) re-applies `SCHEMA`. Today it swaps in the restored database without it, so a restored pre-routines `agent.db` would lack the tables until the next process start.
  - `delete_rows` (:416) also deletes the agent's routines.
  - `queue_turn`, which builds the user entry at :201-210, takes an `origin`.
- `agent/src/runner.rs`:
  - New `queue_routine` next to `send` (:115). It enqueues on the same per-agent slot (`slot`, :373), skipping attachments and auto-title. `send` is the only enqueue path today.
  - `run_turn` (:399): a `max_run_s` sleep arm in the `select!` (:425-429) for routine turns, which ends them as `timeout`. Today no turn has a wall time; the only limits are steps (40 main, 30 computer) and the model client's 180 s read timeout.
  - `take_over` (:555): a routine turn waits at most 10 min, then ends as `waiting_for_user`. Today `rx.await` (:571) has no timeout; the turn lets go of the turns read lock while it waits.
  - Usage per model (main and computer, `computer.rs:92`) goes into the run row. `TurnEnd` keeps one sum, as now.
  - `main_tool` (:533-546): the grant check for `delegate_computer` and `request_take_over`, which never reach the dispatcher. The tool list (:473-477) and the system prompt (`prompts.rs:3-18`) show only granted tools.
- `agent/src/dispatch.rs`:
  - `Caller` (:64) gains `grant: Option<&Grant>`.
  - `execute` (:122) refuses any tool outside the grant's allowlist before `self.tools.run` (:138). It returns `ToolOutput::error` through `record_result`, so the refusal lands in the transcript.
  - Write paths are normalized and checked under `/workspace`, because `files::resolve` accepts absolute paths.
  - The same allowlist then applies to every mode-A turn, which closes the §18 item "Allowlist the tools the mode-A runner forwards".
  - The internal toolhost names it keeps out include `credential_fill` and `active_origin` (§20), which only `Session.FillCredential` and `Session.ActiveOrigin` call.
- New `agent/src/routines/`:
  - trigger parsing and next-due times (`at`, 5-field cron with an IANA zone, `@every`);
  - the in-box firer under the lease;
  - catch-up, coalescing, failure backoff;
  - the `RoutineControl.Tick` server, with the clock offset `now_ms − guest now` taken at each Tick.
- `agent/src/tools.rs`: the `schedule` tool in `main_tools` (:10), with the `observe` grant only.
- `agent/src/server.rs`:
  - `serve_on` (:319) registers `Routines` and `RoutineControl` behind `EdgeAuth`;
  - `recover` after a restore (:287) also closes running runs as `interrupted`.
- api: `edge/proxy.go` refuses `/cocoonbox.agent.v1.RoutineControl/` next to `stateServicePrefix` (:35, :81-83).

Acceptance:
- `cargo test` cases:
  - cron next-due across both America/New_York DST changes;
  - `at` in the past;
  - catch-up inside and outside 1 h;
  - coalescing within 5 min;
  - no fire under an expired lease or a lease for an old `wake_id`;
  - refusals of `computer_act`, `shell`, `delegate_computer` and a write to `/etc` under `observe`, each in the transcript;
  - the wall-time end;
  - the take-over timeout ending as `waiting_for_user`;
  - a restore re-applying `SCHEMA`.
- `go test ./edge` with a `RoutineControl` case answering `PermissionDenied`.
- On the bench host, with the new agent on the dev box (bench.md, Deploying a binary into a box): a routine created from chat appears in `Routines.List` through the edge, and grpcurl `RoutineControl/Tick` through the edge is refused.

Left out: the api side, the precheck, notices.

#### M4.9b Alarms: wake, Tick, re-hibernate (api)

Scope:
- New `api/boxes/routines.go`: `RunAlarms(ctx, cfg)`, a min-heap of `{box, next_wake_at_ms}` in the service layer. `main.go` starts it next to `RunScheduler` (:198).
- `RoutineAgents` is an interface beside `StateAgents`. The dialer implements it as a sibling of `Dialer.state`, on the shared `dialer.Pool`.
- Routines need the state layer. The dialer is built only when `state.bucket` is set (`enableState` in `main.go`), and the notices of 9d live in the store, so `validate` refuses `routines` without the state layer.
- **Per-box record** `<box>/proactive.json` in the state store, written only by the api through `putJSON` and `getJSON` (`statelayer.go:359-376`):
  - fields: `next_wake_at_ms`, `paused`, the day counters, `last_seen_ms` and the away state;
  - at start, the api rebuilds the heap from `machines.List("")` and these objects;
  - claim meta (`SetMeta`) is rejected for this record: a Rebase makes a new machine and drops its meta (`rebase.go:55`), and every meta write re-emits `Watch`.
- **Alarm path:**
  1. Wrap the context with `auth.WithUser`, because the loop has no caller (as in `controlmcp.go:84`).
  2. `Ensure` (`boxes.go:46`), then `waitRunning` (`rebase.go:103`).
  3. `Tick{reason: ALARM}`, read to its end.
  
  If a hibernate is still in flight, the path waits for `HIBERNATED` first. Otherwise sandboxd `Ensure` returns `ActionNone` (`machines/sandboxd/sandboxd.go:114-115`), and the CREATING operation would wait 10 min.
- **Re-hibernate:** when the stream ends, no client is attached and no operation is active (`ops.activeFor`), the api runs:
  1. a live save (`startSave`), waiting for it to finish (open decision 4);
  2. a `PRE_HIBERNATE` Tick;
  3. `Hibernate`.
  
  `Hibernate` does not check `activeFor` and would supersede a save (`operation.go:162-164`), so the alarm path waits. For the same reason a Tick is never an operation.
- **Heartbeat:** every 5 min, Tick each ready box (`ready`, `provision.go:153`) of `routines.users`, in the shape of the `saveStale` loop (`save.go:57-77`).
- **Pre-hibernate Tick** in `Hibernate` (`boxes.go:146-149`, before `s.machines.Hibernate`), with a 2 s timeout. If it fails, the box hibernates anyway and keeps its last alarm.
- **Attachment:** nothing tracks clients today, and `accesslog.go` logs only `Boxes` calls.
  - The edge counts open `Session.Events` and `Session.Screen` streams per box in `pump` (`edge/proxy.go:102-111`), and stamps MCP requests (`edge/mcp.go:77-83`).
  - It reports through a `Presence` interface declared in `boxes` and wired in `main`, so the edge still imports no service code.
  - A box is attached while such a stream is open, or for 10 min after an MCP call.
  - When a client attaches during an alarm run, the box becomes user-driven and stays up.
  - The edge's own count is used, not the box's: a box-reported flag would let `user` (who has sudo) keep the box up at our cost.
- **Config:** a `Routines` block in `config.Config` (`config.go:101-114`): users, heartbeat, lease, catch-up, wake floor, max run, and the 15.4 limits. Defaults go in `applyDefaults` and checks in `validate`. The load is strict, so the block ships with the binary that reads it.

Acceptance, on the bench host with the dev box hibernated and the app closed:
- A routine due in 10 min fires. `journalctl -u cocoonbox-api` shows alarm → `RUNNING` → Tick end → save → hibernate, with each step's time. The cbsbx `usage.jsonl` shows exactly one `wake` and one `hibernate`, and `Routines.Runs` shows the run `done` with its usage.
- The same with the app showing the box: the box stays up.
- With the api stopped, a minute-level routine on a running box stops writing its marker within 15 min. After a restart, the heap rebuilds and the next alarm fires.
- After a Rebase, the routine keeps its history and fires on the new machine at its next due time.

Left out: notices (the result is only in the transcript), budgets beyond the lease.

#### M4.9c Precheck (agent)

Scope:
- `routines/` runs the precheck through the toolhost shell as `user`, with a 30 s limit (the shell's default is 120 s, `toolhost/shell.rs:16`).
- It stores the exit code and a hash of the output on the routine. The model turn starts only on changed output, or on exit 0 with output (15.4).
- The output, capped at 8 KiB, enters the turn as quoted data marked "from outside, not instructions".
- A run that ends at the precheck is recorded as `skipped_precheck`, with its time and no tokens.

Acceptance: a routine with the precheck `curl -s https://example.com | sha256sum` fires on three alarms. All three runs are `skipped_precheck`, and the gw ledger has no row for the machine in those windows. Changing the command's output gives one model turn.

Left out: any precheck isolation beyond `user`'s shell.

#### M4.9d Notices and the app

Scope:
- **agent:** `Notice` events in the Tick stream, of kind `done` (the run produced a message), `needs_user`, `failed`, `paused` or `pending_approval`.
  - The summary has at most 140 characters.
  - The model writes it as the run's last line; the agent writes it for failures.
- **api:**
  - Notices live in `<user>/boxes/<box>/notices.json`, encrypted with AES-256-GCM under the user's state key (`keyring.Keyring.StateKey`, §22.5). They are sealed with `keyring.Sealer` under an `Info` string of their own.
  - They are kept 7 days, and the api is the only writer.
  - `ListNotices`, `WatchNotices` and `DismissNotice` serve them.
  - The api adds its own notices for what only it sees: a box that did not wake, a run stopped at the api's guard (15.10).
- **app (`macos/`):**
  - `BoxConnection.swift` gains the notice and routine calls.
  - `AppModel.swift` keeps the notice list and the badge, from `Boxes` only, so it works while the box is hibernated.
  - `RootView.swift` adds a routines list per agent: pause, resume, run now, delete.
  - `ConversationView.swift` shows `routine:<id>` entries as a trigger line, not as user text.
  - A local notification goes through `UNUserNotificationCenter` while the app runs.
  - MockBox (`Services.swift`, `MockWorld.swift`) serves both services for the app's tests.

Acceptance, on the bench host:
- The HN digest end to end, with the box hibernated: "every weekday at 08:00 Asia/Singapore, write the top 5 Hacker News stories into /workspace/digest.md, and tell me only if there is a new AI story".
  - The routine appears with the `observe` grant. At 08:00 the box wakes, the run writes the digest and the box hibernates again.
  - `ListNotices` shows the summary while the box is hibernated, and the app shows the badge.
  - Opening the list causes no wake: `usage.jsonl` has no extra `wake`.
  - Recorded: wake to run start, run time, tokens, re-hibernate time.
- The summary text does not appear in the stored object.

Left out: APNs and email.

#### M4.9e Limits, kill switches, away guard

Scope:
- **agent:**
  - failure backoff and auto-pause (15.4), with an immediate pause on gw 401 or 403;
  - `pending_approval` for agent-created routines outside `observe` or below the wake floor.
- **api:**
  - `SetRoutinesPaused` ("pause all") writes `proactive.json`.
  - The global kill switch is `routines.enabled: false` plus a restart, since the api has no config reload. At start, the api sends a `CONTROL` Tick with `paused: true` to every running box.
  - The away guard reads `last_seen_ms`, the last client attachment (9b): it applies the 15.4 rule with one notice, then a pause.
  - Wake limits per node: at most 4 alarm wakes and 2 alarm hibernates at a time.
  - Cron alarms due in the same minute spread over a flex window, by a hash of the box id (open decision 3).

Acceptance:
- "Pause all" in the app stops the next alarm.
- The global switch stops heartbeat-driven fires on a running box within one heartbeat.
- The away guard in `go test ./boxes`, with a test clock.
- Removing the machine's `GW_KEY` (`PATCH env`) makes the next run fail with 401 and pause its routine.
- 20 routines due at 08:00 on one box run in one wake.

Left out: tiers.

#### M4.9f Cost metering and budgets

Scope:
- **agent:** each run row gets its usage per model and `guest_cpu_ms` (the guest's `/proc/stat` delta, reported by the guest).
- **api, per alarm wake:** box-seconds from the wake call to `HIBERNATED`, split into wake, run, hold and hibernate.
- **api, gw reads:**
  - `GET /admin/usage/users?since=<day start>` with the tenant token, once per heartbeat for all boxes. It returns rows by (user, model), with tokens and `cost_micros`.
  - gw attributes a request to the key's owner, which is the machine id, ahead of any client hint (`attributed_user`). So the guest cannot move spend to another box, and the rows are authoritative per machine.
  - Rates per model derived from those rows price the agent-reported usage of runs on a box that is up for the user. The api keeps no price table.
  - A Rebase splits one day over two machine ids, and the api sums them by box.
- **api, budgets:** day budgets per user in `proactive.json`, `box_seconds_per_day` and `cost_micros_per_day` (open decision 1).
  - When either is spent, the next lease carries `runs_left_today: 0`, and the api wakes no box for an alarm.
  - The agent records such a fire as `missed: budget`.
- **api, read side:** `Boxes.ProactiveUsage{days}` returns per day: alarm wakes, box-seconds, runs, and cost both as the agent reported it and from gw.
- **app:** a Proactive panel with today's use against the budget, 30 days of history, and the runs of each routine from `Routines.Runs`.
- **Upstream mechanism, later:** per-claim CPU-seconds in the sandboxd row, from the VM's cgroup `cpu.stat`. This is the cost model's third meter. Until then, CPU-seconds are only what the guest reports.

Acceptance, on the bench host, five alarm runs:
- For each run, the api's box-seconds fall within 2 s of the wake-to-hibernate span in sandboxd's `usage.jsonl`. This is the second signal for machine time.
- The agent-reported cost is within 10% of gw's ledger for the same window. This is the second signal for tokens.
- With a budget of one run's cost, the second alarm causes no wake, and the next Tick records `missed: budget`.

Left out: billing, tiers.

### 15.10 Failure handling

| Failure | What happens |
|---|---|
| api down at an alarm | No wake. At start, the api rebuilds the heap from `proactive.json`, and an alarm in the past wakes its box at once, within the node's wake limits. The agent runs each missed routine once inside its catch-up window and records the rest as `missed`. It never replays a backlog. |
| api down while a box is running | In-box fires stop when the lease ends, within 15 min. |
| box fails to wake (`Ensure` error, `Failed`) | Retries at 1, 2 and 4 min. Then an api notice `failed` ("your box did not wake for a scheduled run"), and the alarm moves one wake floor ahead. A machine that is gone goes through restore-on-missing (§14.6), which brings back `agent.db` and its routines from the last save. |
| run past the wall | The agent cancels the turn at `max_run_s` (status `timeout`, counted as a failure for backoff). The api has its own guard: if an alarm Tick stream is still open at `max_run_s` + 2 min, it sends `cancel_running: true`, waits 30 s, and then hibernates anyway. A turn caught in a hibernate image probably fails on the next wake, since its model stream is gone; this is unverified, and 9b measures it. |
| approval nobody answers | After 10 min the turn ends as `waiting_for_user`, with a `needs_user` notice, and the box may hibernate. The user replies in the conversation; a continuation turn comes later (15.5). An M5 connector approval takes the same path (§16.5). |
| Rebase mid-run | A Rebase saves QUIESCED, which waits 30 s for running turns and then fails (`state/mod.rs:539-542`). So `runRebase` (`rebase.go:42`) first sends a `CONTROL` Tick with `paused: true, cancel_running: true`. Cancelled runs end as `interrupted`, which is not a failure, and fire once on the new machine if they are still inside their catch-up window. |
| restore-on-missing mid-run | The run is lost with the machine. The restored `agent.db` may predate the run's start, so the run can repeat inside its catch-up window. Under `observe`, a repeat only rewrites files in `/workspace`. `act` will need a per-run idempotency key (open decision 6). |
| VMM crash (cold restart, #269) | Memory is lost. `recover` ends open turns as failed and marks the run `interrupted`, and the next Tick fires it again inside its catch-up window. |
| clock | The api sees only UTC `next_wake_at_ms` and never parses a trigger. The agent judges due times by the api's `now_ms` plus the offset measured at each Tick. After a wake, nothing fires before the first Tick (the lease's `wake_id`). |
| time zones and DST | Cron is evaluated in the routine's IANA zone with the image's tzdata (`/usr/share/zoneinfo`). A local time that does not exist (spring forward) fires once, at the first instant after the gap. A local time that occurs twice (fall back) fires once, at its first occurrence. `@every` is an absolute interval and ignores DST. The app shows the next due time in the routine's zone and in the Mac's. tzdata changes arrive with image cuts, so a zone whose rules changed keeps the old rule until the next cut. |
| duplicate reports | Run events are rows with a sequence number, acknowledged through `ack_through`. The api dedupes by `run_id`. |

### 15.11 Observability

**Per run** (agent, `routine_runs`; it travels with `agent.db`; the newest 100 per routine are kept):
- `run_id`, `routine_id`, `agent_id`, `turn_id`;
- trigger: `alarm`, `heartbeat`, `run_now`, `resume` or `catch_up`;
- `due_ms`, `started_ms`, `ended_ms`;
- status: `done`, `silent`, `skipped_precheck`, `timeout`, `failed`, `waiting_for_user`, `interrupted` or `missed` (with a reason: `budget`, `lease`, `window`);
- the precheck's exit code, time and whether its output changed;
- usage per model (input, cached, output), tool calls, refusals, the grant, and `guest_cpu_ms`.

**Per alarm wake** (api journal, structured, never content):
- box and machine;
- the alarm time, then wake, `RUNNING`, Tick end, save end, hibernate start and hibernate end;
- box-seconds per step, the number of runs, whether a client was attached at the end, and gw's cost for the machine in the window.

**Per policy change** (api journal): pause all, resume, the kill switch, an away-guard pause.

**Per day and user** (api, `proactive.json`): alarm wakes, box-seconds, runs, cost as reported by the agent and from gw, and the budget left.

**Second signals:**
- The api's box-seconds against the `wake` and `hibernate` times in sandboxd's `usage.jsonl`.
- The agent's tokens against gw's ledger.
- A gap over 10% is logged as `meter_mismatch`.

**How the operator sees it:**
- In the app, the Proactive panel (9f) shows today's use against the budget, 30 days of cost per day (box time and tokens, at list price), and every routine's runs with their status and cost.
- On the bench host, `journalctl -u cocoonbox-api | grep routines` shows the wakes; `Boxes.ProactiveUsage` through grpcurl gives the numbers; and gw's `/admin/usage/users` gives the spend per machine.

### 15.12 Decisions

Decided 2026-10-01: all eight items follow the recommendation below, with one addition to item 5. M4.9 as a whole is in the backlog (§18) and is not scheduled.

1. **Day budgets per user.** Recommended for the first cut: 3600 box-seconds and $0.50 of model cost a day.
   - In the worst month that is 30 box-hours ($5.10 list, $1.30–1.92 Spot) plus $15 of tokens.
   - It covers about 140 text runs, or 10 GUI runs, a day.
   - A free tier would need about 600 s and $0.05 a day, which is 3 text runs with headroom.
2. **Keep 48 runs a day and the 1 h floor.** Recommended. They bound frequency and NVMe writes, and the budgets bound money.
3. **Exact time or a flex window for cron.** Recommended: cron runs within 5 min of their time, spread by a hash of the box id, so that wakes and hibernates do not pile up at :00. `at` one-shots keep the exact time. This costs nothing.
4. **Save before re-hibernate.** Recommended: a live save (1–3 s) after any run that changed `/workspace` or `agent.db`, and none after a precheck-only run. A plain hibernate does not save today, so without this a node loss loses the run's output.
5. **Linger after a run.** Recommended: none; hibernate as soon as the Tick ends. A 60 s linger, in case the user opens the app, would almost double a text run's machine cost. Addition: never hibernate while a client is attached (the edge Presence counter, 15.9b); the user's own session keeps the box up.
6. **Idempotency for `act`.** Recommended: settle it before `act` ships. Each run gets an idempotency key that write tools and M5 connectors receive, so a run repeated after a restore cannot file the same receipt twice. `observe` does not need it.
7. **What counts against the budget on a running box.** Recommended: only tokens, not box-seconds, since the user's session pays for the box being up.
8. **Zero-page skip (§18).** It now has a second reason: the 24 s re-hibernate is most of a short run's machine cost and all of its NVMe write. Recommended: revisit with the 9f numbers from real wakes.

### 15.13 As built: scheduled wake (option B, 2026-10-10)

A user asked for a routine at 09:00 while the Mac is closed. This is the alarm of 15.2 cut down to "wake, stay up for a while, sleep again": the api owns the time and the box owns the content, a systemd user timer that runs the routine (contracts.md, Scheduled wake). No `Routines` service, no Tick, no lease and no notice exist.

- **Contract:** `Boxes.SetWakeSchedule{box_id, cron, tz, awake_for_s}`, `ListWakeSchedules` and `ClearWakeSchedule{box_id}`, one schedule per box. CLI `cocoonbox box schedule set|list|clear`; Kit `BoxConnection.setWakeSchedule`, `listWakeSchedules` and `clearWakeSchedule`.
- **Where it lives:** the table `wake_schedules` in the users database (migration 0007), not the `<box>/proactive.json` of 9b. The per-user cap needs the owner's row lock that `boxes` already takes, a box delete removes the row by cascade, and the alarm needs no state layer. A row holds `cron`, `tz` and `awake_for_s` and the alarm state `next_at`, `last_wake_at` and `sleep_at`, and no prompt or command.
- **Alarm** (`api/boxes/wake.go`, `RunAlarms` every 15 s, off the edge path): each pass reads every schedule in one `SELECT`, with no heap, since 3 per user bounds the count. From a minute before a time, a box without memory (hibernated, failed, no machine) gets `Ensure` as its owner. The exception is a box this schedule woke less than `wake.floor` ago. A box that is hibernating gets the wake once its hibernate ends, so a client may put the box to sleep just before the time. A box that is up gets nothing, and the time passes. A time missed while the api was down gets one wake at once, with a full `awake_for` from that wake. The pass stores the wake before its `Ensure`, so no failure leaves a woken box that the api will not hibernate. A pass that read a schedule the user replaced meanwhile writes no alarm state over it and wakes nothing for it. The schedules of disabled users are skipped.
- **Back to sleep:** at the time plus `awake_for`, the api runs `Hibernate` when all of these hold:
  - the alarm woke the box (`sleep_at` is set);
  - nothing reached the box since that wake;
  - the box is running with no operation (a save, a Rebase); otherwise the api waits.

  A reach is any call the edge routes to the box once `Resolve` succeeds (agent gRPC, MCP, the screen, a share's viewer; `Resolver.Reached`), or the owner's `Ensure`. It makes the box user-driven, and the box stays up until someone hibernates it. The api's own calls are no reach: provisioning and saves skip the edge, the alarm's `Ensure` carries a context mark, and ending a share's sessions calls `Resolve` without `Reached`.
- **Hot path:** each routed edge call and each `Ensure` takes one mutex and one map write. The claim path is untouched.
- **Restart:** `next_at`, `last_wake_at` and `sleep_at` persist, so a restart keeps the next wake and a pending hibernate. The pass after a reach clears `sleep_at`, so a hand-over survives a restart too. Only a reach in the last 15 s before a restart is lost; the restart cuts every edge stream, and clients that reconnect count again.
- **Cron:** robfig/cron v3, five fields, run on the wall clock of `tz`. Alone, the library skips a spring-forward time and fires a fall-back time twice (2026-03-08 02:30 and 2026-11-01 01:30 in America/New_York). On the wall clock, a skipped time fires once at the end of the gap, and a repeated time fires once at its first occurrence, as 15.10 asks. `time.Date` picks the second occurrence east of UTC (Europe/Berlin, Australia/Sydney), so the code looks for the earlier instant itself; the tests cover New York, Berlin and Sydney. `SetWakeSchedule` refuses a cron whose next 512 times come closer than the floor.
- **Limits** (api config `wake`): `max_schedules` is 3 per user, `floor` 1 h, and `max_awake_for` 1 h. `awake_for` defaults to 15 min and is at least 1 min.
- **Guest clock** (the risk in 15.2): it is correct as soon as the box wakes, so the alarm pushes no time and the §12 identity write stays as it is.
  - Measured on cbdesk:v25, n = 10 wakes after 120 s hibernated. Root exec read the guest clock 1.2–2.3 s after `Ensure`, once the box was `RUNNING`: −13 to +1 ms off the host clock, inside read brackets of ±28 to ±46 ms. 10 s later it was −15 to −6 ms off.
  - The second signal, the MCP shell (through the edge and the agent), read +9 to +50 ms inside ±73 to ±109 ms.
  - The mechanism is in the VMM: the cocoonstack Cloud Hypervisor restores the KVM clock with `KVM_CLOCK_REALTIME` (`hypervisor/src/lib.rs`, `with_realtime_filled`). The guest clock moves on by the host's wall time spent hibernated, so a longer sleep than the 120 s measured gives the same result.
  - Nothing else corrects the clock: timesyncd in the box never syncs (`System clock synchronized: no`), because the box has no NTP egress.
- **A routine that outlives `awake_for`:** there is no box-side keep-awake signal. A file the routine touches would need a new backend read into the guest. It would also let `user`, who has passwordless sudo, keep the box up at our cost, the reason 9b rejects a box-reported presence flag. `awake_for` is the hard cap instead, and the unit's `TimeoutStartSec=` keeps the routine inside it. A routine still running at the hibernate freezes with the box and goes on at the next wake, its connections gone.
- **E2E** (2026-10-10, the bench host): a separate instance of this build, `wakee2e`, ran with its own Postgres, its own port and the real `Boxes`, `edge.Proxy` and `RunAlarms`. A second api on the shared cbsbx instance is not safe: the backend renews every claim's lease and provisions every unprovisioned machine. So its backend drove one box of a throwaway user through the live api's public `Boxes` calls, as any client does. The box was cbdesk:v25, with a `Persistent=true` user timer at four times. `awake_for` was 2 min, and the floor 1 min for the run.
  1. Set 6 min ahead on a hibernated box: woken at T−52 s; the timer fired at T+9.6 s (`AccuracySec` defaults to 1 min); hibernated at T+2 min.
  2. A `Session.Hello` through the instance's edge at T−20 s: the next pass logged "a client reached the box" and stored the hand-over; the box was still `RUNNING` at T+2.5 min.
  3. The instance was restarted at T−2.5 min and again at T+1 min: woken at T−58 s, the timer fired at T+9.6 s, and the restarted instance hibernated the box at T+2 min.
  4. The instance was down from T−2.5 min to T+1 min: it woke the box at once on start; the timer, whose time passed while the box slept, fired as the box came up; hibernated 2 min after that wake.

  The run of the build before the review fixes gave the same four outcomes, except that in phase 4 the box got only the rest of `awake_for` after its late wake.
- **Live acceptance** (needs the live api on this build): it uses two boxes, since the floor is per box: A wakes, its timer fires, and it hibernates; B gets an MCP call through the live edge and stays up.

## 16. M5: Connect

Proposed 2026-10-01, for review. It replaces §4.2(b), whose in-VM vault kept credentials at rest in the VM. The user links an external account (GitHub, Google, Notion, …) once in the app. After that, both kinds of agent can use it:
- the box agent (mode A);
- the user's own local coding agent, through the bridge (mode B, §13).

Local exec on the Mac (§4.2(c)) is not part of this section.

Constraints:
- Credentials (boundaries.md): no OAuth token or API key is at rest in the VM. The VM's disk, its hibernate memory images and its state uploads all count as at rest.
- Every call on an account is authorized against that account's user, whichever door it comes through.
- The box is one trust domain. Whatever the agent can reach, the model's shell can reach too, so the boundary is the broker's policy, not the agent.
- A grant is bound to boxes, never to agents: every agent in a box shares its files, its browser and its network. A grant serves all of the user's boxes by default, or the boxes the user picks. Separate boxes are the way to keep a work account away from a personal one.
- Connect covers API access only. Website sessions in the box's Chrome are separate state: connecting Gmail does not sign the box's browser in to Gmail. Anything in the box can read those sessions (Chrome on Linux encrypts cookies with a fixed key, and the agent has a shell and CDP), so product copy never says that agents cannot read the browser's logins.

References:
- **Meta's Muse**, the consumer agent app (Meta's public pages, not verified here):
  - It keeps OAuth tokens inside the user's VM, in a separate daemon. The agent gets surrogates, and the real token is injected at the network boundary.
  - Writes need approval by default, and approvals are scoped: once, for the task, or always.
  - We keep its approval defaults. We cannot keep its token placement, because a token held in the VM is in every hibernate image and state upload of that box (§12).

### 16.1 Where credentials live: a host-side broker

The boundaries.md order is Mac Keychain first, then host side, then VM memory for one request. How the options compare:

| | Mac Keychain | Host-side broker |
|---|---|---|
| Mode B (local agents) | works | works |
| Mode A while the Mac sleeps (proactive runs, §15) | fails: every refresh and every call needs the Mac online | works |
| Implementations | a second set of connector clients in Swift, next to the host's | one |
| Blast radius | one Mac | the api host; its master key opens every grant |

Decided: host custody, stored in PostgreSQL (16.1.1).
- The broker holds every grant, and the Mac holds only the user token, as today.
- No door reads a grant back: not the app, not the box, not a client.
- The Keychain option loses because unattended box work, the reason the box is in the cloud, would stop whenever the laptop lid closes.
- The cost is the host blast radius. §16.6 bounds it.

The broker is a service-layer package of the api (`api/connect`), not a new component. It holds:
- **Grant store.** One record per (user, connector), with:
  - the refresh token, the access token and its expiry;
  - the scopes and the account label;
  - the per-tool policy.
  
  Records are sealed with AES-256-GCM under a key derived from the user's state key, which the master key seals and the database never sees in the clear (16.1.1). The keyring's plain JSON files are not used.
- **The OAuth client** (16.2) and refresh (16.4).
- **The upstream MCP clients,** one per (user, connector) (16.3).
- **Policy, approvals and audit** (16.5, 16.6).

#### 16.1.1 Storage: PostgreSQL

Connect uses the api's PostgreSQL database, next to the users tables, with its own migrations (`api/connect/migrations`, tracked in `connect_schema_migrations`).

Why PostgreSQL:
- Several api replicas share one store. The OAuth callback can land on a different replica from the one that began the flow, and a refresh must be single-flight across replicas, not only inside one process.
- A row lock gives that single-flight with no extra service.
- Production gets HA, point-in-time recovery and IAM login from Cloud SQL.

Where it runs:
- **Bench host:** PostgreSQL 16 on the host, with its data directory on the root disk, not the local NVMe. The root disk survives an instance stop, and the local NVMe does not (§18, off-node state store). The api connects over the unix socket with peer authentication, so there is no database password.
- **Production:** Cloud SQL for PostgreSQL, on a private IP only. The api logs in with IAM database authentication through Application Default Credentials, so there is no password and no key file.

Encryption (envelope):
- The database holds ciphertext only. A dump, a backup or a read replica opens nothing on its own.
- Each user's grant key is `HKDF-SHA256(state key, "cocoonbox connect grant")`. The state key is the 256-bit per-user key of the state layer, which `users.state_key_sealed` holds under the master key, so Connect adds no keyring and no key table.
- The master key seals the state keys:
  - bench host: a 32-byte file read through `LoadCredential` (root 0600), like the edge signing key;
  - production: a Cloud KMS key, used only through `Encrypt` and `Decrypt` with ADC.
- The api derives the grant key in memory for each use.
- Deleting the user's state key makes every grant of the user unreadable, in the database and in any backup.
- Each sealed column uses AES-256-GCM. Its additional data is `table|row id|user id|connector`, so a row copied onto another user's or another connector's row fails to open.
- Rotating the master key reseals the state keys only.

Tables (migration `0001`; `connect_door_keys` came with the box door; since migration `0004` every `user_id` references `users` `ON DELETE CASCADE`):

| Table | Columns | Notes |
|---|---|---|
| `connect_grants` | `id` PK, `user_id`, `connector`, `kind`, `mcp_url`, `account_label`, `scopes text[]`, `status` (`active` / `needs_reconnect`), `policy jsonb`, `sealed bytea` (access token, refresh token, an API key and its header, a registered client), `token_expires_at`, `created_at`, `updated_at` | unique (`user_id`, `connector`); `policy` and the expiry stay plain, so listing and the refresh check need no decrypt |
| `connect_flows` | `id` PK, `user_id`, `connector`, `kind`, `mcp_url`, `state_sha256`, `session_sha256`, `sealed bytea` (PKCE verifier, the registered client), `expires_at` | 10 min TTL; deleted when the callback takes it |
| `connect_approvals` | `id` PK, `user_id`, `grant_id`, `tool`, `args_sha256`, `summary`, `decision` (`pending`, `once`, `always`, `deny`), `expires_at`, `used_at` | used once, 10 min (16.5) |
| `connect_door_keys` | `key_sha256` PK, `machine_id`, `user_id`, `incarnation`, `created_at` | the box door key (16.3, migration `0003`); the key itself is never stored |
| `connect_audit` | `id` bigserial PK, `at`, `user_id`, `connector`, `tool`, `door`, `caller`, `decision`, `outcome`, `upstream_status`, `latency_ms`, `req_bytes`, `resp_bytes`, `args_sha256` | index (`user_id`, `connector`, `at`); a daily job deletes rows older than 90 days |

How the broker uses it:
- **Refresh** (16.4) runs inside one transaction:
  1. `SELECT … FROM connect_grants WHERE id = $1 FOR UPDATE`;
  2. check the expiry again, since another replica may have refreshed while this one waited;
  3. call the vendor, with a 10 s timeout;
  4. write the new sealed tokens and commit.
  
  A rotated refresh token is therefore stored before the new access token is used. If the commit fails after the vendor rotated, the grant becomes `needs_reconnect`.
- **Box door:** the broker hashes the presented key and reads `connect_door_keys`. Provisioning an incarnation inserts its row and deletes the machine's older rows in the same transaction, before `Deliver` installs the key. Deleting a machine (box delete, user delete, park, Rebase) deletes its row; a hibernated machine keeps it. At start the api deletes the rows of machines the backend no longer holds.
- **Disconnect** and **delete user** remove the rows after the vendor revoke. Deleting a user also deletes the user's state key, which makes any copy of their old rows in a backup unreadable.
- **Migrations:** numbered SQL files embedded in the binary. The api applies them at start under `pg_advisory_lock`, so concurrent replicas run each one once.
- **Driver:** `pgx/v5` with its pool. There is no ORM.

Hot-path cost: none on the claim, screen or edge path. A connector call adds one primary-key read and one audit insert, about 1 ms against a local PostgreSQL, next to an upstream call of 100 ms or more. A refresh adds one row lock, held only for the vendor round trip.

gw's MCP gateway does not fit as the broker:
- its upstream credential is one per server, shared by every key;
- it has no authorization-code flow and no per-user token;
- its entitlements change only through a config publish.

Adding those to gw would put cocoonbox's user policy into infra.

### 16.2 The OAuth flow

The flow runs in the browser, from the console's Connect page (`/cb/connect`), and the code comes back to the api, which does the exchange. Decided 2026-10-05: the console page replaces the app's loopback listener, and the app's Connections… item opens that page.

1. The signed-in page calls `POST /cb/api/connect/{connector}/begin`. The api creates a flow:
   - a random `state`, bound to the user, to the hash of the console session secret and to the hash of a random 32-byte nonce, with a 10 min TTL, used once;
   - a PKCE S256 verifier, kept in the api;
   - the catalog entry's scopes.
   
   It sets the cookie `cb_connect` to the nonce, for `/cocoonbox.connect/` only, and returns the vendor's authorization URL. Its `redirect_uri` is the api's `https://<endpoint>/cocoonbox.connect/callback`.
2. The page opens that URL in the same browser, where the user's accounts are already signed in.
3. The vendor redirects to the callback. The api completes the flow only when the hash of the request's `cb_connect` cookie equals the flow's nonce hash, the `state` is unused and unexpired, and the console session that began the flow is still live for the flow's user.
4. The api exchanges the code, using its client credentials and the verifier, and stores the grant. It redirects to `/cb/connect?connected=<connector>`, which shows the account as connected.

The cookies:
- The session cookie `cb_session` is `SameSite=Strict` with `Path=/cb/`, so it never reaches the callback: the vendor's redirect is a cross-site navigation to another path.
- `cb_connect` carries the flow's nonce for 10 min, `SameSite=Lax`, which a top-level GET navigation sends. The callback clears it. The session secret never leaves `/cb/`.

Why the flow is bound to the console session:
- **Hijack.** With an https redirect alone, an attacker can start a flow under their own account and send the link to a victim. The victim's consent would then attach the victim's account to the attacker's cocoonbox (OAuth login CSRF). Here, the victim's browser never holds the nonce of the attacker's flow, so the callback refuses.
- **Against the loopback listener:** both let only the browser that started a flow complete it. The page needs no app release for a UI change and half the code, and a user who has only the CLI or a local agent can connect too.
- **The https callback is still needed:**
  - Slack rejects http redirects.
  - Refreshing a GitHub App token needs its `client_secret`, so the exchange must happen where the secret is.

OAuth clients:
- **Pre-registered clients, owned by us:**
  - a GitHub App: user-to-server tokens last 8 h, and the refresh token lasts 6 months and rotates on use. The user picks the repositories when installing the App.
  - a Google web client.
- **MCP authorization (2025-11-25)** for vendor MCP servers:
  - The broker discovers the protected resource and its authorization server.
  - It registers with a Client ID Metadata Document at `https://<endpoint>/cocoonbox.connect/client.json` where the server supports one, and by dynamic registration otherwise.
  - It always sends PKCE S256 and the `resource` parameter.
  
  This is what brings Notion and Linear without any per-vendor code.
- **API keys** (a custom MCP server):
  - The page sends the key and its header name with `begin`, over TLS.
  - The api lists the server's tools with it, then stores it sealed.
  - The key never enters a transcript, a config file or the Keychain.

### 16.3 How a call reaches the account

Connector tools are MCP tools, served by the broker.
- **Vendors with an MCP server:** the broker is an MCP client of that server with the user's token, and serves its tools on. These are:
  - GitHub `api.githubcopilot.com/mcp/`;
  - Notion `mcp.notion.com/mcp`;
  - Linear `mcp.linear.app/mcp`;
  - Google Workspace `*mcp.googleapis.com` (developer preview).
- **No per-vendor code.** The broker is a generic MCP client. A catalog entry holds only the MCP URL and, for GitHub and Google, our OAuth client and scopes. A vendor without a hosted MCP server gets no adapter; it waits for per-claim credential rules (below).
- **Tool names** are `<connector>_<tool>`, within `^[a-zA-Z0-9_-]{1,64}$`. The broker keeps the upstream's `readOnlyHint` and `destructiveHint` annotations.

Doors:

| Door | Caller | Auth | Serves |
|---|---|---|---|
| `Connectors` gRPC (`proto/cocoonbox/control/v1/connectors.proto`) | the app | user token | catalog, connect, policy, approvals, activity, disconnect |
| `https://<endpoint>/cocoonbox.mcp/connect` | the bridge (mode B) | user token | the user's connector tools |
| `http://10.203.0.1:18091/mcp`, host-internal | the box (mode A) | a per-machine key that the box never holds | the box owner's connector tools |

The box door follows §12.2 (the proof comes from the channel) and the gw pattern of §14.5:
- **Key.** When the api provisions an incarnation, it issues a random door key and installs it with the gw key in the same `Deliver` call: a `guest: false` entry `CB_CONNECT_KEY = Bearer <key>`. The broker keeps the key's SHA-256 with the machine and its owner (`connect_door_keys`); the next incarnation's key replaces it. A failure to issue a key is logged and does not stop the box from starting; its connector calls then get 401.
- **Node config (v24 pools and later):**
  - the pool rule `{host 10.203.0.1, ports [18091], secret CB_CONNECT_KEY}`, ahead of `{host *, intercept inject}`, because a plain request takes the first rule that matches;
  - the secret `{name CB_CONNECT_KEY, header Authorization}` with no `value_env`, so only the claim's entry feeds it, and a claim without one fails closed.
  
  `egress_internal_allow` becomes `10.203.0.1/32:18090,18091,9000`, and the host firewall (`cb-host-net`, CB-OUTPUT) lets the proxy reach 18091. The api listens on `connect.box_listen` (`10.203.0.1:18091`); CB-INPUT drops the port on the uplink. The proxy overwrites `Authorization` on every request to the door. Older pools reach the port through their `*` rule, but without a key, so they get 401.
- **Requests answer `application/json`.** The agent's `GET` event stream, which carries `list_changed`, streams through the forward proxy: since sandboxd b778c66 the proxy passes an event stream through as each chunk arrives.
- **Mode A connectors ride the sandboxd egress door (M4.8).** A per-box key in `agent.env` would be a credential at rest in the VM, one that opens the user's accounts. `agent.env` carries only the door's URL, `CONNECT_URL`, which is the same for every box.

**Egress injection, for programs that don't speak MCP (M5.2, in the first cut).** Some work needs the credential on the wire of an ordinary program, above all `git` and `gh` against GitHub. For that case only:
- The broker also puts a short-lived token into the claim's `guest: false` env.
- Two sandboxd pool rules with `intercept: true` in the node config overwrite `Authorization`:
  - on `github.com` with Basic `x-access-token:<token>`;
  - on `api.github.com` with Bearer.
- This works on today's sandboxd, with no new primitive.

How it works:
- The token is the GitHub App user token (8 h), limited to the repositories the user selected.
- The value is literal (`sandboxd/egress/proxy.go:378-389`), so the broker precomputes `Basic base64(x-access-token:<token>)`.
- The api pushes the claim's entries with `PATCH /v1/sandboxes/{id}/env`:
  - on connect, disconnect and a lost grant, to every machine of the user;
  - every minute, to running machines, refreshing a token with less than 5 min left;
  - at a wake, in parallel with the wake call, without waiting for it;
  - when each incarnation is provisioned (claim, wake, Rebase target). This push is the authoritative one.
  
  sandboxd accepts a `guest: false` change even on a paused or hibernated claim.
- A request between a wake and that incarnation's provisioning can carry the token the box was hibernated with, when the hibernation outlasted it.
- The service layer reaches sandboxd only through `machines.SetCredentials(id, Credentials{Secrets, Flags})`. The sandboxd backend writes only entries that changed. A pool that refuses them (400, a pool that does not intercept) is tried again only when the values change.
- A claim holding a GitHub or Google grant has github.com and api.github.com, or www.googleapis.com, intercepted for all its traffic, Chrome's included, over HTTP/1.1. gRPC to those hosts breaks for that claim only.

Limits, from sandboxd's code:
- Inside an intercepted tunnel, only HTTP/1.1 is served (`sandboxd/egress/intercept.go:32`).
- Only programs that trust `/etc/ssl/certs` accept the proxy's certificate: git, curl and gh do.
- Per-host rules come only from the node config.
- Consent is per host, not per action.
- The audit is sandboxd's egress telemetry: host, method and secret name.

Every other connector stays broker-only.

**Per-claim credential rules (available since sandboxd e0e27fc, cocoonstack/sandbox#302).** Today every injected host and secret is node config, so each new vendor would be a config change on every node, which puts cocoonbox's catalog into infra. The upstream primitive is a per-claim credential list (`{hosts, header, value}`), set through the API, validated as a subset of the pool's intercepted hosts, persisted with the claim, redacted on read and never delivered to the guest (connect-research.md §4). With it, a vendor without MCP becomes a catalog entry, and its logic runs in the box as a CLI, SDK or script. Such grants get vendor-scope approval only, and the app says so at connect time. Filed as cocoonstack/sandbox#301 (2026-10-03).

With #302, M5.2 uses claim env entries with `inject` instead of pool `secret` rules: the node config keeps only `egress_ca` and the `intercept: true` rules for the GitHub hosts, and the api sets and rotates the values per claim (rotation works on a hibernated claim). `egress_ca` loads only at a sandboxd start and only when a pool intercepts. `cbsbx-conf` writes it whenever `/etc/cocoonbox/egress-ca/root.crt` exists, so an image cut keeps it. Placeholder gating (sandbox #307, 858a80e) keeps the injection off the box Chrome's github.com traffic. The recipe:

- `GH_GIT`: value `Basic <base64(x-access-token:<token>)>`, `inject {hosts: [github.com], header: Authorization, placeholder: "Basic @GH_GIT@"}`. Only git requests carry the placeholder that the proxy replaces.
- The placeholder header is switched on at run time, not baked into the image. Without a matching inject rule, git would send `Authorization: Basic @GH_GIT@` to github.com itself, and GitHub answers 400 even for a public repository. The switch:
  - The api sets a guest-visible, non-secret flag `COCOONBOX_GH_GIT = 1` (`guest: true`) in the same claim env as `GH_GIT`, and removes both on disconnect.
  - sandboxd writes guest entries to `/run/silkd.env` (root 0600). The path unit `cocoonbox-github-git.path` follows that file, and its service writes `/run/cocoonbox-github.gitconfig` with the `extraHeader` while the flag is set, and removes the file otherwise. The service also runs at boot.
  - `/etc/gitconfig` includes that file, and git ignores an include that does not exist.
  - sandboxd refuses a `guest: true` change on a paused or hibernated claim (409 `sandbox is paused`). The api therefore sets the flag on a running claim: after the wake in `Ensure`, or at Connect while the box runs. The `/run` file is in the box's memory, so it survives a hibernate.
- `GH_API`: `gh` sends `Authorization: token <GH_TOKEN>`, so the box sets `GH_TOKEN=@GH_API@` and the claim entry is value `token <token>`, `inject {hosts: [api.github.com], header: Authorization, placeholder: "token @GH_API@"}`; the browser's api.github.com calls pass untouched. From v24 the image carries `gh` and the wrapper `/usr/local/bin/gh`, which sets `GH_TOKEN=@GH_API@` only while the same switch is on (`/run/cocoonbox-github.gitconfig` exists) and the caller set no `GH_TOKEN` of its own.
- `GOOGLE_CALENDAR`: value `Bearer <token>`, `inject {hosts: [www.googleapis.com], header: Authorization, placeholder: "Bearer @GOOGLE_CALENDAR@"}`. The units that carry the proxy drop-in also set `GOOGLE_OAUTH_ACCESS_TOKEN=@GOOGLE_CALENDAR@`. It needs no switch: without a grant, Google answers the placeholder with 401, and the placeholder gate keeps Chrome's googleapis traffic untouched.
- Chrome still needs `egress_ca` in NSS, because the github.com intercept rule terminates all of the box's github.com TLS. sandboxd writes the root to `/usr/local/share/ca-certificates/sandbox-egress.crt` and appends it to `/etc/ssl/certs/ca-certificates.crt`, on a golden or cold boot of a pool that intercepts. Chrome on Linux reads `~/.pki/nssdb` instead. `egress-ca-nss` imports the root there as `sandbox egress`, trusted `C,,`, and deletes an import whose certificate differs or whose file is gone:
  - `cocoonbox-egress-ca.path` runs it when sandboxd writes the file, which can happen after the session has started;
  - `cocoonbox-session` runs it before each start, which covers a state restore that brings back an older `~/.pki`.
  
  It costs 9 ms per session start when the import matches, and 25–50 ms when it imports.
- Language runtimes do not all read the system bundle: pip's `requests` uses certifi, and Node uses its bundled roots and ignores the proxy env. `export-rootfs.sh` sets `REQUESTS_CA_BUNDLE`, `SSL_CERT_FILE` and `NODE_EXTRA_CA_CERTS` to `/etc/ssl/certs/ca-certificates.crt`, plus `NODE_USE_ENV_PROXY=1`, on every unit that carries the proxy drop-in. silkd clears the env of its execs, so a silkd exec does not get them. The bundle always exists, so a pool that does not intercept gets no warning.

Accepted on cbsbx 2026-10-03: #302 (9 steps) and the placeholder gate with real git 2.43.0. The NSS import and the git switch were accepted the same day on a throwaway claim of a test pool that intercepts postman-echo.com and github.com, with the branch's image files installed by hand: Chrome loaded the intercepted host with the egress intermediate as issuer, and failed with `ERR_CERT_AUTHORITY_INVALID` before the import; git sent no `Authorization` to github.com without the flag, and the placeholder with it.

### 16.4 Refresh, rotation, revocation

- **Refresh** is lazy:
  - It happens on use, when less than 5 min of the access token remain.
  - It is single-flight per grant. GitHub, Slack and Notion rotate the refresh token on every use, so a second concurrent refresh would kill the grant.
  - The new refresh token is stored before the new access token is used.
  
  Egress-injected grants (M5.2) also refresh on a timer, but only while their box runs.
- **A lost grant.** On `invalid_grant`, or on a vendor 401 after a refresh, the grant becomes `needs_reconnect`. Its tools then answer "Reconnect GitHub in the CocoonBox app", and the app shows a badge.
- **Disconnect** in the app makes the broker:
  1. revoke the grant at the vendor: Google `oauth2/revoke`, GitHub `DELETE /applications/{client_id}/grant`, Slack `auth.revoke` (Notion documents no revoke call);
  2. delete the record;
  3. clear any injected env;
  4. send `tools/list_changed` on the open sessions;
  5. write an audit record.
  
  It does not sign the box's browser out of the vendor's website; that session is the box's own state.
- **Revocation at the vendor** shows up at the next refresh.
- **Deleting a user** deletes and revokes all of their grants.
- **Accounts per connector.** The first cut allows one. Several accounts, such as work and personal Gmail, need an `account` argument on every tool, so they come later.

### 16.5 Scopes, consent and approvals

- **Scopes.** Connect asks for the read scopes by default, and "Read and write" asks for more. Changing it later runs consent again with only the added scopes (Google's `include_granted_scopes`).
- **Per-tool policy.** Each tool is `allow`, `ask` or `off`.
  - By default, tools with `readOnlyHint` are `allow` and all others are `ask`. This is Muse's default: writes need approval.
  - The user changes the policy per tool in the app.
- **Mode A.** The broker enforces `ask`, because the box is not trusted:
  1. The call returns an approval-required error. It carries an `approval_id` and a summary that the broker renders from the actual arguments ("Create issue 'x' in loomhand/cocoonbox"), never from model text.
  2. The agent records `ApprovalRequest{kind: "connector"}` with that summary and waits, as it does for `take_over`.
  3. The app shows the request with the broker's summary and three choices: Allow once, Always allow this tool, or Deny. Each choice calls `Connectors.Approve{approval_id, decision: once | always | deny}` on the api with the user token, then `Agents.Respond` to resume the turn. When the broker no longer holds the approval (it expired, or the console answered it), the app still resumes the turn with the user's answer and says so in one line; the broker decides the retry. `always` sets the tool's policy to `allow`. Every answer, the console's too, also reaches the waiting agent from the broker as `notifications/cocoonbox/approval_answered` on its box door stream, so a console answer resumes the turn without the app.
  4. The agent retries the call with the `approval_id` in the call's `_meta`. The broker accepts it once, only for the same arguments (compared by hash), within 10 min.
  
  The box cannot forge an approval, because only the user token can create one.
- **Proactive runs** use the box door like any mode A call, so an unattended run needs no Mac. For the same reason, they have connectors only on sandboxd (M4.8). §15.5 applies on top of this:
  - A run sees connector tools only if its routine's grant names the connector. The grant check lives in the box's dispatcher, which knows the routine. The api knows a Tick's routine only as an opaque id.
  - The broker does not learn the grant. It checks only that the call comes through that box's door, and so is one of that box's calls.
  - The broker still asks before every write, whatever the box claims about the caller. An in-box grant can therefore narrow what a routine may use, but never skip an approval.
  - An approval that nobody answers ends the run as `needs_user`.
  - An unattended write therefore needs a standing approval: the tool's policy is `allow`, set in the app or through Always allow. That scope is the only knob, and proactive runs get no bypass of their own. `allow` covers every caller of the user's connector (mode A and mode B alike), and the routine's grant narrows which runs see the tool. Example: "every morning, file my receipts" finishes unattended only after the user sets the filing tool to `allow`.
- **Mode B.** The caller already holds the user token: the CLI reads it from the Keychain, and the local agent can run the CLI. An approval gate would add nothing, so:
  - `ask` tools are served with `destructiveHint: true`;
  - the client's own permission prompt is the gate;
  - the trust boundary in mode B is the Mac.
- **Prompt injection.** A tool result can carry it, for example in an email body. The defenses:
  - write tools ask by default;
  - the text the user approves comes from the broker, not from the model.
- **Uncertain writes.** A write that times out or gets a 5xx may still have happened. The broker answers `outcome_unknown` and never retries a write itself. The agent checks the result with a read tool before it calls the write again, so one approval never sends two emails.

### 16.6 Audit and blast radius

- **Audit record.** The broker writes one per call:
  - time, user, connector and tool;
  - door and caller (`box:<agent id>` or `mcp:<client name>`);
  - the policy decision: `allow`, `approved`, `denied` or `off`;
  - upstream status, latency and sizes;
  - a hash of the arguments. Never the arguments or the results themselves.
  
  Records are kept 90 days. `Connectors.Activity` shows them per connector in the app.
- **Host blast radius.** A compromised api exposes every grant. It is bounded by:
  - minimal scopes, and read-only by default;
  - GitHub's repository selection;
  - refresh tokens that never leave the broker process;
  - the master key, held only in the api's memory or in Cloud KMS;
  - a database that holds ciphertext only (16.1.1), so a leaked dump or backup opens nothing without the master key;
  - the audit.

### 16.7 Connectors in the first cut

| Connector | Auth | Why | Gate |
|---|---|---|---|
| GitHub | our GitHub App | the most asked-for by coding work; no vendor review | none |
| Google Calendar | our Google client | Calendar scopes are sensitive, not restricted (to confirm at registration) | Google brand verification |
| Notion, Linear | the vendor's MCP server, with client metadata or dynamic registration | no per-vendor code | none |
| Custom MCP server (URL, plus OAuth or a key) | MCP authorization | covers the long tail | – |
| Gmail, Drive (read) | our Google client | high value | see below |
| Slack | – | not in the first cut: no adapter | waits for a directory listing (Slack's MCP server serves only listed apps) or for per-claim credential rules (16.3) |

Gmail and Drive:
- `gmail.readonly` and `drive.readonly` are restricted scopes.
- Until the app is verified, it is capped at 100 users and shows a warning screen.
- General use needs Google's verification plus a CASA assessment, renewed every year.
- `drive.file` is not restricted.

Google's Workspace MCP servers are in developer preview, and our account is not enrolled (2026-10-05). Google Calendar is therefore a grant-only connector: Connect, refresh, revoke and Activity work, it serves no tools, and programs in the box will use it through per-claim credential rules (16.3, an `inject` entry for `www.googleapis.com` with `Authorization: Bearer <token>`). The broker gets no adapter.

Connector events for §15.2's triggers (GitHub webhooks, Gmail push) arrive at the broker. They are not in the first cut.

### 16.8 Work split, order and acceptance

Split:
- **proto / contracts.md:**
  - `control/v1/connectors.proto`;
  - `ApprovalRequest.kind = "connector"`, with an `approval_id` that the api issued;
  - a "Connect MCP" section in contracts.md: both MCP doors, tool naming, the approval-required error, and the key rule of the box door.
- **api:**
  - `api/connect`: the PostgreSQL store with its migrations and envelope encryption (16.1.1), the OAuth and MCP-authorization client, the upstream clients, refresh, policy, approvals and audit;
  - the callback and client-metadata routes;
  - the `Connectors` service and `/cocoonbox.mcp/connect`;
  - the listener for the box door;
  - `CB_CONNECT` in the sandboxd `Deliver`;
  - M5.2: `machines.SetSecret`, and the token push on refresh and before wake.
- **agent:**
  - an MCP client for one upstream, the box door, with its URL from `agent.env`, reached through the proxy;
  - connector tools join the dispatcher's list for the loop only. They are not added to the box's `/mcp`, because mode B gets them from the api;
  - the path from approval-required to `ApprovalRequest` and the retry.
- **console:** `/cb/connect`: the catalog, Connect, per-tool policy, activity, pending approvals, disconnect and reconnect.
- **macos:**
  - a Connections… item that opens `/cb/connect` in the default browser;
  - the reconnect marker from `Connectors.List`, read when the app connects and each time it becomes active. The item then names the accounts to reconnect, and a Reconnect toolbar button opens the same page;
  - connector approvals in the transcript;
  - the bridge merges the tools of `/cocoonbox.mcp/connect` and passes their `list_changed` on.
- **image:** `CONNECT_URL` in the static `agent.env`.
- **infra:**
  - the sandboxd pool rule and secret for the box door;
  - PostgreSQL 16 on the bench host (root disk, unix socket, peer authentication) and its backup timer; Cloud SQL with IAM authentication in production;
  - the master key: a `LoadCredential` file on the bench host, a Cloud KMS key in production;
  - registering the GitHub App and the Google client, with redirect `https://<endpoint>/cocoonbox.connect/callback`;
  - M5.2: `egress_ca` and the intercept rules for GitHub.

Order:
1. proto and contracts.
2. api and macos. This gives mode B end to end on the current backend.
3. agent: mode A, on sandboxd.
4. M5.2, the last step of the first cut.

Hot-path cost: none on the claim, screen or edge path. A connector call adds one host hop, and the bridge opens one more MCP session at start. M5.2 adds one host-local sandboxd env call before the wake of a box with a GitHub grant, plus a vendor refresh only when the token is about to expire (16.4).

Acceptance, on the bench host through the public edge:
1. **Connect** GitHub and Google Calendar from the console's Connect page.
   - The consent page opens in the same browser, and the page shows the account as connected.
   - `Connectors.List` output has no token field.
   - Negative: a callback without the console session that started the flow (no cookie, another user's session, a replayed or expired `state`) is refused.
2. **Mode B, box hibernated.** A local agent CLI, with the app closed, lists this week's Calendar events and the user's GitHub issues. `cocoonbox status` still says hibernated.
3. **Mode A, on sandboxd.**
   - The box agent lists issues. This is a read, and nothing asks.
   - Then it creates an issue. The app asks, with the broker's summary. Approve → the issue is created.
   - A retry of the same approval with a changed title is refused. Deny → refused.
   - Activity shows all four calls.
4. **No credential in the VM.** The acceptance script decrypts the grants on the host (root, with the master key) into a pipe. It then searches for the access and refresh tokens in:
   - the guest's `/proc/*/environ`, `/etc`, `/run`, `/home`, `/workspace` and `agent.db`;
   - the box's hibernate memory file on the host;
   - the Mac's client configs and shell history;
   - a `pg_dump` of the database.
   
   No match. A positive control matches.
5. **Refresh.** With the cached expiry set in the past, two concurrent GitHub calls, sent to two api processes on the same database, cause one refresh, and the grant still works afterwards.
6. **Disconnect.**
   - GitHub reports the old token revoked (401).
   - The tools disappear in both modes (`list_changed`).
   - Activity records the disconnect.
7. **M5.2.**
   - In the box, `git clone` and `git push` of a private repository in the selection work, with no credential helper and no token in the guest (searched as in 4).
   - GitHub refuses a repository outside the selection.
   - After the box was hibernated for longer than the token's 8 h, the first `git fetch` after the wake works.

Decided: host custody of every grant, with no Mac-only option, stored in PostgreSQL (16.1, 16.1.1).

Decided:
1. Gmail and Drive read ship in the first cut as an unverified Google app (at most 100 users, with the warning screen). Verification and CASA come before general use.
2. In mode B, `ask` is the client's own permission prompt (16.5).

Decided 2026-10-03, from connect-research.md §5:
1. The broker is a generic MCP client of vendor-hosted MCP servers, with no per-vendor adapters (16.3).
2. GitHub egress injection (M5.2) is in the first cut, on today's sandboxd (16.3).
3. Per-claim credential rules are a later upstream sandboxd primitive, an open item to file as a cocoonstack/sandbox issue (16.3).

## 17. M6: Market

Proposed 2026-10-01; in the backlog since 2026-10-01 until its two open questions are decided (§18). It replaces §4.3. Users install capabilities (skills, MCP servers, agent presets, desktop apps) for the box and for their local agents, from one catalog.

What goes where:

| Capability | Box agent (mode A) | Local agents (mode B) |
|---|---|---|
| skill (`skills/*/SKILL.md`), command, agent preset (`agents/*.md`) | loaded by the box agent from the box's user state | installed by the client's own plugin manager from the same catalog |
| stdio MCP server | runs in the box as `user`. Its tools join the dispatcher, so the box's `/mcp` serves them too | uses the box-hosted tools through the bridge, or installs the server locally through its own plugin manager |
| remote MCP server that needs OAuth or a key | a connector (§16): the broker is its client, and nothing lands in the box | the same connector, through `/cocoonbox.mcp/connect` |
| desktop app | installed per user in the box, with Flatpak | – |

### 17.1 Package: the M4.7 plugin format

A plugin uses the agent-plugins layout that the three clients share (§13.5):
- `.claude-plugin/plugin.json`, plus `.codex-plugin/` and the `cursor-agent` manifest;
- `skills/`, `commands/`, `agents/`, `hooks/`;
- `.mcp.json`.

Existing public plugins install unchanged. What only the box needs goes into one extra manifest, `.cocoonbox-plugin/plugin.json`, which the clients ignore:

```json
{ "requires": { "connectors": ["github"], "egress": [{"host": "api.example.com", "ports": [443]}], "apps": [{"flatpak": "org.gimp.GIMP"}] } }
```

A plugin without that manifest counts as `requires: {}`.

Variables:
- **Non-secret variables** (from the JSON Schema `variables`) are asked for at install and put into the plugin's env.
- **Secret variables for a stdio server in the box are refused at install.** A variable is secret if it is `writeOnly`, has `format: password`, or has TOKEN, KEY, SECRET or PASSWORD in its name.
  - Such a value would sit in the box's process env, and from there in its hibernate images.
  - The refusal says so, and offers two other routes: use the plugin from a local agent, or add it as a connector if its server is remote.
- **Later:** a reviewed plugin whose key goes as a header to a fixed host could use the egress injection of §16.3. That needs a node-config rule per plugin, so it is not in the first cut.

### 17.2 Catalog, integrity, trust

- **Catalog.** One public git repository, `loomhand/cocoonbox-plugins` (open item below).
  - It carries the M4.7 marketplace files, `.claude-plugin/marketplace.json` and `.agents/plugins/marketplace.json`. So `claude` and `codex` use it as a marketplace unchanged, and `cursor-agent` can too once the repository is public.
  - Each entry pins a commit SHA and the sha256 of the plugin's tarball.
  - The repository's history is the review log.
- **Review.** Adding a plugin is a pull request to the catalog. The review reads:
  - the manifests;
  - the commands in `.mcp.json`;
  - the hooks;
  - the apps the plugin asks for.
  
  A reviewed entry carries `reviewed: true`. User-added catalogs come later, and install with "not reviewed" shown.
- **Integrity checks,** done by the agent:
  - fetch the tarball at the pinned SHA;
  - check its sha256 against the index;
  - refuse symlinks, path traversal, files over 10 MB, and archives over 500 MB or 50,000 files.
  
  There are no floating refs: an update is a new pinned entry. There are no signatures in the first cut. The pinned index, fetched over TLS, is the root of trust.
- **Permissions shown at install,** taken from the manifests:
  - "instructions only": skills, commands and presets, which contain no code;
  - "runs code in your box": stdio MCP servers, shown with their command lines;
  - "uses your accounts": connectors;
  - "installs apps";
  - "network": the declared egress hosts.
- **What is enforced:**
  - Plugin code runs as `user` under the toolhost, never as `cbagent` or root (§3).
  - Connector use goes through the broker's policy (§16.5), whatever the plugin says.
  - Apps are Flatpak `--user` installs from Flathub, inside Flatpak's sandbox.
- **What is only shown: declared egress.** The box's egress policy is `*` (§14.6), and sandboxd rules come only from the node config. Enforcing egress per plugin needs a per-claim egress policy upstream (a mechanism, §14.7), and waits for it.
- **Hooks do not run in the box in the first cut.** They run on every agent event, and they are the hardest part to review. A plugin with hooks installs with them turned off, and says so.
- **What a plugin can reach.** A plugin is code that the user chose to run in their own box. The box is one trust domain (§16), so a plugin can do whatever the model's shell can. Its reach is bounded by that box's own credentials (§12, §16.3), never another user's.

### 17.3 Where installs land

- **The install record** is a row in a new `plugins` table in `agent.db`: plugin, catalog, SHA, variables, enabled, installed_at.
- **Files** unpack to `/home/user/.local/share/cocoonbox/plugins/<name>/<sha>`.
  - That path is user state (§11), so it survives hibernate, node loss and Rebase.
  - Installs never go into the image, because every box shares the golden.
- **Flatpak apps** install with `flatpak --user` into `~/.local/share/flatpak`.
  - The state layer excludes that directory. A runtime is about 1 GB, and per-user encryption rules out dedupe across users.
  - After a Restore, the agent reinstalls the recorded apps in the background, and the app shows "reinstalling 2 apps" until it is done.
- **Nothing lands on the Mac** from the box's installs.
  - Local agents install through their own plugin managers. For each client, the app shows the command, for example `claude plugin marketplace add loomhand/cocoonbox-plugins`.
  - `cocoonbox setup` can run that command.

### 17.4 Runtime in the box

- **Skills.** Each skill's name and description go into the main agent's system prompt. A loop-only tool, `load_skill(name)`, returns the body (the `claude` CLI's pattern).
- **Commands** become prompt templates in the app's composer.
- **Agent presets** (`agents/*.md`: prompt, tool allow-list, model role) are offered in the new-agent sheet and passed as `Agents.Create{preset}`.
- **stdio MCP servers:**
  - **Start.** The toolhost spawns each one as `user`, with the plugin's env plus the proxy variables and the plugin-root variable expanded. It relays the JSON-RPC over the toolhost socket.
  - **Lifetime:**
    - it starts on the first `tools/list` of a turn or an MCP session that needs it;
    - it stops after 10 min idle;
    - it restarts with backoff after a crash.
    
    Per-VM memory is density: one Node server takes 50–100 MB.
  - **Packages.** `npx` and `uvx` servers fetch their packages at first start, through the proxy, and cache them in the user's home. The catalog requires pinned package versions.
- **Tool names** are `<plugin>_<tool>`. A clash with a built-in tool or another plugin's tool refuses the install.
- **The tool list becomes dynamic.** The agent's MCP transport declares `listChanged: true`. It sends `notifications/tools/list_changed` on install, uninstall, enable and disable. The bridge already refreshes its cache on that notification (§13.2).

### 17.5 Update, disable, uninstall

- **Update** is explicit, never automatic. When the index pins a newer SHA, the app shows "update available". Before updating, it shows what changes in the permissions.
- **Disable** keeps the files and the record, stops the plugin's servers and drops its tools.
- **Uninstall:**
  - stops the plugin's servers;
  - removes the files and the record;
  - removes its apps with `flatpak uninstall --user`;
  - sends `list_changed`.
- **Scope.** An install covers the whole box, all agents in it. A preset's tool list limits what one agent sees.

### 17.6 Interface

`cocoonbox.agent.v1.Plugins` is served by the agent. The edge routes it like `Agents`.

```proto
service Plugins {
  rpc Install(InstallRequest) returns (stream InstallProgress);   // catalog URL, plugin, SHA, non-secret variables
  rpc Update(InstallRequest) returns (stream InstallProgress);
  rpc SetEnabled(SetEnabledRequest) returns (InstalledPlugin);
  rpc Uninstall(PluginRef) returns (Empty);
  rpc ListInstalled(Empty) returns (ListInstalledResponse);
}
```

- **Install.** The agent reads the catalog index at the given URL, checks that the entry pins the requested SHA, then fetches and verifies the tarball (17.2). Progress covers fetch, unpack and apps.
- **Browsing.** The app reads the catalog index itself; the index is a published file, like a proto. So browsing never wakes the box, while installing does.

### 17.7 First cut and later

M6.1, the first cut:
- **catalog:** the repository, with 3–5 reviewed plugins made of skills and secret-free stdio servers;
- **agent:**
  - the loader and the `Plugins` service;
  - skills through `load_skill`;
  - stdio servers through the toolhost;
  - the dynamic tool list;
- **app:** a Market pane to browse, show permissions, install with progress, disable, uninstall and update;
- **local agents:** the catalog serves as their marketplace.

M6.2:
- agent presets;
- Flatpak apps, reinstalled after a Restore;
- user-added catalogs;
- secret variables through egress injection, for reviewed plugins.

### 17.8 Work split and acceptance

Split:
- **agent:**
  - the install path: fetch, verify, unpack, record;
  - the loader;
  - the toolhost stdio relay;
  - `load_skill` and `listChanged`;
  - `Plugins`.
- **macos:**
  - the Market pane and the install permission sheet;
  - commands in the composer;
  - the per-client install commands.
- **api:** the edge routes `cocoonbox.agent.v1.Plugins` like `Agents`. Nothing else changes; remote plugins that need auth go through §16.
- **image:** none in M6.1. M6.2 adds `flatpak` and the Flathub remote, and excludes `~/.local/share/flatpak` from the state layer.
- **catalog** (no code): the repository, its marketplace files and the review checklist.
- **proto / contracts.md:**
  - `agent/v1/plugins.proto`;
  - the catalog index format and `.cocoonbox-plugin/plugin.json`;
  - in "Remote tools over MCP": tool naming for plugin tools, and `listChanged` on the box MCP.

Hot-path cost: none on the claim or screen path.
- A turn's first `tools/list` starts at most the servers that the turn lists.
- Idle servers stop.
- Each running server costs its memory in the box.

Acceptance, on the bench host:
1. **Install in the box.** An existing public agent-CLI plugin, made of a skill and a secret-free stdio MCP server, installs unchanged from the catalog.
   - The permission sheet shows the server's command line.
   - In a box-agent turn, the skill loads and the MCP tool runs.
2. **Mode B.** With the plugin installed, a local agent CLI (app closed, through the bridge) sees the plugin's tool after `list_changed`, and runs it in the box.
3. **Local install.** The same plugin installs locally in `claude` and in `codex`, from the catalog used as a marketplace.
4. **Refusals.** Each of these is refused with a message that says why:
   - a tampered tarball (sha256 mismatch);
   - an archive with a symlink;
   - a plugin with a secret variable for a stdio server in the box.
5. **Disable and uninstall.**
   - Disable → the tool is gone in both modes.
   - Uninstall → the files, the record and the process are gone.
   - An idle server stops after 10 min, as the process list shows.
6. **Rebase.** After a Rebase, the plugin is still installed and works.
7. **Memory.** Box memory with the plugin installed and idle is back at the baseline, because the server stopped. Measure how long the first call takes, including the server start.

Open (deferred to the backlog):
1. Whether the catalog repository is public: `loomhand/cocoonbox-plugins`. It must be public for `cursor-agent`.
2. No hooks in the box in the first cut.

## 18. Backlog

- **M6 Market** (§17): waits on its two open questions, a public catalog repository and no hooks in the box.
- **Zero-page skip in the CH fork snapshot writer** (measured: v6 golden 8.00 → 3.22 GiB, mmap restore of a hole-punched file works; design and hard gate: full snapshots only, never diff snapshots). With the state layer carrying user data across nodes, the remaining cost of dense snapshots is local only: hibernate writes the full memory (23–50 s at 8 GiB), each hibernated box pins its memory size on local disk until released, and the write evicts neighbours' page cache. Revisit when hibernate time or per-node disk becomes the constraint.
- **S3 checkpoint export sparse + compressed** (sandbox store): only needed if memory-image checkpoints ever travel between nodes again.
- **Price a proactive wake in the free-tier cost model** (§15.4). Done 2026-10-01 in §15.8: about $0.007 for a text run at list price and $0.059 for a GUI run, or $0.63 a user-month at 3 text runs a day. A free tier's day budget is still open (§15.12, decision 1).
- **Off-node state store (GCS) for real node-loss recovery.** On the bench host the MinIO state store (`/srv/cb/state`) sits on the same GCP local NVMe as the boxes. That disk survives a host restart but not an instance stop or termination, so a lost node takes the state layer with it, and §14.6's restore-on-missing only covers a lost machine on a live node.
- **Re-seed cbsbx after a local-SSD wipe.** The template tarballs (`/srv/cb/sbx/build`), the imported templates and the sandboxd goldens live on the same local NVMe as the boxes, so an instance stop leaves no template to claim from. Keep the current and previous template tarball off the node (next to the off-node state store) and document the re-seed: mkfs, `deploy/cbsbx/install`, `cocoon-cbsbx image import`, `cbsbx-conf`.
- **Per-port internal allow in the sandboxd egress guard** (upstream sandbox mechanism; cocoonstack/sandbox#276). Done 2026-10-01: cbsbx runs sandboxd 90230c0 (#277) with `egress_internal_allow [10.203.0.1/32:18090,9000]`, so the proxy itself refuses any other port on 10.203.0.1; cb-host-net's `CB-OUTPUT` stays as the second layer.
- **Reopen local agents after an app restart** (app). Quitting the app ended every local agent under On this Mac, and the next launch started none, although the CLIs keep their sessions on disk. Done 2026-10-01:
  - `~/Library/Application Support/CocoonBox/local-agents.json` lists each open local agent: kind, box id, the CLI's session id, workspace, start time, the automatic and the user's name. The app rewrites it on every change (start, End, rename, a new terminal title), so a killed app loses nothing.
  - Session ids: `claude` gets one from the app (`--session-id`). `codex` and `cursor-agent` choose their own; the app claims the earliest session in the shared workspace that began after the agent started and before the next agent of the same kind started (`codex`: the rollout's `session_meta` `cwd` and `timestamp`; `cursor-agent`: `~/.cursor/chats/<md5 of the workspace>/<chat id>/meta.json` with a conversation). Two agents of one kind started within the CLI's start-up time can still swap.
  - Once the box is running, the app reopens them with the box MCP config: `claude --resume <sid>` (a new `--session-id <sid>` when no conversation was written), `codex resume <sid>`, `cursor-agent --resume <chat id>`; an agent with no conversation yet starts fresh. End removes it from the file.
  - Names: a local agent's automatic name is its terminal title without spinner, status and workspace name (`claude` and `codex` title the conversation; `cursor-agent` shows its chat title); a double-click renames it, and the user's name wins and is kept across restarts.
- **Offload a coding session into the box:** designed in §27, contract in contracts.md "Offload". The entry is the user's own terminal session, not the app's local agents (§27.4).
- **Allowlist the tools the mode-A runner forwards** (agent). The loop passes any tool name the model emits to the toolhost, so a model that guesses an internal call (`state_*`, `browser_close`, `desktop_save`) can invoke it. Since v10 the model's shell has sudo, so this adds no privilege inside the box, but the dispatcher should still forward only the tools it advertised. Found during the v10 acceptance (2026-10-01).
- **M4.9 proactive agents** (§15): the whole build plan (9a–f) waits here, with its decisions settled in §15.12.
- **Keychain prompt on every app rebuild.** The ad hoc signature changes with each build, so the login keychain asks again. With an Apple Developer account, sign with Developer ID, which also enables notarization, and move the token to the data protection keychain behind Touch ID (`SecAccessControl` `.userPresence`). Until then the prompt stays.
- **Batch small chunks in state transfers** (agent state layer and api). Restore is one presigned GET per chunk through the sandboxd proxy, so time grows with the chunk count, not the bytes: a 1298-chunk restore took about 11 s against 2 s for a 519-chunk one (2026-10-01). Pack small chunks into larger objects (for example, a pack per save with an index in the manifest, ranges read with one GET), and raise the transfer concurrency. Measure the restore of a 1300-chunk box before and after.
- **gw: look up keys by owner, and store a hash instead of the key** (upstream gw mechanism; cocoonstack/gateway#137 and #138). Done 2026-10-01: the bench host runs gw HEAD 3ab7a722, which filters `GET /admin/keys?owner=` before paging and stores and reports keys by `ak_id` only. The api lists a machine's keys by owner, lets gw generate each key and revokes by `ak_id`.

## 19. Several boxes per user

Proposed 2026-10-01. Direction: an agent in a box drives only that box, and a local coding agent may drive any of the user's boxes, told apart on the MCP side.

### 19.1 Which agent drives which box

- **Mode A (in the box): one box.** The agent lives in its box: its transcript is that box's `agent.db`. Driving another box would need a credential in box A that opens box B, which breaks boundaries.md invariant 1, and a prompt injection in A would reach B.
- **Mode B (local): any of the user's boxes.** The client already holds the user token, and the edge authorizes every call against box ownership, so crossing boxes adds no new trust.
- An agent that orchestrates several boxes unattended belongs in the api (§15), never in a box.

### 19.2 Where the split lives: the bridge

The local bridge (`cocoonbox mcp`, §13) already merges the control MCP and one box's MCP into one stdio server. It becomes the multiplexer:
- Box tools get an optional `box` argument (a box name or id). It defaults to the bridge's default box (`--box`, else the newest running box), so a single-box user sees no change.
- The control tools gain `box_list`, `box_create` and `box_delete`, and the existing ones take `box`.
- The bridge opens one MCP session per box on first use and routes each `tools/call` by its `box`. (Superseded by §23: the owner's MCP sessions are not tracked, so no box records an `mcp:<client>` agent; the CLI keeps its own history, 19.6.)
- `tools/list` is the box tool contract once, plus `box`. Every box serves the same tool set; per-box plugins (§17) come later as a union annotated with their box.

Not chosen:
- **One MCP server per box in the client config** (`cocoonbox-dev`, `cocoonbox-test`): it multiplies the tool list by the number of boxes in the model's context, and a new box needs a config edit and a client restart.
- **Multiplexing at the edge:** the edge would have to read tool arguments, against boundaries.md ("the edge never interprets agent payloads").

The app's **+ → Local agent** starts the CLI through the bridge (stdio) instead of the direct HTTP config, with `--box` set to the box selected in the app.

### 19.3 Control plane

- `Boxes.Create{name, image, flavor}` and `Boxes.Delete{box_id}`. `Box.name` is unique per user. `Ensure{box_id}` wakes or creates that box; an empty `box_id` keeps today's meaning, the user's first box, for old clients.
- **Quota** (as built, 19.6): the total is the per-user max boxes setting (§22.4), the running limit is api config `max_running_boxes`, default 1. Waking or creating a box beyond the running limit fails with `FailedPrecondition` naming the running box; the caller hibernates it first. The api never hibernates a box on its own to make room.
- **Delete:** a quiesced save first, then the machine is removed. The gw key is the user's since §22.4 and stays (19.6). The state prefix stays until a retention sweep, so a mistaken delete can be restored from the store (as built: `state.gc.deleted_retention`, 19.6).
- Hibernated boxes cost disk only: on sandboxd, the memory file and the disk on the node's NVMe.

### 19.4 App

- A box switcher at the top of the sidebar. The screen, agents and lifecycle buttons follow the selected box.
- A Mode B agent's activity shows under the box it touched, as its `mcp:<client>` agent there. (Superseded: owner sessions are not tracked, §23, and local agents show their boxes themselves, below.)

Decided 2026-10-03: an agent is an agent, a box is a box.
- In-box agents (Mode A) stay per box. The switcher selects the box, and its in-box agents and screen follow it.
- Local agents (Mode B, started by the app) belong to no box in the UI. **On this Mac** lists all of them, whatever box is selected. Each row shows the agent's default box by name, and its context menu changes it; the change applies to the agent's next call that names no box, with no CLI restart. Each row also shows a badge per box the agent called in the last 60 s.
- Selecting a local agent never moves the screen. The screen is the box the switcher selects.
- The default box and the calls travel between the app and the bridge over a local socket, never through the server (19.6).

### 19.5 Order and acceptance

Order:
1. proto and contracts.
2. api: Create, Delete, name and quota.
3. Bridge multiplexing and the control tools.
4. App switcher and **+** through the bridge.

Hot path: none on the screen or edge path; the bridge adds one map lookup per call.

Acceptance, on the bench host:
1. Create `test` next to the existing box. Running a second box at once is refused, and works after hibernating the first.
2. One local agent CLI session, with no restart and no config change:
   - lists the boxes;
   - creates and wakes `test`;
   - runs `uname -n` in both boxes with `box` set;
   - each box's transcript shows only its own call.
3. A third box is refused by the total quota.
4. Delete `test`: its machine and gw key are gone, and its last snapshot is still in the store.
5. Another user's box name or id in `box` is refused by the edge.

### 19.6 As built (2026-10-03)

Reconciled with §22–§26 before building:

- **Box record.** A `boxes` table (users migration 0004: `id`, `owner` → `users ON DELETE CASCADE`, `name`, `created_at`, `UNIQUE (owner, name)`), kept in memory next to the token index, so ownership on the edge path is one map lookup and a scan of at most 16 boxes. The migration gives every existing user its box `cb-<12 hex of sha256(user id)>`, named `main`; `users.Create` inserts the same first box for every new user. Every other box id is `cb-` and 12 random hex, so a new box never inherits the state prefix of a deleted one. A name is 1 to 32 of `a-z 0-9 - _`, starting with a letter or digit and never with `cb-`, so a `box` argument is never ambiguous between a name and an id.
- **Machine → box.** The api no longer derives the box from the user: a machine's box is its id cut at `-r` (Rebase and restore machines are `<box>-r<base36>`). The serving machine of a box is still its oldest machine. The identity file, the credential, scheduled saves and the restore lock all follow the machine's box.
- **Quota.** The total is the user's max boxes (`users.max_boxes`, at most the admin's cap); `AddBox` counts under a `FOR UPDATE` lock on the user's row, so parallel creates cannot pass it together. Lowering max boxes below the user's box count is refused. The tenant's `max_claims` stays max boxes + 1, which a hibernated box's claim counts against. The running limit is the user's own `max_running` when an admin set one (users migration 0005, nullable, 1 to max boxes; the console admin table sets it, empty returns the user to the default, the user's page shows it, and `PATCH /cb/api/me` with it answers 403), else api config `max_running_boxes` (default 1). Lowering max boxes below the user's own running limit is refused; lower the limit first. A start (an `Ensure` that creates or wakes a machine, every `Create`) is refused with `FAILED_PRECONDITION`, `box <name> (<id>) is running, and 1 may run at a time; hibernate it first`, while that many other boxes are starting, running or hibernating. A per-user lock spans the check and the claim. The api never hibernates a box on its own.
- **`Ensure{box_id}`.** An empty id is the user's first (oldest) box, and adds a box named `main` when the user has deleted all of them, so old clients keep working. A box without a machine is listed with `BOX_STATE_UNSPECIFIED` (absent) instead of being left out; `Get` returns it the same way instead of `NOT_FOUND`.
- **Delete** is an operation (`PHASE_DELETING`): a quiesced save when the box is running and ready (a failed save keeps the box), then every machine of the box is removed, then the row, and the box's live share is revoked in the same transaction, which cuts its sessions (§23). A hibernated or failed box is deleted without a wake, so what stays in the store until the GC purges it is its last save. A delete is refused while another operation runs on the box, and `Ensure`/`Hibernate` are refused during it.
- **gw key.** Keys are per user since §22.4 (owner = user id, issued at sign-up, revoked at user deletion). Deleting a box releases its claim, and with it the host-only `GW_KEY` env of that claim; the key itself stays, even for the user's last box, since the next box's machine needs it.
- **Owner MCP sessions are not tracked (§23).** 19.2's per-box `mcp:<client>` agent is dropped: the bridge routes by `box`, the box records nothing for the owner, and the CLI keeps its own history.
- **Sharing** stays per box: `Shares.Create` checks the box against the caller's boxes (no longer `BoxID(user)`); the partial unique index `shares_live_box` is per box id and needs no change.
- **State and GC.** The state key and the chunk store are per user (§22.5), so a user's boxes dedupe against each other. A deleted box keeps `<user>/boxes/<box>/` for a retention window (built 2026-10-13): the GC lists boxes from the store, then asks the in-memory box index, so a box listed in the store but not in the table is deleted (a box saves only while its row exists, and no box id is reused). The first pass that finds it writes `deleted.json` with the time; until `state.gc.deleted_retention` (default 7 days) has passed since, the pass prunes it by the rule like any box. The first pass after that deletes every object under the prefix under the box's lock, `deleted.json` last, before the sweep, so the sweep marks none of its chunks and deletes those no other box of the user references. A dry run counts the purge and writes nothing. Until then a deleted box's head was kept forever and its chunks stayed marked.
- **Control MCP.** New tools `box_list`, `box_create` (`name`, `wait`) and `box_delete` (`box`, `confirm`); every other tool takes an optional `box` (name or id) and defaults to the first box. A `box` that is not the caller's is refused as `You have no box "<box>"`; `box_delete` needs `box` and `confirm: true`. `box_status` gains `name`.
- **Bridge.** `cocoonbox mcp [--box NAME|ID]`. A box tool's `box` argument is stripped and routes the call; without it the call goes to the default box: `--box`, else the newest running box when the bridge first needs one (it stays until that box is deleted). The bridge fills the default `box` into `box_status`, `box_ensure`, `box_hibernate`, `box_save` and `box_upgrade` when the call names none, never into `box_delete`. Names map to ids from `box_list`, cached and refreshed on a miss or after a create or delete; a `cb-` id passes as is, so the edge decides (404 for a box id the user does not own). One MCP session per box, opened on first use; a box is woken on its first call unless `--no-wake`. `tools/list` is the control tools plus the box tools once, each with `box`. Hot path: one map lookup per call.
- **CLI.** `--box` picks the box of `status`, `ensure`, `hibernate`, `save` and `upgrade` (default the first box). `COCOONBOX_TOKEN`, when set, takes the place of the Keychain item: the app sets it for the agents it starts, which pass it on to the bridge.
- **App.** A box switcher at the top of the sidebar (select, New Box, Rename, Delete). The screen, events, agents, share and lifecycle buttons follow the selected box; at launch it selects the newest running box, else the first. The sidebar lists every local agent (19.4, decided 2026-10-03); their records keep their `boxID`, now the agent's default box, and all records reopen once per launch at sign-in, a record whose box is gone with the selected box as its default. **+ → Local agent** now runs the agent CLI with the bridge as its stdio MCP server (`cocoonbox mcp --box <id> --endpoint <url>`, the CLI from `Contents/Helpers/cocoonbox`, or `cocoonbox-cli` next to a development build) instead of the direct HTTP config; the token stays in the child's environment as before (`COCOONBOX_TOKEN`), and `claude` (`env`), `codex` (`env_vars`) and `cursor-agent` (`${env:…}`) pass it to the bridge. The MCP server is still named `cocoonbox`, so the tool names a reopened conversation knows are unchanged. The Connect sheet's direct recipes stay.
- **Local agents and the app's socket** (2026-10-03). The app listens on `~/Library/Application Support/CocoonBox/agents.sock` (mode 0600; the test HOME redirects it). It hands the bridge the socket path and the agent's id the way it hands it the token, as `COCOONBOX_APP_SOCKET` and `COCOONBOX_AGENT` in the CLI's environment, named in the MCP config (`claude` `env`, `codex` `env_vars`, `cursor-agent` `${env:…}`) and never in argv. The bridge connects at start and sends newline-delimited JSON: `{"agent"}` once, then `{"agent","box","at"}` for every call it routes to a box's MCP (the box id; `at` in Unix ms). Control tools (`box_status`, `box_list`, `box_create` and the rest) are not reported, so the row shows only real box work. The app answers the first line with the agent's default box, `{"box"}`, and sends `{"box"}` again whenever the user changes it; the bridge uses the latest one for calls with no `box`, in place of `--box`. So a bridge that restarts with an old `--box` still gets the current default. The agent's terminal pane has a Default box picker; when the box switcher selects another box, the pane says "Screen shows <box>" beside it and changes nothing. The row reads the agent's kind, and below it "↗ main · test2", the boxes it acted on in the last minute. A box stays in that label 60 s after the agent's last call there: longer than an agent's pause between two tool calls while it works, so it does not flicker, and short enough to clear within a minute of the agent stopping or moving on. A call on a box the app's list lacks (one the agent just created) refreshes the list. Best effort: both ends write non-blocking, and a line that does not fit the socket buffer is dropped (the rest of a line the buffer cut goes first on the next write, so lines stay whole). With no socket, an app that quit, or a path longer than `sun_path`, the bridge works as before with its `--box`. Hot path: one mutex read and one non-blocking local `write(2)` of about 100 bytes per call on the bridge; one read and a JSON parse on the app's main queue.
- **Console.** The user page lists boxes with name, id, state and image, and creates, renames and deletes them (`POST /cb/api/boxes`, `PATCH|DELETE /cb/api/boxes/{id}`); the share form picks the box.
- **MockBox** plays several boxes with one shared desktop and agent set, the running limit of 1 and a quota of 3; its shell answer names the box (`on <name>`), so tests can tell where a call ran.

Not built: per-box agents in MockBox, a CLI `list` command, per-box `cursor-agent` workspaces (two `cursor-agent` agents for different boxes share one `.cursor/mcp.json`, written just before each starts). A rename made elsewhere reaches a running bridge only on its next name miss, create or delete, so until then a box tool may still find the box under its old name.

Acceptance: `deploy/accept/multibox.sh` on the bench host with a scratch user (19.5 steps 1, 3, 4, 5 scripted, and step 2's routing checked over the edge by each box's identity file, since clones share a hostname; the agent CLI session itself stays manual and the script prints it. Step 2's per-box transcripts no longer apply, since the owner's sessions are not tracked).

## 20. Sign in to a website with a password saved on the Mac

Proposed 2026-10-01. The user has a site's password in the Mac's Passwords (iCloud Keychain). The box's Chrome should sign in with it, and the password should never be written in the VM or seen by any model.

boundaries.md invariant 2 allows exactly this shape: "inside the VM only in memory for one request". The session cookie that results is the existing exception, browser sessions, encrypted in the state layer.

### 20.1 Flow

1. **Pick on the Mac.** A third-party app cannot read Passwords entries directly, and an ad hoc signed app gets no AutoFill on an arbitrary site (20.4). The app opens a small native sheet with a username field and a password field (`NSTextField` and `NSSecureTextField`). The user pastes the values from Passwords or 1Password, or types them, and the app holds them in memory only. The fields keep `contentType` `.username` and `.password`, which cost nothing and may get AutoFill once the app has Developer ID signing.
2. **Send.** The app calls a new agent RPC, `Session.FillCredential{origin, username, password, submit}`, through the edge with the user token. It is app-only:
   - it is not an MCP tool;
   - neither the box's loop nor a local agent can call it or see its arguments;
   - it never appears in a transcript, an event or a log.
3. **Fill.** The toolhost picks the Chrome tab whose top-level origin equals `origin` (scheme, host and port) and fills the page's sign-in form through CDP (the focused field, else the first username/password pair), then submits if asked.
   - A tab on any other origin is refused, so a phishing page or a redirect gets nothing.
   - The values live in the agent and toolhost memory for this call only, and are zeroed after.
4. **Keep it off disk.** The image sets Chrome's `PasswordManagerEnabled=false` (managed policy), so Chrome never offers to save the password. The resulting cookies follow the browser-session rule.

### 20.2 Entry points

- **Take-over:** a "Sign in with Passwords" button in the screen header, used while the user has taken over and the tab shows a sign-in page.
- **Agent approvals:** when the box agent stops on a sign-in page (`ApprovalRequest{kind: "take_over"}`), the approval card adds the same button. After the fill, the user answers the approval and the agent continues.

### 20.3 Not covered, and second factors

- **Passkeys: not today; possible with one Apple entitlement.** The private key never leaves the Mac, so the WebAuthn ceremony has to be relayed:
  1. the agent catches the page's `navigator.credentials.get` through CDP and hands the challenge and `rpId` to the app;
  2. the app asks macOS for an assertion (Touch ID);
  3. the agent resolves the page's promise with it, and the site verifies the signature as usual.
  macOS lets an app request assertions only for its own associated domains, except for apps holding the managed web-browser entitlement `com.apple.developer.web-browser.public-key-credential` (Developer ID plus Apple's approval, what Chrome and Firefox hold). A remote browser is the case that entitlement exists for, but Apple decides. The hybrid QR flow (scan with an iPhone) needs Bluetooth next to the browser, which a cloud VM does not have. Backlog: apply for the entitlement once the Developer account exists, then build the relay.
- **TOTP and SMS codes: the One-time code mode.** Its field has content type `.oneTimeCode`, where macOS AutoFill offers a verification code saved in Passwords and, when the iPhone forwards SMS, a code from Messages. To measure. The TOTP secret never goes into the box.
  - Small step once that works: the agent reports whether the active page has a one-time-code input (`autocomplete="one-time-code"` or a 6-digit numeric field), and the sheet opens in code mode. Agent change, so it ships with an image cut.
- **Push approvals** (approve on the phone) need nothing from the box.
- **Native apps in the box:** only Chrome, through CDP. Typing a password as keystrokes over RFB is not used, because the keys go wherever the focus is.

### 20.4 AutoFill in the sheet

- Measured 2026-10-01 on macOS 15.7.7 with an ad hoc signed probe: clicking the Username field showed no suggestions, and the note here then said an ad hoc signed app gets no AutoFill. That was wrong for the Password field.
- Seen 2026-10-02 in the shipped app (ad hoc signed): focusing the Password field (`NSSecureTextField`, `.password`) brings up "Passwords is locked" (Touch ID), then the system's "Choose a password" picker over every saved entry, with a search box. Picking one fills the user name and the password.
- The picker is not filtered to the box's site, and the app cannot filter or pre-fill its search: macOS matches entries to an app's associated domains only, which a site's owner declares, and the picker runs out of process. So the user types part of the site name (`face`) into its search.
- The sheet focuses the User name field (the Code field in One-time code mode) when it opens, since the account comes before the password. Clicking the Password field brings up the picker.
- 1Password users could get an exact match through its CLI (`op item list --url <origin>`); not built.

### 20.5 Work split and acceptance

Work split:
- **proto / contracts.md:** `Session.FillCredential`; the rule that it is app-only and never logged.
- **agent:** the RPC handler (no transcript entry, no event), the toolhost CDP fill with the origin check, and zeroing the buffers.
- **image:** Chrome policy `PasswordManagerEnabled=false`.
- **macos:** the sheet (paste or type), the take-over button and the approval-card button.

Hot path: none.

Acceptance, on the dev box:
1. Sign in to a test site (a local page served in the box with a known account) from a Passwords entry. The page shows signed in.
2. Search for the password in:
   - the guest's `/proc/*/environ`, `/home`, `/workspace`, `/tmp`, agent.db and the Chrome profile;
   - the next state snapshot;
   - the api and agent journals.
   No match. A positive control matches.
3. A fill aimed at an origin that doesn't match the active tab is refused, and nothing is typed.
4. The box agent and a local MCP client both list their tools, and neither has a fill tool.

## 21. Residential or datacenter exit per box

Proposed 2026-10-01. Some boxes should reach the internet through a residential exit, for sites that block or throttle datacenter addresses; the rest keep the node's datacenter exit. The choice exists only at the sandboxd layer. Inside every box the proxy is still 127.0.0.1, and nothing in the guest says which exit it has.

### 21.1 Pieces

- **sandboxd: a per-claim upstream** (cocoonstack/sandbox#279, landed as #282; cbsbx runs it since 6a07d5a). The egress proxy evaluates policy, secrets and the SSRF guard as today, then makes the final connection through the claim's upstream, if it has one, instead of dialing direct. The upstream comes from a `guest: false` claim env entry (`EGRESS_UPSTREAM` on cbsbx, under `egress_upstream.claim_env`) or a pool default. A dead upstream fails closed (502), never direct.
- **Host: one authenticating forwarder per upstream credential.**
  - A small HTTP proxy on the host, alone on its own address (`cbres0`, 10.203.1.2:8823). The address is on `egress_upstream.allow` and never on `egress_internal_allow`: sandboxd dials an upstream with its plain dialer, and an internal-allow entry would let any box CONNECT to the forwarder itself and bypass the switch.
  - It adds `Proxy-Authorization` for the provider and relays CONNECT and absolute-URI requests.
  - The provider credential lives only in the forwarder's root-only env file.
  - The claim's upstream value is the forwarder's address with no secret (`http://10.203.1.2:8823`), so no claim env, no API response and no box ever holds the provider credential.
- **api: which boxes get which exit.** It's a policy decision, so it lives in cocoonbox (the mechanism-only rule).
  - The first cut is a per-user switch, on or off (default off). It lives in the api's `users` table (§22); until that lands, every user has it off.
  - The api writes or removes the claim's upstream entry through `PATCH /v1/sandboxes/{id}/env` when it provisions a machine (§14.5 `Deliver`), and the provisioner's 10 s pass brings every machine whose entry disagrees with its owner's switch in line, hibernated ones included. A change takes effect for new connections.
  - Later, a per-box choice in the app, and per-site rules (residential only for listed domains) if #279 grows a rule-level upstream.

### 21.2 Provider facts

- Provider: a residential proxy provider, plain HTTP proxy (CONNECT plus absolute URI), Basic `Proxy-Authorization`. SOCKS5 and per-country hosts are unverified.
- The login carries no country or session suffix. Rotation is the provider default, unmeasured. Sticky sessions are untested.
- Latency is clearly higher than the datacenter exit but fine for GUI work. CONNECT tunnels end to end, with no TLS issues.

### 21.3 Order and acceptance

Order:
1. #279 lands in sandbox; cbsbx is upgraded to it.
2. Forwarder unit on the bench host, with its credential file, a port-scoped internal allow entry, and a dedicated bridge address. It never binds `0.0.0.0` or the host network.
3. api setting plus Deliver/PATCH.
4. Docs.

Hot path: none on claim, screen or edge. Residential latency applies only to the boxes that chose it.

Acceptance on the bench host, with dev on residential and a throwaway claim on datacenter:
1. `curl https://ifconfig.me` inside each box shows a residential address for dev and the node's address for the other. Both boxes' proxy env says 127.0.0.1.
2. The provider credential appears in no guest, claim env read, api log or usage/audit record.
3. Stopping the forwarder makes dev's requests fail with 502, never a direct dial.
4. Flipping dev back to datacenter by PATCH changes its exit for new connections without a restart.

## 22. One cocoonbox user, one gw key, one sandboxd tenant

Built 2026-10-02 (22.4 records what was built and the decisions made that night). Proposed 2026-10-01. gw and sandboxd are both multi-tenant. cocoonbox-api keeps a per-user record that links a user to exactly one gw key and one sandboxd tenant, and it hides both from the user: clients hold only their cocoonbox user token. gw then shows each user's total model spend, and sandboxd shows each user's boxes and caps them.

### 22.1 What changes from today

| | Today | Proposed |
|---|---|---|
| User record | `users` in api.yaml (name and token hash), edited by hand, api restart | A `users` table in the api's PostgreSQL (the instance Connect uses): user id, token hash, gw `ak_id`, sealed gw key, sandboxd tenant name, sealed tenant token, state |
| gw key | One per machine (owner = machine id), issued on every new machine and revoked on Rebase | One per user (owner = user id), issued once at sign-up and revoked when the user is deleted. Every machine of the user gets the same key in its claim's `guest: false` env |
| Model budget | Per key = per machine; the tenant pool is shared | Per key = per user: `/admin/usage` by key is the user's total; the user's day budget is a per-user override in the one tenant (§22.2, gateway#144) |
| sandboxd | One tenant-less root token; boxes found by `cb_owner` metadata | One sandboxd tenant per user (name = user id, its own token, `max_claims` = the user's box quota, §19.3). The api creates and lists a user's claims with that user's tenant token, so a bug in the api cannot list or claim across users. Root-only surfaces (per-id reads, `GET /v1/info`) keep the root token |
| Onboarding | Hand edit plus restart | An admin call on the api (later the web console) creates the user, issues the gw key and adds the tenant; deleting reverses all three |

The two invariants hold:
- the gw key is still injected at egress and never enters a VM;
- tenant tokens never leave the api.

Sealed values (as built, 22.4): AES-256-GCM directly under the master key (a 32-byte file on the bench host), additional data `users|<id>|<column>`. There is no per-user data key, no envelope and no master key rotation path yet; the §16.1.1 envelope replaces this when Connect lands.

### 22.2 What it needs upstream

- **sandboxd: tenants at runtime** (cocoonstack/sandbox#284). Tenants live only in `config.json` and are re-read at boot (sandbox docs/cluster.md), so adding a user means a config edit and a restart, and every restart sweeps the warm pool. Self-serve sign-up needs a root-only API to add, update (token rotation, `max_claims`) and remove a tenant, persisted by sandboxd and applied without a restart. The cluster form has to keep every node's tenant set equal. This is mechanism; cocoonbox only calls it.
- **gw: per-user budget overrides** (cocoonstack/gateway#144, which supersedes #143). gw already meters per user across all of a user's keys, since a key's `owner` is the user; what it lacks is a per-user cap value (at 3ab7a722 `key_daily_cost_quota_micros` is one tenant-wide number). #144 adds `PUT|GET|DELETE /admin/tenants/{tenant}/users/{user}/budget` (daily cost, monthly cost, daily tokens; a number, `"unlimited"`, or absent to inherit), callable with the tenant's own admin token, plus a tenant option `require_key_owner`. Admission checks every configured scope independently (tenant pool, per key, per user), so the tightest cap wins; an override replaces only the per-user scope, and `"unlimited"` leaves the key and pool caps in force. Caps are soft: a request is checked before it runs and charged after. Built 2026-10-02: the bench host runs gw db2e82e (#145). gw is authoritative for the per-user default; a tenant token cannot read the tenant config, so api.yaml's `accounts.default_daily_cap_micros` only mirrors it for the console and nothing checks that the two agree.
  - Plan once #144 lands: the api sets a $5 daily override at user creation, `"unlimited"` for admins, and an admin console override; `require_key_owner: true` on `cocoonbox-boxes`; drop the tenant key cap (today $10/day, which would otherwise stop an unlimited admin at $10); keep the pool (today $50/day) as the backstop that admins share. No tenant per user and no global token.
  - Optional later: per-box attribution inside a user's key. The sandboxd proxy would add an attribution header carrying the claim's id, and gw's end-user attribution (`user`) would record it.

### 22.3 Order and acceptance

Order:
1. sandboxd runtime tenants (upstream).
2. The api `users` table, the admin create/delete calls, the per-user gw key (the current gwkeys client issues by user instead of by machine), and Deliver injecting the user's key.
3. The api moves to the user's tenant token for claim and list.
4. Migrate dev and test.
5. Delete the `users` list from api.yaml.

Hot path: none on claim, screen or edge. Deliver reads one sealed key per provision.

Acceptance on the bench host:
1. Create a user through the admin call, with no restart of api or sandboxd. Its first Ensure works; gw lists one key with owner = the user id; sandboxd lists the claim under the user's tenant.
2. A second box for the same user carries the same gw key (same `ak_id` in gw's usage), and the user's `max_claims` refuses one box more than the quota.
3. gw's usage for that key equals the sum of both boxes' model calls.
4. Delete the user: the key is gone (401), the tenant is gone, the claims are released, and the state prefix stays for the retention window.
5. User A's token cannot reach user B's box at the edge, and the api's tenant-scoped list for A never shows B's claims.

### 22.4 As built (2026-10-02)

Decided and built:

- **Web console at `/cb/`** on the api's HTTP listener (the MCP one), reached through a Caddy route. Static HTML and JS embedded in the binary, no build toolchain.
  - Sign-in is username and password (bcrypt). The `idp` column is there for Google later; no Google code yet.
  - The session is an HttpOnly, Secure, SameSite=Strict cookie for 12 h, stored as a sha256 in `sessions`. Mutating requests must send `application/json`, so a cross-site form cannot post one.
  - Password checks (sign-in and password change) are limited to 5 per username and 20 per client address in 15 minutes (in memory). An attempt is reserved under the limiter's lock before bcrypt runs and given back on a correct password, so parallel requests cannot pass the check together.
  - Deleting a user disables it first (its token and sessions stop at once), then releases its claims, deletes every object under its state prefix `<user id>/`, removes the tenant, revokes the gw key and drops the row.
  - Disabling a user stops its tokens and sessions, hibernates its running boxes (their state stays), ends its open gRPC streams and its MCP and Connect door exchanges, and expires its gw key (`expires_at_epoch_secs: 0`); the api's start does the same for the gw key of every disabled user. Enabling clears the expiry; the boxes wake on the next `Ensure`.
- **Accounts come from an admin or from signup**: the console's admin page (create with an initial password, disable or enable, reset the password, delete, per-user settings) or the root CLI on the host (`cocoonbox-api user add|passwd|token-sha256`, secrets on stdin); and, while an admin has signup on, a person's first sign-in with GitHub, with a seat cap, a queue and invites (contracts "Sign-in with GitHub and signup"). Signup is off by default.
- **One personal token per user, plus device tokens.** The personal token (once "API token") is what a user pastes; the console shows it once on regenerate, which invalidates the old one. A browser sign-in of the CLI or the app gets a device token of its own instead, so a regenerate no longer signs those out (contracts "Browser sign-in and device tokens"). Only sha256 values are stored.
- **User id = owner key.** One opaque id is the machine owner (`cb_owner`), the box id input (`cb-<12 hex of sha256(id)>`), the gw key owner and the sandboxd tenant name. New users get `u-<12 hex>`, never reused, since a reused tenant name inherits the old tenant's claims and templates. The migrated users kept their old name as id, so their box ids and state keys did not change. Their token hashes were imported from api.yaml, so neither app had to sign in again.
- **Sealing.** The gw key and the tenant token are sealed with AES-256-GCM directly under one 32-byte master key file (root 0600, `LoadCredential`), with additional data `users|<id>|<column>`, so a value copied onto another row does not open. There is no envelope and no rotation path yet: rotating the master key means resealing both columns of every row. The 16.1.1 per-user data key envelope replaces it when Connect lands.
- **gw key per user:** issued at creation (owner = user id, tenant `cocoonbox-boxes`), revoked at deletion, installed as the claim's `guest: false` `GW_KEY` on each new machine's first boot. A machine made before the switch keeps its per-machine key until it is replaced; the api no longer revokes those, so they are revoked by hand.
- **Tenant per user:** `PUT /v1/tenants/{id}` with the user's token, `max_claims` = max boxes + 1 (the +1 is the old machine a Rebase keeps until the new one serves; the api's own count is the one box per user that the box id enforces), and `egress_class` = `sandboxd.tenant_egress_class`. A tenant without an egress class reaches nothing on a pool with a policy, and sandboxd before cocoonstack/sandbox#292 cannot give an API tenant one; so machines claim under the user's tenant only while that setting is non-empty, and before #292 they stay root claims stamped with `cb_owner`. The api reconciles every tenant at start.
- **Listing stays on the root index**, refreshed every 2 s, not a tenant-token request per call, since Resolve is on the edge path. A tenant claim's owner comes from sandboxd's `tenant` stamp, which outranks the api's own metadata; root claims made before the switch keep being found by `cb_owner` and adopted until replaced, so the switch never created or restored a second machine.
- **Daily model budget per user** (§22.2, built 2026-10-02): tenant `cocoonbox-boxes` sets `user_daily_cost_quota_micros: 5000000` as every user's default, keeps the $50/day pool, drops the per-key cap and sets `require_key_owner`. The api gives admins an `"unlimited"` override at creation and at the startup reconcile when they have none, drops a user's override at deletion, and lets the admin page set a user's cap (dollars, unlimited, or inherit); the user page shows the cap and today's spend (the ledger since UTC midnight). Hot path: none in the api; gw does one in-process override lookup per request with an attributed user.
- **Settings per user:** residential exit (default off; only the admin page flips it; `PATCH /cb/api/me` with `residential` answers 403; `ApplyExit` runs after the row is committed) and max boxes (the admin sets the cap, default 1, at most 16; the user picks up to it).
- **Scheduled saves cover every box**; `state.scheduled_users` is gone with the `users` list (no back-compat).
- **Storage:** database `cocoonbox` on the bench host's PostgreSQL, role `cocoonbox-api`, peer authentication over the unix socket; migrations are embedded and run at start under `pg_advisory_lock`.

Hot path: none. Token checks read an in-memory index, updated by every write; the root CLI sends `NOTIFY cocoonbox_users` after its writes, and the api, which `LISTEN`s, reloads the index on each notification and after each (re)connect of its listening connection. Ensure unseals one tenant token (microseconds); provisioning unseals the gw key once per new machine.

### 22.5 State key and chunk store per user (decided 2026-10-02, built 2026-10-02)

§11 says per-user keys and one prefix per user; the code drifted to one key and one prefix per box (`keyring.Key(ctx, box)` reads `/var/lib/cocoonbox-api/keys/<box>.json`, and `api/boxes/statelayer.go:404-407` keys every object by box). With one box per user the two are the same. With several boxes per user (§19) they are not, so the per-user form was decided:

- **One state key per user.** 32 random bytes, sealed in a `users.state_key` column like the gw key and the tenant token (§22.4 sealing, additional data `users|<id>|state_key`). The keyring files go away.
- **One chunk store per user,** shared by all of the user's boxes:
  - `cocoonbox-state/<user>/chunks/<id>` and `<user>/index.json` (the chunk index);
  - `cocoonbox-state/<user>/boxes/<box>/{manifests/, snapshots/, head.json}` (each box keeps its own snapshot chain).
- **What it buys:**
  - a file present in two of the user's boxes is stored once, because chunk ids are keyed hashes under the same key;
  - a new box can start from another box's snapshot with no copy and no re-encryption (§19 create-from);
  - deleting the user drops the key, which makes every snapshot of every box unreadable at once; the prefix is then removed by the retention sweep.
- **What it costs:** a box that is compromised during a save or restore, when it holds the key in memory, can keep it and read the user's other boxes' state. Accepted: the boxes belong to one user and are one trust domain. The alternative (per-box keys derived from the user key) keeps boxes apart but loses cross-box dedup and cheap copies.
- **Concurrency:** saves of two boxes of one user both update `<user>/index.json`. The state layer's lock moves from per box to per user for the index write (plan, upload and restore of different boxes still run in parallel).
- **GC** (§25.3): a chunk is garbage only when no kept snapshot of any of the user's boxes references it, so the sweep walks every box's snapshot records under the user.
- **Migration** of the users that exist (one box each): the user's state key is that box's current key, so no chunk is re-encrypted and no chunk id changes. Copy the objects from `<box>/...` to the new layout server-side (S3 CopyObject), switch the api, verify a restore of each box from the new layout, then delete the old prefix. Do not run it during an open save.

Hot path: none. Save and restore read one sealed key per request instead of one keyring file.

As built (api 185e1bf, b8de45e; switched 2026-10-02):
- **Key:** columns `users.state_key_id` (plain) and `users.state_key_sealed` (AES-256-GCM under the master key, additional data `users|<id>|state_key`), migration 0002. The key id is stored because the agent ignores its `known-chunks.json` stamps under another key id and the api ignores an index under another key id; keeping the box key's id kept both. `Create` draws a key (id `users:<id>:<unix>`); a user without a key cannot save or restore (no key is created on first use, so a user that missed the migration fails loudly instead of starting a second key). Deleting the user drops the row and with it the key; the prefix stays, as before.
- **Layout and locks** as above. A save or restore holds the box's lock; the index write is a read-modify-write under a lock on `<user>/index.json`, so two boxes of one user save in parallel and neither drops the other's chunks.
- **Migration:** `cocoonbox-api state migrate [-keyring DIR]`, safe to rerun at any time. Per user it imports `<keyring>/<box>.json` into the row (a no-op when the row holds that key, an error when it holds another; a user without a file and without a key gets a new one), copies every object of `<box>/` missing under the new layout with CopyObject, writes the union of both indexes, and moves the new head only forward (by `saved_at_ms`). It reads the old head and index before listing, so everything they name is copied. Unknown objects or indexes under different key ids stop it.
- **Commit failures** (185e1bf): the commit writes the index first (its chunks are stored once Upload returns), then the record, then the head, each retried 3 times (1 s, 2 s). If a commit still fails after a successful Upload, the agent's known chunks are ahead of the index, and the next save's Upload is refused with InvalidArgument (`plan again without a parent`). The api then plans once more without a parent, which spools every chunk. The heal keeps no state, so it survives an api restart; it costs one full read of the tree, once.

Acceptance on the bench host:
1. After the migration, the two existing boxes each restore (a same-image Rebase of one, the other on its own upgrade) from `<user>/boxes/<box>/head.json`, with the same snapshot id and file count as before.
2. With §19, a second box of dev seeded with a file already in its first box uploads no chunk for that file.
3. Deleting a test user removes its `state_key`; its snapshots can no longer be opened, and its prefix goes with the retention sweep.

## 23. Sharing a box

Decided 2026-10-02, revised 2026-10-03. A box owner shares one box through a share link. Whoever holds the link watches the screen in a web page and can connect their own TUI coding agents to the box over MCP. Nobody takes over the screen through a share. The same change stops tracking the owner's own MCP sessions as agents.

### 23.1 Decisions

- **Share:** one per box, created by the owner in the app or the console. It always expires: 24 h by default, and the owner picks a longer expiry when creating it. The owner can revoke it or regenerate it at any time. Revoking, regenerating, expiry and deleting the box all cut every open share session at once.
- **Who may use it:** anyone holding the link, or only named cocoonbox users. An open share needs no account. A named share also needs the caller's user token. Both reach the box over the same connection.
- **Link:** `https://<endpoint>/cb/s/#<secret>`, a 128-bit random secret. The secret sits in the fragment, so it reaches neither the server's access logs nor a `Referer`. The page reads it and sends it on its WebSocket. The api looks it up by its SHA-256 and keeps it sealed so the owner can see the link again (23.5).
- **Live view:** the page shows the box screen with noVNC, view-only. The api bridges a WebSocket to the box's RFB server, which it reaches through the sandboxd port relay to 5900, and drops every client input message. The owner can use the same page.
- **Guest agents:** an MCP endpoint per share. The guest's CLI uses the share secret as its bearer, plus the guest's own user token for a named share.
  - **Read by default:** the read-only tools (screenshot and `computer_state`, file read and list, the browser page snapshot). File read and list reach only `/workspace` (23.6).
  - **Write:** the owner turns on "allow write" for the share in the app or the console. That adds every other in-box tool. It takes effect on the next call, and turning it off does too. Write includes `shell`, so it cannot be confined: the app and the console warn next to the switch that it hands the guest every login in the box.
  - **Never:** lifecycle tools (wake, save, upgrade, hibernate, delete) and `Session.FillCredential`, which stays app-only (§20).
- **Watching:** the owner sees a guest's actions on the live view. The app's External list shows the share sessions: who (the user name, or "link"), which client, since when, and each one's full tool-call log, recorded as a `mcp:<client>@<share>` agent. The owner can end a session from the app.
- **Owner's own MCP sessions are not tracked:** no `mcp:*` agent, no transcript, nothing to prune. The CLI keeps its own history, and the app shows its terminal. The pane under a local agent that mirrored its box-side calls goes away, and so does the special case that hid those sessions from External.
  - Kept: every MCP tool call counts as in flight for quiesce, so a save or a Rebase still waits for a running `shell` or file write. A counter, nothing on disk.
- **Disclosure** on the share sheet: anyone with the link sees the whole desktop, including the browser's signed-in sites and `/workspace`, and with write on, their agent acts on it. Box egress injects the owner's gw key. A guest's own CLI spends the guest's own model quota.
- **Stages:** one box per user first. Several boxes per user (§19) follow as a later stage, each box shared on its own.

### 23.2 Pieces

- **api:**
  - a `shares` table in PostgreSQL (id, box, secret_sha256, allowed user ids or none, write, expires_at, created_at, revoked_at);
  - console pages and app calls to create, list, toggle write, regenerate and revoke;
  - the edge authorizes a box call for its owner or a valid share, and revocation drops that share's streams.
- **Edge credential:** the signed `x-cocoonbox-credential` (§12.1) gains the caller, `principal = owner | share:<share id>:<read|write>[:<user id>]`. The agent trusts only that field to decide tracking and the tool set.
- **agent:** MCP filters `tools/list` and `tools/call` by the principal's mode, tracks agents only for share sessions, and keeps the in-flight count for every session.
- **Console:** the share page with noVNC (static, embedded), and the WebSocket-to-RFB bridge in the api.
- **App:** a Share sheet (create, expiry, named users, allow write, revoke). External lists share sessions, with an End button for each.

Hot path: the edge adds one share lookup (in memory, like the token index) per call that does not come from the box's owner. Owner calls are unchanged.

### 23.3 Acceptance on the bench host

1. qa1 creates an open share of its box. With only the link, a browser shows qa1's screen and ignores mouse and keys. An agent CLI configured with the share lists only the read-only tools, and a shell call is refused.
2. qa1 turns on write. The next shell call from that agent CLI runs. Lifecycle tools and FillCredential stay refused. Turning write off refuses the next shell call.
3. A named share for qa2 refuses the link without qa2's token and accepts it with qa2's token. qa3's token is refused.
4. qa1's app lists the share session under External with its calls. qa1's own agent CLI session creates no agent on the box.
5. A Rebase of qa1's box waits for a share session's running `shell` call before it quiesces.
6. Revoking, regenerating and expiry each cut the share's page and MCP sessions within a second, and the old link answers PermissionDenied afterwards.

### 23.4 As built (2026-10-03, stage 1)

Decisions made while building, on top of 23.1 and 23.2:

- **Secrets.** A share secret is `cbs_` and 32 hex digits (128 bits). The edge looks a bearer up as a user token first, and only when no user holds it and it starts with `cbs_`, as a share secret. A user token that happens to start with `cbs_` (one imported from api.yaml could) stays its user's. Both are in-memory lookups by SHA-256, so no secret is ever compared directly. The api also keeps the share secret sealed for its owner (23.5); the edge never opens it.
- **Table** `shares` (users migration 0003): `id` (`sh-` and 12 hex), `box`, `owner` (references `users`, `ON DELETE CASCADE`), `secret_sha256` (unique), `user_ids text[]` (empty for anyone with the link), `write`, `expires_at`, `created_at`, `revoked_at`. A partial unique index allows one row per box with `revoked_at IS NULL`. Create revokes an expired row of the box in the same transaction, so a box has one live share. Regenerate rewrites `secret_sha256` and keeps the id. Expiry writes nothing; the row only stops being loaded. (Superseded by 23.5: migration 0006 adds `secret_sealed`, and expiry clears it.)
- **Index.** `users.Service` keeps the live shares in memory next to the token index, loads them at start, and updates them on every write. Each live share has a `Cut` channel, closed when its streams must end, and a `Moded` channel, closed when write switches. A timer at `expires_at` drops the share. Disabling the owner cuts the share's streams. Deleting the owner drops the share from the index, and the cascade deletes the row.
- **RPCs.** The new `cocoonbox.control.v1.Shares` service: `Create{box_id, ttl_seconds, user_names, write}`, `List`, `SetWrite{share_id, write}`, `Regenerate{share_id}` and `Revoke{share_id}`. Create and Regenerate return the secret; since 23.5, every `Share` the owner gets carries it too. `ttl_seconds` 0 means 24 h; the range is 1 min to 30 days. Users are named by user name, and an unknown name is `INVALID_ARGUMENT`. The console calls the same server at `/cb/api/shares[/{id}[/regenerate]]` (GET, POST, PATCH, POST, DELETE). The agent gains `Agents.End{agent_id | share_id}`.
- **Guest MCP wire.** `https://<endpoint>/cocoonbox.mcp/<box id>` with `Authorization: Bearer <share secret>`. A named share also needs `X-Cocoonbox-Guest-Token: <the guest's own user token>`; the app's snippets read it from `${COCOONBOX_TOKEN}`. The edge strips both headers before proxying. It answers:
  - 401 for a named share without a guest token, or with a token that is not valid;
  - 403 for any other refusal: an unknown, revoked, regenerated or expired secret, another box, or a user the share does not name.

  A share secret is never a gRPC token: gRPC and the control MCP authenticate user tokens only, so a share secret gets `UNAUTHENTICATED` there, and no share reaches Agents, State, `Session.FillCredential` or the lifecycle tools.
- **Edge credential.** Owner calls keep the byte-identical v1 credential. A share call carries `v2.<b64url(key_id|box_id|wake_id|exp|principal)>.<sig>`, with `principal = share:<id>:<read|write>[:<user id>]`. A v17 agent refuses v2 as malformed (401), so sharing needs a v18 box, and an old box fails closed.
- **Agent.**
  - It takes the principal only from the verified credential. The edge token and v1 are the owner.
  - Agents and State refuse a share principal with `PERMISSION_DENIED`, and so does every Session method except `Screen`.
  - An MCP session is bound to the principal that opened it: another principal, including another user of the same share, gets 404 for its session id.
  - `tools/list` and `tools/call` follow the mode of each request's credential. A read share gets `screenshot`, `computer_state`, `read_file`, `list_dir`, `browser_snapshot` and `browser_read`, with files only under `/workspace` (23.6); a write share gets all 13 remote tools. A refused call gets JSON-RPC `-32602`.
  - Share sessions declare `listChanged: true`. When a request carries a mode other than the one the session last listed tools in, the session's GET stream sends `notifications/tools/list_changed`.
  - On a write switch, the edge also cuts the share's open GET streams (`Moded`). The client reconnects at once with the new mode and gets the notification, so a client that listed read-only tools learns about `shell` without a restart. POST exchanges are not cut, so a running tool call survives a switch.
- **Ending.** Revoke, regenerate and expiry close `Cut`. The edge cancels every open MCP exchange of the share, and every share page bridge, at once. The api then calls `Agents.End{share_id}` on the box, best effort: a box that is not running has nothing to close. That call closes the share's sessions, and their agents go idle. The app's End button calls `Agents.End{agent_id}`. A guest whose session the owner ended can open a new one while the share lives; revoke to keep them out.
- **Tracking.** A share session is the agent `mcp:<client>@<share id>`, with `:<user id>` added for a named share. The owner's sessions create no agent, no transcript and no events, except `ComputerAction` for the live-view marker. Their toolhost shell is keyed by the MCP session id and is closed when the session closes. Before this change it was closed only by the 24 h prune of the agent.
- **In-flight count.** 23.1 calls it kept, but at main 8ecd6e0 MCP calls did not hold the turn gate, so a quiesce did not wait for them. Every MCP tool call now holds `rt.turns` shared, like a loop turn, so a quiesced save or Rebase waits for it, and the call waits out a running save.
- **Quiesce bound.** Before this change a quiesce waited 30 s for the gate and then failed with `UNAVAILABLE` ("a turn is still running"); the save failed and a Rebase kept the old machine. A shell call may run 600 s, so a guest could have failed every Rebase. Now, past the 30 s wait, the agent interrupts the share sessions' calls: each returns `interrupted: …` in its transcript and its shell session is ended, so a `sleep 100000` dies. Then the quiesce waits up to 5 s more. The owner's turns and MCP calls are not interrupted; if one still holds the gate, the quiesce fails as before. Hibernate takes no quiesce (sandboxd snapshots the VM), and scheduled saves are live, so neither waits on a guest.
- **Share page.**
  - `/cb/s/` is static: noVNC v1.7.0 `core/` and `vendor/pako/`, unmodified, under `console/static/s/novnc/`, with `LICENSE.txt` and the license texts.
  - The page opens `wss://<host>/cb/s/ws`. The api checks that its `Origin` host is the page's host. The page sends the secret from the fragment as the first text message; nothing travels in the URL.
  - The api answers `{"ok":true}` or `{"error":…, "code":…}`, then bridges binary RFB to the box's RFB server through the sandboxd port relay to 5900, view-only both ways.
  - A named share takes the viewer from the console session cookie (`cb_session`, `Path=/cb/`, `SameSite=Strict`, sent on the same-origin WebSocket). Without one, the page asks the viewer to sign in at `/cb/`.
  - CSP for `/cb/s/`: `default-src 'self'; img-src 'self' data:; connect-src 'self' wss://<host>; frame-ancestors 'none'`. `data:` images are noVNC's JPEG rects and cursor.
- **View-only filter**, parsed message by message in both directions without decoding pixels:
  - Viewer to box: it passes the version (3.3, 3.7, 3.8), security type None only, ClientInit forced to shared, SetPixelFormat at 32 bits per pixel only, SetEncodings rewritten to Raw, CopyRect, ZRLE and the pseudo-encodings DesktopSize, LastRect, Cursor, ExtendedDesktopSize, Fence and ContinuousUpdates, FramebufferUpdateRequest, EnableContinuousUpdates and Fence (payload up to 64 bytes). It drops KeyEvent, PointerEvent, ClientCutText (either sign of length, skipped without buffering), SetDesktopSize, xvp and the QEMU key and audio messages.
  - Box to viewer: it frames the handshake (refusals included), FramebufferUpdate rect by rect, SetColourMapEntries, Bell, ServerFence and EndOfContinuousUpdates. Each kept encoding has a known length: ZRLE carries a u32, Raw is w·h·4, Cursor is w·h·4 plus a ⌈w/8⌉·h mask, CopyRect is 4 bytes, ExtendedDesktopSize is 4 plus 16 per screen, and DesktopSize and LastRect are empty. It drops ServerCutText, plain and extended, so the box's clipboard never reaches a viewer.
  - Anything else, in either direction, ends the stream.
  - Checked against a live TigerVNC 1.12 with noVNC 1.7's own encoding list (ZRLE, Cursor, ExtendedDesktopSize, LastRect, ServerFence and EndOfContinuousUpdates arrived; 1.2 MB framed in 10 s with no error). noVNC 1.7 decodes ZRLE (`core/decoders/zrle.js`) and asks for Cursor at 24-bit depth, which is its default.
- **Logs.** No secret is logged: the edge logs the principal (`user=share:<id>:<mode>[:<user>]`), and the page logs the box, the principal and the client address. Caddy writes no access log (no `log` directive), and the secret never sits in a path or query.

Hot path:
- **Owner calls:** the old path exactly: one token lookup and the cached v1 credential.
- **Share calls:** the token lookup that misses, one in-memory share lookup by hash, plus one token lookup for a named share, and a v2 credential cached per (box, wake, principal). The page bridge parses RFB headers in both directions and copies pixel bodies through untouched.
- **Agent:**
  - Owner MCP calls now skip the two SQLite transcript writes they used to make, and take one uncontended read of the turn gate.
  - The credential cache maps a credential to its principal.

Deploy:
- **api:**
  - Migration 0003 runs at start under the existing advisory lock.
  - No new config keys.
  - No Caddy change: `/cb/*` already proxies HTTP/1.1, WebSocket upgrades included, to 18201, and `/cocoonbox.mcp/*` and `/cocoonbox.*` are unchanged.
  - New gRPC service `cocoonbox.control.v1.Shares` on 18200; the share page at `/cb/s/`.
- **agent:** ships with the v18 cut; a share works only on a v18 box.
- **Order:** the api before or with v18. A v17 box keeps working for its owner, and refuses shares.
- **app:** needs the api with `Shares`; with an older api, the Share sheet reports the `UNIMPLEMENTED` error of `Shares.List`.
- **Acceptance:** `deploy/accept/share.sh` on the bench host, with scratch users qa1 (with a v18 box), qa2 and qa3 and their console passwords in `QA1_PASSWORD`, `QA2_PASSWORD` and `QA3_PASSWORD`. It issues the three users fresh API tokens and never prints a secret. The browser view, the agent CLI and the app's External list stay manual steps; the script prints them.

### 23.5 The owner can see the link again

Decided 2026-10-03. Before this, the api kept only the SHA-256 of a share secret and the app kept the secret in memory. After an app restart, the Share sheet found the live share but could not show its link. The owner's only way to get a link was Regenerate, which ended the link that guests already used. Now the api keeps the secret sealed and returns it to the box's owner. Nothing goes in the Mac keychain.

- **Storage.** Migration 0006 adds `shares.secret_sealed` (bytea, nullable). The secret is sealed like the users' keys (§22.5): AES-256-GCM under the api's master key, with additional data `shares|<share id>|<box id>|secret`, so a sealed secret copied to another row does not open. No new key or config. `secret_sha256` stays, and the edge looks a bearer up by it as before; the edge never opens the sealed column.
- **Ending.** Regenerate replaces the sealed secret. Revoke, deleting the box, and Create over an expired row set it to NULL with `revoked_at`. Expiry sets it to NULL when the share's timer fires. When the api starts, it sets it to NULL on every revoked or expired row, which covers a share that expired while no api ran.
- **Who gets it.** `Share.secret` (field 7) carries the secret in every `Share` that `Shares` returns: `Create`, `List`, `SetWrite` and `Regenerate`. `Shares` serves only the caller's own shares and authenticates user tokens only, so a guest, and a share secret used as a bearer, never gets it. The console's owner endpoints (`/cb/api/shares`) return it as `secret`, from the signed-in user's session. The link is `<endpoint>/cb/s/#<secret>`, and the MCP snippets need the endpoint, the box id, the secret and whether the share names users; the app and the console build them from `List`. No log line carries a secret: the api logs share ids, and an open failure logs the additional data, which holds ids only.
- **Older shares.** A share created before 0006 has no sealed secret. `List` returns it with an empty `secret`, the app shows "Regenerate to get a link", and the console shows no Show link button. Its link keeps working until it ends; the api does not try to recover it.
- **App.** The Share sheet shows the link, its Copy button and the MCP snippets for a live share whenever `List` returns the secret, as it does right after Create. The in-memory secret is gone; `AppModel.shareSecret` reads `share.secret`. Regenerate stays and says that it replaces the link and ends the old link's sessions.
- **Risk.** A database dump together with the master key reveals every live link. Both already reveal each user's gw key, tenant token and state key, and the api already reaches every box, so this adds no new reach to a holder of both.

Hot path: none. The edge lookup is unchanged; `List` reads the secret from the in-memory index, opened once when the api loads the shares.

Deploy: migration 0006 runs at start. The api before this change (7a3c012) runs on a database with 0006 applied: it names its columns and ignores `secret_sealed`. Rolling back leaves sealed secrets that the old api does not update on Regenerate, so the rollback sets `secret_sealed` to NULL on every row; those shares then show "Regenerate to get a link" after the next deploy.

### 23.6 A read share reads files only under /workspace

Decided 2026-10-03. A read share's `read_file` and `list_dir` reached every file the desktop user can read: the Chrome profile (cookies, Login Data) and the box agent CLIs' logins (`~/.claude`, `~/.codex` and the third CLI's config directory). A link holder could take the owner's web sessions and agent subscriptions, far more than seeing the screen.

- **Rule.** A read share reads and lists only paths that resolve beneath `/workspace`, with `..` and symlinks resolved when the file is opened. Every other path fails the call with `<path> is refused: a read-only share reads files only under /workspace`. A symlink in `/workspace` that leads out is refused, and so is any absolute symlink, even one that leads back in.
- **Unchanged.** `screenshot`, `computer_state` and the browser page tools work as before. The owner and a write share read any path, as before.
- **Write.** A write share has `shell`, so it cannot be confined. The Share sheet and the console say next to "Allow write" that it hands guests every login in the box: browser sessions and agent CLI logins.
- **Enforcement.** For a read principal, MCP sets `workspace_only: true` in the call's arguments, over any value the guest sent; the transcript shows it. The toolhost then opens the path with `openat2(RESOLVE_BENEATH)` relative to a descriptor of the workspace. The kernel resolves every component once, at open, and fails an escape with `EXDEV`, which becomes the refusal. The open descriptor is what is read, and a directory is listed through `/proc/self/fd/<fd>`, so a path swapped after the open changes nothing. A kernel without `openat2` (before 5.6) fails the call; the box runs the image's Ubuntu 24.04 kernel (§14).
- **Tool audit** (the read tool list, `tools::READ_ONLY`):
  - `read_file`, `list_dir`: confined as above.
  - `screenshot`: pixels only.
  - `computer_state`: the screenshot, the titles of the windows on screen, and the accessibility tree of one window, the frontmost or any `pid` the guest names. A title can carry a file path, and a covered window's tree can carry text the screen does not show, such as an editor's whole document. It reads only what the owner has open, and it opens no file. Kept by this decision.
  - `browser_snapshot`, `browser_read`: they read only the Chrome window that the same session opened while it had write. A read share opens no window and starts no Chrome, so it cannot close another agent's window past the window cap; without such a window the call fails with `a read-only share opens no Chrome window: …`. A read share cannot navigate, click or type, so it cannot load a `file://` page. Its window shows a page only when the session had write before (with `shell` already) or the owner navigated that window by hand. No `file://` block is added.
- **Accepted.** A hard link or a bind mount in `/workspace` to a file elsewhere is read as a file in `/workspace`; only the owner or a write session can make one.

Hot path: owner and write calls add one match on the principal. A read share's file call adds one open of `/workspace`.

Deploy: ships in the agent of cbdesk:v20; the app and the console wording ship with their next build.

## 24. User data on a persistent data disk

Decided 2026-10-02. An upgrade today saves the user's files to the store and restores them into a new machine. That is fast now for the save, but the restore moves every byte: a box of 96,949 chunks (9.3 GB) restores in about 87 s. Instead, the user's data lives on a persistent data disk that the box carries from machine to machine; an upgrade replaces only the system disk.

### 24.1 Layout

- System disk: the template (`cbdesk:vN`), replaced by every upgrade. Packages installed with `apt` still come back through the package replay (§ v10).
- Data disk (`/dev/vdb`, one per box): `/home/user` (Chrome profile, dotfiles, agent.db) and `/workspace`. Encrypted in the guest with dm-crypt; the key is the user's state key (§22.5), delivered by the api at every boot and wake and kept in memory only.
- Upgrade: quiesce and unmount on the old machine, release it, claim the new template with the same volume, mount, start the desktop session. No store round trip; expected in the tens of seconds.
- Warm pool: the volume is hot-attached to the claimed warm VM; the agent then mounts it and starts the session, as it restarts the session after a restore today.

### 24.2 Phases

1. **Node-local disk.** A raw sparse file on the node's XFS (reflink-capable), attached through cocoon's runtime disk attach. Needs cocoonstack/sandbox#300 (per-owner persistent volumes: runtime API, attach to warm claims, single writer, hibernate, node placement). cocoon stays loosely coupled: its runtime `vm disk attach`/`detach` is enough, and sandboxd detaches the volume (after the guest unmounts it) before a hibernate or snapshot and re-attaches and re-mounts it after the wake (cocoonstack/cocoon#280 closed as control-plane logic). The state layer stays as the off-node backup and for node loss and moves; it saves from the mounted disk, unchanged. A box is bound to its disk's node.
2. **S3-backed block device.** The same volume API, with the disk's blocks in S3 behind a node-local write-back cache, so a box attaches on any node and node loss costs only unflushed cache. Not S3FUSE for `/home`: Chrome profiles, SQLite and git need locking, fsync, atomic rename and mmap that FUSE-on-S3 does not give. At this phase the state layer becomes snapshots of the volume.

### 24.3 Open

- Default size and growth (proposal: 20 GiB, online grow).
- The single-writer handover during an upgrade (old machine released before the new attach) and its failure path.
- Migration of existing boxes: one last restore onto a fresh data disk.

Hot path: none on the screen or edge path. Wake gains one disk attach and mount; upgrade loses the restore.

## 25. Time capsule

Decided 2026-10-03. The user picks a point in time and gets back the files they had then. The state layer (§11) already keeps every save, so the history is in the store; what is missing is retention, garbage collection and a way to restore part of a snapshot into the running box.

### 25.1 Today (verified 2026-10-03)

- **Every save is kept forever.** The store interface has no delete (`api/store/store.go`), nothing in `api/boxes` removes a record, a manifest or a chunk, and the bucket has no lifecycle rule and no versioning. The scheduler saves every running box every 15 min, whether or not anything changed (`saveStale` in `api/boxes/save.go`).
- **One long-lived box**, read with `mcli` on the bench host:
  - 158 snapshots, saved from 2026-09-30 16:27Z to 2026-10-02 16:08Z;
  - the prefix holds 9.1 GiB in 107,318 objects: 7.8 GiB of chunks (107,000), 840 MiB of snapshot records, 502 MiB of manifests;
  - the head references 104,097 chunks, 6.7 GiB stored (99,722 files, 9.15 GiB logical).
  
  History therefore costs 1.1 GiB of chunks and 1.3 GiB of metadata. Each record repeats the whole chunk list (5.3 MiB at this size), and each manifest the whole file list (3.2 MiB), even for a save with no change. 11 stored chunks are referenced by no record (commits that failed after Upload).
- **Snapshots form one parent chain.** Walking `parent` from the head reaches all 158 records and ends at the one without a parent; no record has two children. Each manifest is complete (every file with its chunks), so a snapshot restores without its parents, and dropping one from the middle breaks no other.
- **Restoring an older snapshot is manual:** release the machine, rewrite `head.json`, restart the api, then Ensure (bench.md). `stateLayer.restore` takes a snapshot id, but every caller passes the head.
- **The agent's Restore has no path filter.** It stages every root whole, quiesces, swaps every root and agent.db, and restarts the desktop (`agent/src/state/mod.rs:268-347`).

### 25.2 Decisions

- **Retention, Time Machine style,** per box: every snapshot of the last 24 h, the newest of each day for 30 days, the newest of each week after that. Days and weeks are UTC. The head is always kept.
  - The rule is stable: no new snapshot lands in a past day or week, and a week's newest snapshot is also the newest of its day, so a dropped snapshot is never wanted later.
  - At 15 min saves this keeps about 96 + 30 snapshots, plus one per week.
- **Pruning runs in the api, in the background.** Chunks that no kept snapshot references are deleted by a mark-and-sweep under the user's lock (25.3).
- **First cut: file and folder restore.** In the app the user picks a time point and a path. The agent restores just those files into `~/Restored/<timestamp>/<path>` in the running box. The machine is not replaced (25.4).
- **Later: whole-box rollback.** A Rebase to a chosen snapshot. It saves the current state first, so a rollback is itself undoable. The next save's parent is the restored snapshot, so the chain becomes a tree; the timeline shows it in time order.
- **Memory images are never part of it.** Running programs are not restored.
- **Timeline:** the app lists each snapshot with its time, the files changed and the bytes added.

### 25.3 Retention and garbage collection

Metadata:
- **`timeline.json` per box** (`<user>/boxes/<box>/timeline.json`): one small entry per kept snapshot (id, saved and taken times, consistency, files, logical bytes, changed files, new chunks, uploaded bytes). Listing the timeline from the records instead would read 840 MiB for that box.
- The commit writes it after the record and before the head. A box without one gets it built once from its records, by the GC pass; until then the commit leaves it absent.
- **Changed files** come from the agent: Plan counts new and removed files, and files whose chunks differ from its last upload. Records saved before that field show only bytes added.

Both steps below run in one background pass in the api, per user: prune every box of the user, then sweep the user's chunks. The pass runs once at start and then every `state.gc.interval`.

Prune (per box, under the box's lock):
1. Apply the rule to the timeline. The head and every pinned snapshot are kept.
2. Write the timeline without the dropped snapshots.
3. Delete their records, then their manifests, and every record or manifest the timeline does not name (a commit that failed after Upload).

A crash between 2 and 3 leaves orphan records; the next prune finds them by listing `snapshots/`. Every reader (restore, browse, the mark) takes the kept set from the timeline, never from the listing.

Sweep (per user, after the prune):
1. **Mark, without a lock.** Read every box's timeline and the chunk list of every record it names. Records are immutable and pruning only removes, so what is marked stays true; only saves that commit later add references. List `<user>/chunks/`.
2. **Take the user's lock** exclusively, then the index lock. Every save holds the user's lock shared from its start to its commit, so no save is between its index read and its commit while the sweep runs. Re-list the user's boxes, re-read the timelines and mark the records committed since step 1.
3. **Garbage** = the index and the chunk listing, minus the marked chunks.
4. **Still under the lock:** write the index without the garbage, then delete the garbage objects (`DeleteObjects`, `state.gc.delete_batch` per call, at most 1000), then release.
   - The index goes first. A crash after it leaves only unindexed objects: a save that needs one uploads it again, and the next sweep deletes the rest.
   - The reverse order would leave index entries for deleted chunks, and a save would skip uploading a chunk that is gone.
   - The deletes stay under the lock: once the lock is released, a save may upload a chunk from the garbage set again, and a late delete would remove it.

Why it is safe against the agent's stamp reuse: the agent's known chunks are those of its last upload, which is the box's head, and the head is always kept. If they are ever ahead of the index, the existing heal applies (Upload refused, plan again without a parent, §22.5).

Cost: the mark reads every kept record, about 5 MiB each at that box's size, so it runs once a day and never under the lock. The lock covers the new records, one index write and the deletes; that user's saves wait for it. The locks are in-process, like today's save locks; several api replicas need them in PostgreSQL first.

The store interface gains `Delete` (S3 `DeleteObjects`), and `List`, which the migration already used, moves into it and returns each object's size.

As built (2026-10-03):
- **One user lock instead of every box lock.** The design took every box lock of the user. A box whose first save starts after the sweep lists the boxes would hold none of them, so the lock is a per-user `RWMutex`: saves take it shared after their box lock (lock order: box, user, index), the sweep takes it exclusively and takes no box lock. Restores take only the box lock: they read a kept snapshot, whose chunks the sweep never deletes, and a prune of the same box waits for them.
- **Prune runs in the pass, not after each save.** One switch (`dry_run`, the allowlist) then covers every delete, and a save pays nothing for it. Between passes a box keeps at most one interval of extra snapshots.
- **Kept set** = the rule + the head + pinned snapshots. A Rebase pins its snapshot from its save to the end of its restore; a restore into a missing machine pins the head it restores. A head or pin missing from the timeline stops that user's pass with an error, before any delete.
- **The parent chain.** Records keep `parent`; pruning leaves it pointing at a removed record, and nothing follows it. The api plans each save on the head, which is always kept. The agent reads `parent` only to decide whether its stamps apply (`agent/src/state/mod.rs:573`) and never looks it up; `rec.Parent` is written and never read; the manifest's sealed `parent` is informational. If the sweep removes a chunk the agent still counts as known (only after a commit that failed), the 185e1bf heal plans again without a parent (test `TestASaveAfterPruningPlansOnTheHead`).
- **Timeline build.** A pass that finds no `timeline.json` reads every record of the box before taking the box lock, then reads under the lock only the records committed meanwhile. A dry run builds it in memory and does not write it.
- **Settings:** `state.gc.interval` (absent or 0 turns the pass off), `state.gc.dry_run` (same reads and locks, no write and no delete), every user in the users table but those whose policy sets `-no-gc` (`cocoonbox-api user set`), `state.gc.delete_batch` (default 1000), `state.gc.deleted_retention` (default 168h, 19.6).
- **Log:** one line per user and pass: snapshots pruned (with kept and boxes), chunks deleted of those stored, index entries dropped, and MiB freed split into records, manifests and chunks. A dry run says "would remove". Keys never reach the pass; it never reads the user's state key (until 25.8, whose manifest compaction reads it).
- **Not built yet:** the `state prune` command (a pass outside the api would not hold its locks; the pass at start with `dry_run` serves the rollout). `changed_files`, `ListSnapshots`, `BrowseSnapshot` and `RestorePaths` were built on 2026-10-03 (25.4).

### 25.4 File and folder restore

1. The app shows the timeline (`ListSnapshots`). The user picks a snapshot and browses its tree (`BrowseSnapshot`, one directory per call). A hibernated box is woken first.
2. The api, under the box's lock, checks that the snapshot is in the timeline. It calls the agent's `Restore` with a scope: the paths, and a target folder name.
3. The agent reads the manifest and selects the entries under the paths. Its first answers list the chunks they reference, in pages of 2048. The api checks that each one is in the record, and presigns and pages only those.
4. The agent writes the entries as `user` into `~/Restored/.cb-restore-<id>/` and renames that to `~/Restored/<timestamp>/` at the end, so a failed restore leaves only a leftover that snapshots already exclude.
   - `<timestamp>` is the snapshot's save time; a second restore from the same snapshot gets a `-2` suffix.
   - `<path>` is the box path without its leading slash, for example `~/Restored/2026-10-02T1608Z/workspace/app/main.go`.

What a scoped restore does not do: quiesce, stop the desktop, close Chrome, touch agent.db, or write outside `~/Restored`. A path must lie under one of the agent's roots (`/home/user`, `/workspace`) and contain no `..`, and the target is one folder name. Anything else is refused.

The next save uploads no chunk for the restored files: their chunk ids are keyed hashes under the same user key, so they are already stored.

The agent keeps the decrypted manifest of the last `Browse` in memory for 10 min, so browsing a tree does not fetch it once per directory.

Decided and built 2026-10-03. Where it differs from the plan above:
- **The api reads the manifest, not the agent.** It holds the user's key for every save and restore already, so it opens the manifest itself (`api/boxes/manifest.go`: AES-256-GCM bound to the id, zstd, keyed hash equal to the id, the agent's format) and keeps one directory index per box for 10 min, without chunk lists. Browsing therefore works on a hibernated box, and there is no `State.Browse`.
- **The api picks the chunks.** For `RestorePaths` it selects the manifest's entries under the paths, checks that the record lists every chunk they name, and presigns only those, in pages of 2048 (`boxes.stateLayer.restorePaths` logs the counts). No `TransferProgress.needed` round trip.
- **A separate RPC, `State.RestorePaths`,** carries the scope (`RestoreRequest.scope`). An agent without it answers `UNIMPLEMENTED`, so an old agent never sees a scope it would ignore and run as a full restore; the operation then fails with "this box's agent cannot restore files; upgrade the box first". The agent ships with cbdesk:v20.
- **Lock and pin.** The operation (`PHASE_RESTORING_FILES`, so the edge still serves the box) pins the snapshot from its start to its end. The box lock is held only for the check that the timeline keeps it, which orders the check after a prune already running; a prune that starts later sees the pin. A save of the box waits for the operation, as for any other operation.
- **Agent** (`agent/src/state/mod.rs` `restore_paths`, `select`, `scope_path`; toolhost `state_place`): the target is one folder name that does not start with `.`; each path is absolute, under a root, without `.`, `..` or empty parts, and must name something in the manifest, else `INVALID_ARGUMENT` before anything is written. Entries are staged as `user` in `~/Restored/.cb-restore-<uuid>/<box path without its leading slash>`, with modes and mtimes; directory modes are set last, deepest first. `state_place` then creates `~/Restored/<target>` (or `-2`, `-3`, … up to 100) with `mkdir`, which fails when the name exists, and renames the staging directory onto that empty directory, so it never replaces an entry. The final message carries `TransferProgress.target`. No quiesce, no desktop or Chrome restart, no agent.db write, no `recover`.
- **Changed files** (`PlanResponse.changed_files`): files whose chunk list differs from the last upload's for the same path, plus new and removed paths. Unchanged stamps count as unchanged without a read. Files read through an SQLite backup or changed within 1 s of the walk are now recorded in `known-chunks.json` with `compare_only`, so they are compared but still never reused. Absent when the agent has no stamps for the key (the first plan on a machine, a plan without a parent); the timeline then shows only bytes added.
- **`ListSnapshots`** reads `timeline.json` (built by the GC; a box without it is listed from its records). `changed_files` is a new optional timeline field; an older api drops it from entries it rewrites.

Hot path: none on claim, screen or edge. A save pays one map entry per re-read file for `changed_files`. Browse and RestorePaths are their own calls.

### 25.5 Pieces and contract

- **api:** `timeline.json` in the commit; the prune and sweep pass (built, 25.3); `Delete` in the store; `ListSnapshots`, `BrowseSnapshot` and `RestorePaths` on `Boxes`; a `cocoonbox-api state prune [-user ID] [-now T] [-dry-run]` command for acceptance and for operators.
- **agent:** `PlanResponse.changed_files`; `State.Browse`; the scoped `Restore`.
- **app:** a timeline (time, files changed, bytes added) and a path picker, with Restore; the result opens in the box's file manager.

Sketch; the fields are the contract to refine (as built: no `State.Browse` and no `TransferProgress.needed`; `State.RestorePaths` takes the scope; `TransferProgress.target` = 7; `EntryKind` values `ENTRY_KIND_FILE|DIR|SYMLINK`; `BrowseSnapshotResponse.truncated` past 5000 entries; see 25.4):

```proto
service Boxes {           // clients, through the edge
  rpc ListSnapshots(BoxRef) returns (ListSnapshotsResponse);                 // kept snapshots, newest first
  rpc BrowseSnapshot(BrowseSnapshotRequest) returns (BrowseSnapshotResponse);
  rpc RestorePaths(RestorePathsRequest) returns (Operation);
}
message Snapshot { string id = 1; int64 saved_at_ms = 2; bool quiesced = 3; uint64 files = 4;
                   uint64 logical_bytes = 5; optional uint64 changed_files = 6; uint64 added_bytes = 7; }
message BrowseSnapshotRequest { string box_id = 1; string snapshot_id = 2; string dir = 3; }   // a box path; "" lists the roots
message BrowseSnapshotResponse { repeated SnapshotEntry entries = 1; }
message SnapshotEntry { string path = 1; EntryKind kind = 2; uint64 size = 3; int64 mtime_ms = 4; }
message RestorePathsRequest { string box_id = 1; string snapshot_id = 2; repeated string paths = 3; }

service State {           // api only
  rpc Browse(BrowseRequest) returns (BrowseResponse);
}
message BrowseRequest { StateKey key = 1; PresignedGet manifest = 2; string dir = 3; }
message RestoreScope { repeated string paths = 1; string target = 2; }      // target: one folder name under ~/Restored
// RestoreRequest.scope = 4: unset restores the whole state, as today.
// TransferProgress.needed = 7 (repeated string): a scoped restore's chunk ids, in pages, before any data.
// PlanResponse.changed_files = 6.
```

### 25.6 Acceptance on the bench host

On a scratch user's box, never a real user's:
1. **Timeline.** Five saves, each after writing one known file. `ListSnapshots` lists five, each with 1 changed file and added bytes near that file's size.
2. **File restore.** `RestorePaths` of `/workspace/t/file-2` from the second snapshot writes `~/Restored/<timestamp>/workspace/t/file-2` with the sha256 written before that save.
   - The desktop session and Chrome keep their PIDs.
   - A walk of both roots before and after differs only under `~/Restored`.
   - The api presigned only that file's chunks (its log).
3. **Folder restore** of `/home/user/Documents` works the same way. A path under `/etc`, a path with `..` and a target with `/` are refused.
4. **Prune.** `state prune -now <40 days ahead>` keeps exactly the rule's set. The sweep deletes exactly the chunks referenced only by dropped snapshots (checked against a mark computed offline from the records). A file restore from every kept snapshot still works.
5. **Concurrent save.** A sweep started during a large save of a second box of the same user waits for that save's commit, and the new snapshot restores. A unit test with a fake store forces the interleavings (sweep between index read and commit; delete after release).
6. **Crash.** Killing the api between the index write and the deletes: the next save that needs a garbage chunk uploads it, and the next sweep removes the leftovers.
7. **The first real prune of a long-lived box:** object counts and bytes before and after, and a file restore from the oldest kept snapshot.

### 25.7 Open

- When the prefix of a deleted user (§22.5) goes, and after how long. A deleted box's prefix goes `state.gc.deleted_retention` after the GC finds it (19.6).
- A per-user storage quota. Time Machine drops the oldest snapshots when the disk fills; there is no such bound here yet.
- Slimmer records: built as 25.8. Left open there: the whole `index.json` is still rewritten by every save that adds a chunk.

Hot path: none on the claim, screen or edge path. A commit writes one more small object (`timeline.json`). Prune and the sweep run outside every request; while the sweep holds a user's lock, that user's saves and restores wait. The new RPCs are their own calls.

### 25.8 Slimmer snapshot metadata

Decided and built 2026-10-03. Each record repeated the full chunk list (5.3 MiB on the box of 25.1) and each manifest the full file list (3.2 MiB), so a save wrote about 8.5 MiB of metadata and every kept snapshot stored it again.

- **Delta records, at the save.** A save of a user whose policy sets delta (`cocoonbox-api user set NAME -delta`) writes its record against a base: the newest full snapshot of the box saved on the same UTC day. The record keeps its timeline fields and parent, names the base in `base`, and its `chunks` is `{"added": [...], "removed": [...]}` instead of the list; an api without delta records fails to decode that field instead of reading an empty list. A full record is written for the first save of a UTC day, for a change (chunks added plus removed) larger than 1/4 of the base's chunks, which keeps a delta under about half a full record, and for a box without a timeline; it becomes the next base. The api keeps the base record in memory per box, so a save reads nothing more.
- **Manifest deltas, by the GC.** The agent still uploads its full manifest; nothing changes in the agent, so every agent version works. The prune turns the full manifest of every kept delta snapshot, except the head and pinned ones, into `manifests/<id>.d`: the base's file records as copy runs plus the records the base lacks, verbatim, sealed under the user key with `<id>.d` as associated data. It deletes the full manifest only after the delta rebuilds it byte for byte. A reader rebuilds the exact plaintext and checks its keyed hash against the snapshot id, so integrity is as before. A restore (Rebase, a missing machine, RestorePaths) first writes the full manifest back (`materialize`); the next pass compacts it again. The GC therefore now reads the user's state key, in memory for the pass.
- **Pruning across bases.** Every delta is one step from a full base, never a chain. The timeline names each snapshot's base. When the prune drops a base that one kept snapshot needs, it rewrites that snapshot full (record and manifest) and then deletes the base; a dropped base that several kept snapshots need keeps its objects, out of the timeline. So a kept snapshot always has its base. With one base per UTC day and the daily rule, each past day ends as one full snapshot; the rewrite costs what the dropped base freed.
- **Locks** as in 25.3: a save and a prune of one box serialize on the box lock, so a save never picks a base that a running prune drops, and a prune never sees half a delta.
- **Measured** (`COCOONBOX_MEASURE=1 go test -run TestMetadataBytesPerSave ./boxes`, 100,000 files with one chunk each, 40 changed per save; the fake's chunk sizes make a full record 8.9 MB here against 5.3 MiB on the box of 25.1). Written per save: record 8,900,327 B full against 579 and 735 B as deltas; the manifest (3.9 MB) and `index.json` (6.7 MB) are unchanged at the save. After one GC pass the snapshot before the head keeps 735 B of record and a 2,100 B manifest delta instead of 8.9 + 3.9 MB. Commit CPU in process with 104,000 chunks: 22 ms for the delta with the base in memory against 19.5 ms for marshalling the full record, and the store write falls from 9.2 MB to 6.4 KB.
- **On dev's box** (the bench host, 2026-10-03, about 540 chunks): a full record is 47,297 B and a delta 7,337 B; after a pass a kept delta snapshot stores 7.3 KB of record and a 4.7 KB manifest delta instead of 47.3 + 37.6 KB. A restore from a compacted snapshot through the real agent read the manifest the api wrote back.
- **Still per save:** the full manifest upload until the next pass, and the whole `index.json` (6.7 MB at 100,000 chunks) whenever the save adds a chunk, which a live save nearly always does. The index is now the largest metadata write; an append-only index compacted by the sweep is the next step.
- **Rollback is explicit.** An api without delta records cannot read them, and its prune would delete bases that kept deltas need. Stop the api, run `cocoonbox-api state expand [-user ID]`, which rewrites every delta record and manifest delta as full objects and clears the bases in the timeline, then install the older binary. Checked with the ac0de79 code on layouts written by this code: on the expanded layout it restores the head, saves, and runs a GC pass that deletes exactly the garbage chunks; on an unexpanded one its restore and its sweep fail on the delta record before any chunk delete, but its prune deletes the dropped base, which is why expand comes first.

Hot path: none on claim, screen or edge. A delta save does less store I/O than before and a few ms more CPU; a restore of a compacted snapshot rebuilds and writes one manifest first. Compaction runs in the GC pass.

## 26. Several agents on one desktop

Decided 2026-10-03. Several agents (mode-A loops and MCP sessions) run in one box at once, and today they all drive one desktop. A user saw two agents typing into each other's Chrome tab. Stage 1 gives each agent its own Chrome window on `:0`. Stage 2, private displays per agent, is designed here and built when several agents need non-browser GUI apps at the same time.

### 26.1 The problem (verified 2026-10-03)

- **One tab for every agent.** Every `browser_*` call attaches to `first_page(targets)`, the first tab of the one Chrome (`agent/src/toolhost/browser.rs:369-399`, `:637`). Two agents' `browser_type` calls land in the same tab.
- **One lock for every agent.** `Browser.run` takes one `lock` for every browser call (`browser.rs:227`), and the cua client takes one `input` mutex for every action (`agent/src/toolhost/cua.rs:29`, `:135`, `:197`, `:253`). Calls never interleave, but they take turns on the same tab and the same focus.
- **One desktop.** Xorg `:0` (`image/rootfs/etc/systemd/system/cocoonbox-xorg.service:8`), one VNC port (`cocoonbox-xorg.conf:34`), one cua-driver, one Chrome, one CDP port. `computer_act` on Chrome picks the window by pid, and a window has one active tab.

### 26.2 Stage 1: an own Chrome window per agent

- Each agent gets its own window in the `:0` Chrome (`Target.createTarget` with `newWindow`), keyed by the `agent_id` that every toolhost call already carries (`agent/src/toolhost/rpc.rs:19-24`). `browser_*` calls attach to that window's target, never to `first_page`.
- The lock is per window, not global.
- The window closes when the MCP session closes or the agent is deleted. A window the user closed is opened again on the next call.
- CDP input goes to a target, not to X focus, so agents in different windows no longer type into each other. Logins stay live and shared.

Hot path: one map lookup per browser call; no new process, no memory beyond the window's tab.

### 26.3 The `:0` policy

- `:0` is the user's desktop.
- Browser work stays on `:0`, each agent in its own window (26.2).
- **Until stage 2:** `computer_act` on `:0` works as today. Every agent may use it; the global input mutex keeps one action from interleaving with another, and agents share the focus.
- **With stage 2:** pixel and keyboard work (`computer_act`) on `:0` runs only for an agent the user has handed `:0` to in the app, one agent at a time. The user takes it back in the app. Without the hand-over, `computer_act` on `:0` is refused with a message that names the hand-over; `screenshot` and `computer_state` only read and stay open. The other agents act on their own displays (26.4).
- The hand-over rule ships with stage 2, never before: without a private display to fall back to, it would stop today's single-agent use of `:0`.

### 26.4 Stage 2: a private display per agent (design)

**Display N (N ≥ 1):**
- `cocoonbox-xorg@N.service`: `Xorg :N -seat seatN -novtswitch` with a config per display: the same dummy 1280x800 and the TigerVNC module with `rfbport 5900+N` (localhost, no auth, as on `:0`). Its own MIT cookie, written like `xauth-init`.
  - `-seat seatN` is required. Xorg then takes no VT and ignores input devices without a seat tag. cua-driver's uinput devices have none, so they reach only `:0`. A second Xorg on seat0 would take every cua-driver device, and an agent typing on `:0` would type there too.
- `xfwm4@N` (`--sm-client-disable --compositor=off`). No XFCE session: one `org.xfce.SessionManager` per session bus.
- `cua-driver@N` with its own socket, foreground delivery only. Its background route (an XI2 master pair over uinput) cannot reach a seat≠0 server; foreground is the right route anyway, since the agent owns the focus there.
- No Chrome by default (26.5).
- The polkit rule (`image/rootfs/etc/polkit-1/rules.d/60-cocoonbox-cbagent.rules`) widens to let `cbagent` start and stop `cocoonbox-xorg@*`, `xfwm4@*` and `cua-driver@*`.

**Allocation and release:**
- Keyed by the caller: mode A's agent id `ag-…`, and the MCP session id for MCP calls (`agent/src/mcp.rs:177-196`), which stays the key once owner sessions are no longer tracked as agents (§23.1).
- Allocated on the agent's first `computer_*` call, not when the agent is created: agents that never use the desktop cost nothing.
- Released when the MCP session closes (10 min idle, client DELETE, shutdown; `mcp.rs:219-235`), when the agent is deleted (`agent/src/runner.rs:206-240`), and after 30 min with no `computer_*` call and no viewer attached. Release stops the display's units; what ran on it is gone.
- `serve` owns the agent-to-display table and passes the display on each toolhost call. The toolhost keeps a cua client, an input mutex and the shells' `DISPLAY` per display.
- **No switching.** The display is injected by dispatch like the cua session label (`agent/src/dispatch.rs:132-137`); no tool takes a display argument. `:0` is reached only through the hand-over (26.3).

**Caps and fallback:**
- A per-box cap on private displays, 3 by default on the 4 vCPU / 8 GiB box (`api/machines/sandboxd/state.go:34`), and no new display while MemAvailable is below 1.5 GiB.
- When none is free, `computer_*` fails with "no private display free" and the agents holding one. It never falls back to `:0`, which would bring the collision back. The app can end another agent's display.

**App view and take-over:**
- `ScreenOpen` (`proto/cocoonbox/agent/v1/session.proto:119`) gains `display`; `serve` dials `127.0.0.1:5900+display`. The edge forwards frames as it does today.
- Selecting an agent shows its display. A strip shows `:0` and each held display. `ComputerAction` markers are drawn only on their agent's display.
- Take-over is per display. While the user holds display N, the agent's `computer_act` on N returns "the user has control" (the take-over holds N's input mutex). Clipboard is per X server and per RFB connection already.

**What changes, file by file:**
- image:
  - `cocoonbox-xorg.service:8` and `cocoonbox-xorg.conf:34`: `:0` and 5900 fixed;
  - `xauth-init`: a `:0` cookie only;
  - `cua-driver.service` (user): `DISPLAY=:0`, one socket;
  - `cocoonbox-toolhost.service:13-15`: `DISPLAY=:0` for every child;
  - the polkit rule: `cocoonbox-session.service` only.
- serve:
  - `agent/src/config.rs:12,83`: one `vnc_addr`, used at `agent/src/server.rs:257` and `agent/src/screen.rs:35`;
  - `session.proto:119`: an empty `ScreenOpen`;
  - `server.rs:35,197`: one screen size in `Hello`;
  - `runner.rs:37`: one `screen` mutex for delegated computer tasks;
  - `dispatch.rs:132-137`: injects the cua session label and would inject the display.
- toolhost:
  - `toolhost/mod.rs:38-39,50-55`: one `Browser` and one `Cua`;
  - `config.rs:116-123`: one profile, CDP 9222, one cua socket;
  - `cua.rs:29`: one input mutex;
  - `cua.rs:331`: `display_id: "primary"`;
  - `toolhost/shell.rs:31-37`: no `DISPLAY` per shell.
- app:
  - `macos/Sources/CocoonBoxKit/ScreenSession.swift:26`: an empty `ScreenOpen()`;
  - `macos/Sources/CocoonBox/AppModel.swift:67`: one `ScreenController`;
  - `ScreenController.swift:28-29`: one `takeOver`;
  - `CocoonBoxApp.swift:41`: one Live Screen window.
- state: `config.rs:17`, a fixed desktop unit for Restore. Private displays are not restored; their agents get a new one on the next `computer_*` call.

**Prototype (dev's box, 2026-10-03, nothing installed).** The image already has Xorg, the dummy driver and the TigerVNC module.
- Started on `:1` (`-seat seat1`, VNC 5901): Xorg in about 200 ms, xfwm4, then Chrome on its own profile with CDP 9223, answering in about 1.2 s. RFB on 5901: `framebuffer=1280x800`. No VT taken; the active VT stayed `tty1`.
- XI2 raw-event listeners on both displays during an XTEST click and "hello1" on `:1` and three Shift taps from a kernel uinput keyboard: `:0 RawKeyPress=3` (all from the uinput keyboard), `:1 RawKeyPress=6` (XTEST only); the `:1` page read `hello1`; the uinput keyboard was listed on `:0` and absent on `:1`.
- A second cua-driver bound to `:1`: its `list_windows` showed only the `:1` Chrome and the `:0` driver's only `:0` windows. Foreground typing reached the `:1` page (`hello1fg3`) with `:0 RawKeyPress=0`. Background typing failed: `timed out waiting for X input slave device 'CUA v1… uinput pointer'`.
- Memory, MemAvailable drop from 5,146,944 kB: Xorg 18 MB, xfwm4 19 MB, Chrome with one light tab 237 MB; 308 MiB for the whole stack with cua-driver after use. PSS: Xorg 33–41 MB, xfwm4 27 MB, cua-driver 48 MB, Chrome 311–328 MB (the `:0` Chrome with three tabs: 420–432 MB).
- So a display costs about 90 MiB without Chrome and about 300 MiB with one light tab. Four vCPUs of Chrome rendering bind before memory does.

**Acceptance on the bench host (dev's box, never a real user's):**
1. Two MCP sessions each call `computer_act`. Each gets its own display (`:1`, `:2`); XI2 listeners on `:0`, `:1` and `:2` count each session's keys only on its own display.
2. The app shows each agent's display when it is selected. Taking over `:1` refuses that agent's next `computer_act` and leaves the agent on `:2` running.
3. The fourth concurrent agent's `computer_*` gets "no private display free" with the holders listed; nothing runs on `:0`.
4. Closing an MCP session stops its display's units within a second of the close; MemAvailable returns to within 20 MB of before.
5. Without the hand-over, `computer_act` on `:0` is refused; after it, the chosen agent acts on `:0` and a second agent is still refused.

Hot path: one map lookup per tool call in `serve` and in the toolhost; nothing per frame, the screen relay dials another port. Allocation runs once, at an agent's first `computer_*` call (about 1 s: Xorg 0.2 s, cua-driver 0.75 s). Off the hot path, each held display keeps about 90 MiB resident, which also grows the hibernate image.

### 26.5 The Chrome profile constraint

- **Two Chromes cannot open one profile.** Tested on a scratch profile: a second launch on the same `--user-data-dir`, from another display, exits 0 in 190 ms ("Opening in existing browser session"), opens its tab inside the first browser on the first browser's display, and never opens its CDP port. The main profile holds the same lock (`SingletonLock` → hostname and pid).
- So a Chrome on a private display never shares `:0`'s live logins. Browser work stays on `:0` (26.2, 26.3).
- **Never a copy of the main profile.** Every cookie in the main profile is `v10` (39 of 39, 12 hosts): `--password-store=basic`, a fixed key, so a copy decrypts in any Chrome. A copy would be a full credential copy that then diverges, and the state layer would save it.
- If an agent needs a visual browser on its own display, it gets a profile of its own, kept per display slot (CDP `9222+N`), and starts signed out.

## 27. Offload

Decided 2026-10-03. The contract is contracts.md "Offload".

The user is about to leave the Mac. The task a local coding agent session works on moves into the user's box, where the same kind of CLI, signed in inside the box, keeps working; on return the box's commits and hand-over come home, and the local session continues. This is mode C, beside mode A (the in-box loop) and mode B (a local CLI over MCP).

### 27.1 Model

- **The hand-over is a compaction.** The local agent writes `handover.md` from a fixed template; the box CLI starts a fresh session with it as its first prompt. No transcript moves and no session resumes across machines, so CLI versions, accounts and path rewriting do not matter, and the box agent may be another vendor (`--kind`). The local transcript stays on the Mac, and the box's final hand-over is the local session's next message.
- **The box has its own login.** The user signs the box CLI in inside the box with the vendor's own flow; nothing is copied from the Mac or written back. The login sits under `/home/user`, saved by the state layer under the user's key, like browser sessions (boundaries.md invariant 2 gains this exception when offload ships).
- **Judgement in the agents, mechanics in the packers.** A skill tells each agent when and what to write; `cocoonbox offload` / `back` on the Mac and `cocoonbox-agent offload` in the box do the preflight, the snapshot, the transfer, the delivery and the merge, and enforce the template's headings and the preflight's blocks.
- **Transport:** rsync and git over `cocoonbox rsh` (§13.7). Offload is three Execs: `begin` (box checks, the guard, the directory), rsync of the work tree and `.git` without `objects/`, and `send`, whose stream carries the hand-over, `env.json`, the history as a bundle `--not` the box copy's refs, and opted-in secrets. Back is a wait on the outbox and a `git fetch` over rsh, then a merge against the recorded base S.
- **The inbox** (`cocoonbox-agent offload serve`, a user unit) owns the box CLI's tmux session: it starts it, pastes or queues messages, follows the CLI's hooks, keeps the outbox (`progress`, `question`, `back`, and `blocked` when a usage limit or API error stopped the agent), and restarts a lost session from the newest progress hand-over. The agent turns its changes into `offload` events on `Session.Events`, so the app notifies without polling.

### 27.2 Decisions

| # | Decision |
|---|---|
| D3 | The box CLI runs in bypass-permissions mode by default (`--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`); the offload sheet says so |
| D5, D14 | The work tree minus git-ignored files, and `.git` without `objects/`, by rsync; history by bundle; ignored files are rebuilt in the box |
| D6 | Merge back: fast-forward, else a clean three-way merge applied uncommitted, else a branch |
| D7 | Share guests: covered by §23.6 (a read share reads only `/workspace`); a write share has `shell`, and the UI warns next to "Allow write"; `box_agent_*` tools, if they ship, are owner only |
| D9 | The app shows the box CLI through `Session.Exec` with a PTY running `tmux attach`, in a Ghostty pane |
| D12 | One box CLI session per (repo, kind), named by both; at most one runs per repo (the guard) and 3 per box |
| D13 | Preflight outcomes as in contracts.md: blocks for a box older than v22, missing box tools, a signed-out box CLI, unreturned work, a full box, a non-git directory and macOS-only commands; warnings for local services, VPN-only hosts, secrets, LFS and submodules |
| D15 | No push from the box until M5.2; commits come home with back |
| D18 | The copy's git-ignored directories are left out of state saves; the box packer lists them at each offload |
| D20 | Hibernate and upgrade stay allowed; the app and the CLI warn first while a box agent works |
| D21, D22 | The box CLI's own login; the hand-over as a compaction |
| D23 | The inbox reached through rsh is the agent-to-agent channel; MCP `box_agent_send` / `box_agent_read` wait until after v1 |
| D24 | Opted-in secrets live on tmpfs and never reach the state store; a node-local hibernate image carries them, accepted like the box login, and `end` wipes them |
| D25 | `--kind` may pick another vendor for the box; the default is the local kind |
| D26 | The image carries no box CLI. When the preflight finds the CLI missing, it offers "Install in box", which runs the vendor's official installer over rsh into `/home/user`, so the install persists through the state layer; the user then signs in and updates it like any program |

### 27.3 Facts from O0 that shaped the contract

- A fresh box agent finished a half-done Go and a half-done Rust task from the template hand-over (graders 25/25 and 15/15). A 4-line summary also finished them, but lost a user preference and wrote a thin hand-over back, so the template is enforced and inlined into the box preamble.
- Most of a 9–13 s offload was five sequential rsh startups at about 1.3 s each; hence three Execs.
- A `git gc` on the Mac made rsync resend 91% of a repo; hence history by bundle.
- Box agents rediscovered the proxy variables and the missing C toolchain; hence those facts in the preamble, `build-essential` in v21, and exact Mac versions through mise.
- A usage limit ended a turn silently; hence the `blocked` entry.
- tmux refused `xterm-kitty` and `xterm-ghostty`; v21 uploads the terminal's terminfo with Exec.

### 27.4 Entry points

Decided 2026-10-03. The app's own local agents (the agent CLIs it launches) drive boxes over MCP; their work target is already the box, so their pane has no "Move to box" or "Bring back". Offload is for the user's own terminal sessions: a `claude` or `codex` TUI in any repo.

- **`claude`:** `/cocoonbox:offload` and `/cocoonbox:back` from the cocoonbox plugin. The skill runs the CLI at the path the plugin was generated with (`~/.local/bin/cocoonbox` when it links to this binary, else the app's `Contents/Helpers/cocoonbox`) in the user's working directory, and assumes no app session. One-time install: `cocoonbox setup claude`, or the app's File menu plugin install item (also in the Offloaded section's menu), which runs it with the login shell's environment and shows its output. It is idempotent and also updates the plugin.
- **`codex` and others:** `cocoonbox offload --handover-prompt [--kind K]` prints the request to paste: write the hand-over to this repo's hand-over path with the template, then run `cocoonbox offload --kind K` (both outside the CLI's sandbox, so it asks for approval), and report blocks and warnings. `cocoonbox offload` and `cocoonbox back` also work by hand.
- **The app's Offloaded section** lists the offloads in flight across boxes from the Mac's records: repo, box, box CLI, status (working, idle, waiting for you, blocked, stopped, or the box's state when it is not running; a hibernated box is not woken to ask) and the time since the last activity (the newest of the last hook, the transcript's mtime and the tmux window's activity, else an event's arrival). Selecting one attaches the box CLI's terminal, with Reattach. The app only watches (decided 2026-10-04): offload and back start from the user's own TUI (`/cocoonbox:back` or `cocoonbox back`), and the app has no Bring back. A row stays until `back` or `end` removes its record, as the CLI keeps records; the pane shows one line of help, "Bring this back from your terminal: /cocoonbox:back (or cocoonbox back) in <repo path>", with a Copy button for the command. The section's menu opens "Install in box" and "Sign in in the box" for any box and CLI; a CLI preflight block on the box CLI names that menu. Sign in runs the CLI's own login with `BROWSER=true NO_OPEN_BROWSER=1`, so the box's browser stays shut and the user approves on the Mac: the app opens the first https link the CLI prints on that vendor's sign-in hosts (`claude.ai`, `claude.com`, `anthropic.com`; `auth.openai.com`, `chatgpt.com`; `cursor.com`) in the Mac's browser, once, and opens no other link from the box's output. ⌘V and Edit > Paste in a box terminal paste the Mac's text, bracketed when the program asks. Notifications for a question, back and blocked are unchanged.

Hot path: none. The edge and the api do not change; the inbox is idle between messages; the event is one bus publish per outbox entry or status change. The Offloaded section asks each running box with a record for `offload status` (one Exec) when the app connects, comes to the front or selects a row, never on a timer.

Order: O2 agent and image (the box packer, the inbox and its unit, the events, the tmpfs mount, the state excludes), O3 macOS (the packer, the skill, the app), O4 end to end on the bench.

## 28. Flows follow the user

Decided 2026-10-09. A flow saved in one cbcore box replays in every cbcore box of the same user. The user can export flows to a file and import them again, so a client app can carry a working flow to another box, and later to another computer. Today a flow lives only in the box that saved it: `gui_plan` writes `/workspace/flows/<name>.json` and agents write scripts as `/workspace/flows/<name>.mts`.

### 28.1 Shape

```
box (cbcore)                      api                                 Mac
toolhost: /workspace/flows  ◄──►  serve: Syncer ──box door──►  flows.Library ◄── cocoonbox.control.v1.Flows ◄── cocoonbox flows
          (files, INDEX)          agent.db flow_sync          (state store, <uid>/flows/)         (user token)
```

- **Library:** one per user, in the state store under `<uid>/flows/`. It holds an index and one object per flow, each sealed with AES-256-GCM under `HKDF-SHA256(user state key, "cocoonbox flows")`. The api reads and writes them; boxes never get store URLs for them. Deleting a user removes them with the rest of `<uid>/`, and the state GC sweeps only `boxes/` and `chunks/`.
- **Box ↔ api:** the box door (§16.3, `connect.box_listen`), which already names the machine and its owner by the door key that the egress proxy injects. The door gains `/flows` beside Connect's MCP, so no pool rule, key or port changes. The agent finds it at `CONNECT_URL` with the path `/flows`.
- **Mac ↔ api:** a new gRPC service, `cocoonbox.control.v1.Flows`, for the owner's user token. A share secret is no gRPC token at the edge.
- **Agent:** `serve` owns the sync (network and `agent.db`). The toolhost owns the files: `flows_scan`, `flows_read`, `flows_write`, `flows_remove`, `flows_mark` and `flows_index`. These are toolhost calls, not tools. Only a cbcore box with `CONNECT_URL` syncs; a cbdesk box runs none of it.
- **Nothing new on MCP.** On cbcore, `gui_plan {flows: true}` and the guide resource's "Saved flows" section already list `/workspace/flows`, which now holds the library. The cbdesk surface stays byte-identical.

### 28.2 Sync

- **Rows:** `flow_sync(name, rev, sha256)` in `agent.db` records, per flow, the rev and content hash that the box and the library last agreed on. `agent.db` and `/workspace` are saved and restored together, so after a restore or a Rebase the rows still describe the files on disk.
- **Pull:** runs before `gui_plan` with `flow`, `flows` or `find`, before `gui_flows` (the guide resource), before `script_run` of a script under `flows/`, and once when `serve` starts. It gets the listing and, for each library flow newer than the box's row:
  - copies it in when the box lacks the file or the file is unchanged since the row;
  - records the row when the file already equals it;
  - leaves a changed local file alone, so the push sends it.

  For a flow the library dropped, it removes the box's unchanged copy and forgets the row. When a `.json` file changed, it rebuilds `INDEX` with `gui_plan {reindex: true}`, which reuses the executor's own index format and commits to the flows git. A pull is bounded at 3 s; on a failure the box keeps its files.
- **Push:** runs in the background 500 ms after `gui_plan`, `script_run`, `write_file`, `edit_file`, `shell` or `upload`, and 500 ms after any other write of a flow file, which the toolhost sees through inotify on `/workspace/flows` (a desktop app's save, `rsh`), following the folder when a restore replaces it. It scans the folder and PUTs each file whose hash differs from its row. Content that the library refuses (a bad name, too large, a full library) is not sent again until it changes. The same watch sees a flow deleted or moved out, and the toolhost then drops it from `INDEX` in place, so `gui_plan {flows: true}` lists what is on disk.
- **Which files:** regular files directly in `/workspace/flows` named `[A-Za-z0-9][A-Za-z0-9._-]*` (at most 96 characters) and ending in `.json` or `.mts`, at most 128 KiB each, UTF-8. `INDEX`, `.runs.jsonl`, `.pending.json` and the git folder stay local.

### 28.3 Decisions

| Question | Decision |
|---|---|
| Conflicts | Last write wins, in the order the api receives writes. Every write takes the library's next `rev`, and a write of identical content changes nothing. A box never overwrites a local edit with a pull; its push then wins. The losing version stays in the git history of the box that had it (`gui_plan` commits every save, the pull commits after it writes). Per-flow merge or CAS buys nothing for a single-author GUI script. |
| Limits | 128 KiB per flow, 200 flows and 3 MiB per user, so one Export answer fits gRPC's 4 MiB default. A flow is typically 2–10 KiB. Over a limit: HTTP 413 at the door, `RESOURCE_EXHAUSTED` on gRPC. |
| Another app version | Each library entry records the box and the image that wrote it last (`box_id`, `image`), and `cocoonbox flows list` shows them. Replay already resolves each step by its saved fingerprint, then by name, then by a decision; a step that still fails escalates with `escalate.reason`, the step and the screen it saw, as today. A replay that passes with re-resolved steps saves the flow, and the push carries the new fingerprints and image to the library. A flow therefore follows the image it last passed on. |
| Deletion | `cocoonbox flows rm NAME` (gRPC `Delete`) deletes from the library; every box removes its unchanged copy at its next pull. Deleting the file in a box stays local: the box does not pull it again until the library has a newer version. This way an agent that clears `/workspace` cannot wipe the library. Removing the whole folder is a lost folder, not a delete of each flow: the rows hold only for the folder they were agreed in (`.sync-id`), so a new folder gets every library flow it lacks, as a new box does. |
| Privacy | Flows hold typed values and params, so the library is owner-only: the door acts only for the machine's owner, `Flows` only for the user token, and objects are sealed with the user's key. A read share (§23.6) is refused `/workspace/flows` on a cbcore box (`read_file`, `list_dir`, symlinks included) and gets no "Saved flows" section in the guide. A write share has `shell`, which §23.1 already treats as handing over the box, so it can read the flows on that box like any other file. The share sheet's disclosure should name saved flows (app follow-up). |

### 28.4 Hot path

- cbdesk: zero.
- cbcore: one door GET of the listing before a call that reads saved flows (on the order of milliseconds; the listing is at most 200 short rows), plus the content of each newer flow and one local `flows_mark` read.
- Pushes run off the call path; the call itself pays one `Notify`.
- The api adds one door-key lookup per sync request, as Connect's door already does, and nothing on the claim path.

### 28.5 Export file

`cocoonbox flows export [NAME...] [-o FILE]` writes JSON:

```json
{"version": 1, "exported_at_ms": 0, "flows": [{"name": "mousepad-save-fruit.json", "content": "<text>", "sha256": "<hex>", "rev": 3, "saved_at_ms": 0, "box_id": "cb-…", "image": "cbcore:v1"}]}
```

`cocoonbox flows import FILE` (`-` for stdin) sends `name`, `content` and `image` for each flow. The rest of each entry is informational.

### 28.6 Rollout

- The api change and the agent change are independent.
- An agent without the door's `/flows` (an old api) logs a failed pull and keeps its local flows.
- An old agent never calls the door.
- An image needs the new agent and `gui_plan.mts` (for `reindex`); a cbcore cut carries both.
