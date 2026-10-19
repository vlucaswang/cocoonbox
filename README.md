<picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/loomhand-logo-horizontal.svg"><img src="docs/assets/loomhand-logo-mono-navy.svg" alt="loomhand" width="440"></picture>

# cocoonbox

A persistent cloud computer per user, with a resident agent that any model can drive and a native macOS client to watch it, talk to it, and take over. Local coding agents use the same box through MCP.

Each box is a Linux desktop (Ubuntu 24.04, XFCE) in a [cocoon](https://github.com/cocoonstack/cocoon) microVM, claimed from [sandbox](https://github.com/cocoonstack/sandbox) warm pools, hibernated when idle and woken in about a second. Model calls go through [gateway](https://github.com/cocoonstack/gateway) (`gw`).

```
CocoonBox.app / cocoonbox CLI / local agents (MCP)
        │  gRPC over HTTP/2, MCP Streamable HTTP
        ▼
cocoonbox-api ── control plane: users, boxes, sharing, Connect, vault, state layer
        │        edge: ownership check, per-box credential, frame-by-frame proxy
        │  claims, wake, hibernate, port relay
        ▼
sandboxd (cocoonstack/sandbox) ──► cocoon microVM: the box
                                     ├─ cocoonbox-agent serve: agent loop, gRPC Agents/Session, /mcp
                                     ├─ cocoonbox-agent toolhost: shell, files, CDP browser,
                                     │    cua-driver (loomhand/cua), desk AT-SPI runtime
                                     └─ Xorg + XFCE desktop ── RFB ──► the app (loomhand/swift-rfb)
model calls ──► gw (cocoonstack/gateway)
```

**Documentation: [loomhand.github.io/cocoonbox](https://loomhand.github.io/cocoonbox/)** (design, contracts, layering, measurements), source in [`docs/`](docs/).

## Layout

| Path | What |
|---|---|
| `proto/` | gRPC contracts shared by every component |
| `agent/` | `cocoonbox-agent`, the in-box agent (Rust) |
| `api/` | `cocoonbox-api`, control plane and edge (Go) |
| `macos/` | `CocoonBox.app`, the `cocoonbox` CLI and its stdio MCP bridge (Swift) |
| `image/` | guest image provisioning and the template cut |
| `deploy/` | host, sandboxd instance and acceptance configuration |
| `scripts/` | repository gates |
| `docs/` | design, contracts, boundaries, measurements |

## Build and test

```bash
make help            # every target
make api             # api: build, vet, lint, test
make agent           # agent: fmt check, clippy -D warnings, test (CB_NO_BROWSER=1 skips the Chrome tests)
make macos           # macOS app: build and the swift-testing suite (macOS only)
make image           # syntax-check the image scripts
make gate            # clean gate over the tracked files and the dependencies
```

The agent targets Linux; on macOS run its gates in the pinned toolchain image:

```bash
docker run --rm -v "$PWD":/w -w /w/agent -e CB_NO_BROWSER=1 rust:1.98.0 \
  sh -c 'cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked'
```

The root `Package.swift` exports `CocoonBoxKit` and `CocoonBoxTerminal` for packages that build on them: `.package(url: "https://github.com/loomhand/cocoonbox.git", from: "<version>")`. Component details are in [agent/README.md](agent/README.md), [api/README.md](api/README.md) and [macos/README.md](macos/README.md).

## Run

- **api:** `api/deploy/api.yaml.example` lists every key; `api/deploy/cocoonbox-api.service` runs it under systemd behind a reverse proxy (`deploy/host/Caddyfile.cocoonbox`).
- **sandboxd instance and host:** `deploy/cbsbx/` writes the instance config and units; `deploy/host/` holds the host bridge, gw config, state store, apt cache and backups.
- **image:** `image/cut.sh` cuts a desktop template from the previous one on the sandboxd host and imports it; versions, pins and publishing are in [docs/image.md](docs/image.md).
- **app:** `CB_ENDPOINT=https://<api host> macos/scripts/bundle.sh` builds `dist/CocoonBox.app`.

## Documentation

- [Design](docs/design.md)
- [Contracts](docs/contracts.md)
- [Boundaries](docs/boundaries.md)
- [Box image](docs/image.md)
- [Measurements and lessons](docs/bench.md)

## Related projects

| Project | Role |
|---|---|
| [cocoonstack/cocoon](https://github.com/cocoonstack/cocoon) | the microVM engine under every box |
| [cocoonstack/sandbox](https://github.com/cocoonstack/sandbox) | sandboxd: warm pools, claims, hibernate and wake, egress proxy |
| [cocoonstack/gateway](https://github.com/cocoonstack/gateway) | gw: the LLM gateway behind every model call |
| [loomhand/cua](https://github.com/loomhand/cua) | cua-driver with the Linux X11 fixes the box runs |
| [loomhand/swift-rfb](https://github.com/loomhand/swift-rfb) | the RFB client and Metal renderer of the app's live view |

## License

[AGPL-3.0](LICENSE)
