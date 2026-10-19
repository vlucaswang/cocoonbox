# Boundaries

cocoonbox follows the layering of cocoon and projecteru2/core: components never couple sideways, and each one depends only on the layer below it through a published interface.

## Horizontal: zero coupling

The four components share nothing except published contracts.

| Component | Language | Talks to others only through |
|---|---|---|
| `macos/` app | Swift | `proto/` gRPC via the edge; RFB bytes inside `Session.Screen` |
| `api/` control plane + edge | Go | `proto/` (serves Boxes, proxies agent RPCs and MCP as opaque bytes) |
| `agent/` in-box agent | Rust | `proto/` gRPC and the MCP contract, served on 7443 |
| `image/` guest | shell | `docs/contracts.md` (users, sockets, ports, env) |

Rules:
- No component imports another component's code, reads its config or state, or shares a database, file or socket with it.
- `shuttle-desk-core` (loomhand/shuttle `crates/desk-core`, a git dependency pinned to a shuttle tag) is the one shared library: the toolhost wire and the desk call arguments of [shuttle's desk wire](https://github.com/loomhand/shuttle/blob/main/docs/wire.md), which shuttle serves and the toolhost relays (contracts.md, Toolhost wire). It holds contract code only.
- The edge never interprets agent payloads: gRPC frames and MCP HTTP exchanges pass through unchanged; it knows only auth, ownership and where a box lives. One deliberate exception is a trust point that must not rest on what a box reports, since `user` has sudo and could forge it: the share page bridge parses the RFB stream and drops what a viewer may not send or get, so it stays view-only and never gets the box's clipboard (`api/edge/screen.go`, `api/edge/rfb.go`).
- The agent never knows how its machine is scheduled (Kubernetes, sandboxd, a laptop) or who the user is beyond the principal in the edge credential (the owner, or a share and its guest's user id).
- The app never knows pod IPs, VNC ports or guest users; it sees box ids, gRPC services and an RFB byte stream.
- A contract change lands on `main` first (`proto/`, `docs/contracts.md`), then each component adopts it.

## Vertical: one interface per layer

Each component is a stack; a layer calls only the interface of the layer directly below.

**api**

```
gRPC services (Boxes) · edge (gRPC + MCP proxy)
        │ machines.Machines: lifecycle (Ensure / Get / List / Hibernate / Delete / Watch / Changes),
        │   Address, SetMeta, Deliver, SetCredentials, SetEgress
        │ machines.Tenants: PutTenant / DeleteTenant
        ▼
backend: sandboxd (claims and tenants on the cbsbx instance, one client)
```

The service layer owns users, ownership, operations and their event logs, and names every key it installs. A backend owns only machine lifecycle and tenants and returns an address; it gets secrets as named values to install and never sees proto types or which product credential a value is. The edge depends on an address resolver, never on a backend.

**agent**

```
transports: gRPC (Agents, Session) · MCP (/mcp)
        │ both call the same tool dispatcher; neither implements tools
        ▼
loop (mode A only) ── Model interface (chat wire → gw URL)
        │ Tools interface
        ▼
toolhost RPC (unix socket, user `user`)
        ▼
executors: shell · files · CDP browser · cua-driver
```

Mode A and mode B differ only at the top layer. Nothing below the dispatcher knows which mode issued a call.

**app**

```
SwiftUI views → CocoonBoxKit (gRPC client, connection, events) → swift-rfb (RFB over any byte stream)
```

swift-rfb is a separate MIT library with no knowledge of cocoonbox.

**infra**

cocoonbox consumes cocoonstack mechanisms (cocoon, sandboxd, gw) through their public APIs and CLIs only. Anything cocoonbox-specific (users, tiers, idle policy, credentials) stays in cocoonbox; a missing primitive goes upstream as a mechanism, never as cocoonbox logic.

## Authorization and credentials

Two invariants:

1. **No user can reach another user's machine.** Every path into a box (agent gRPC, MCP, the screen, state transfer, lifecycle calls) is authorized per user against box ownership at the edge or api, and nothing inside one box can reach another box: no shared secret that opens every box, no box-to-box network path, no storage prefix readable across users.
2. **Credentials stay as local as possible, never at rest in the VM.** Order of preference: on the user's Mac (Keychain) → host side, outside the VM (injected at egress, never readable back) → inside the VM only in memory for one request. A credential written to the VM disk ends up in its snapshots, checkpoints and state uploads, so it is treated as leaked to every future copy of that box.

Consequences:
- Mode B keeps the model subscription on the Mac; the box only sees MCP tool calls.
- Box-to-edge auth uses a per-box secret, never one secret shared by all boxes.
- Model and connector keys reach outbound requests through egress injection (sandbox per-claim env with `guest: false`) or a broker on the Mac, not through a file in the VM.
- Browser sessions are the one unavoidable exception (they live where the browser runs); they are encrypted with the user's key in the state layer.

**`user` has passwordless sudo since desktop v10.** The in-box split between `user`, `cbagent` and root no longer protects anything from the desktop user or from the tools agents run as `user`: they can read `agent.env`, `/var/lib/cocoonbox` (agent.db) and the identity file, and replace the agent binary. Both invariants still hold, because neither relies on that split:
- No cross-box path: box isolation lives on the host (the sandboxd none lane: no NIC, egress only through the proxy allowlist, `CB-OUTPUT`). Root in the guest can rewrite the guest firewall or the identity file, which affects only its own box. The identity file holds only the edge's public key, and the edge never sends one box's credential to another box.
- No credential at rest: the host proxy injects the gw key and `agent.env` holds the placeholder. The state key and the presigned URLs arrive per request and cover only the box's own state.
- Residue: up to v11, clones of one golden share its ssh host keys (bench.md, Known gaps), readable by `user`; using them needs a path to another box's traffic, which the host blocks. v12 has no sshd and no host keys.
