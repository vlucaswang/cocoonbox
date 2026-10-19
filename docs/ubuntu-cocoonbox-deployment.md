# Ubuntu cocoonbox deployment

Installed and verified on 2026-10-18. The prior Fedora assessment in
`openclaw-deployment.md` is historical; the host has been reinstalled.

## Access

- SSH: `ssh cocoonbox` (`lucas@192.168.10.147`), passwordless sudo.
- Console: `https://cocoonbox.tailb695d.ts.net/cb/`, reachable inside the tailnet.
- Console username: `lucas` (initial admin).
- Retrieve the generated password in your own terminal:

  ```sh
  ssh cocoonbox sudo cat /etc/cocoonbox/initial-admin-password
  ```

  Change it in the console after signing in. Do not paste it into chat.
- Personal API token: root-only `/etc/cocoonbox/initial-api-token`.
  Regenerating the personal token in the console invalidates this initial token.
- Desktop: `main`, box ID `cb-b023b84c576f`, image `cbdeepseek:v1`.
- The native macOS app can use the same HTTPS origin; browser desktop sharing
  remains view-only in the shipped application.

## Traffic and TLS

```text
Browser / native app on tailnet
  -> Tailscale Serve :443 (valid, automatically managed certificate)
  -> Caddy 127.0.0.1:18202 (HTTP/1.1 and h2c)
  -> API gRPC 127.0.0.1:18200 / console and MCP 127.0.0.1:18201

API / guest's sandbox egress proxy
  -> Caddy 127.0.0.1:19000 or 10.203.0.1:19000
  -> HTTPS 192.168.10.250:8010 (NAS certificate verification disabled)
  -> QuObjects bucket cocoonbox-state
```

NAS verification is disabled only for that Caddy upstream, at the operator's
explicit request. The NAS presents QNAP's default self-signed certificate,
without hostname/IP SANs. The exception does not affect the web certificate.
The internal S3 HTTP listener binds only loopback and `10.203.0.1`; firewall
rules block access from the host's LAN uplink. The S3 Host header, including
its port, must be preserved for signature verification.

Tailscale Serve handles web TLS independently of Secure MCP Tunnel. The
tunnel is an outbound HTTPS connection to OpenAI; it does not publish a web
console or provide its browser certificate.

## Installed components

| Component | Version / configuration |
| --- | --- |
| OS | Ubuntu 24.04.5 LTS, x86_64 |
| Storage | ext4 root; XFS `/srv`, `reflink=1`, `prjquota` |
| Cocoon | v0.7.2 |
| sandboxd / silkd | v0.1.17 |
| Cloud Hypervisor | cocoonstack fork, v55.0.0, dev build commit `0b29f399` |
| Firecracker | cocoonstack fork, v1.18.0-dev, commit `23fb009c` |
| EROFS utilities | v1.9.4, built from commit `f36cadb5c563995ab3aa8572a60ed6b721b9557d` |
| API | Cocoonbox release v0.1.8 |
| Gateway | v0.3.0, PostgreSQL storage, direct DeepSeek Platform provider |
| PostgreSQL | Ubuntu PostgreSQL 16, Unix sockets and peer authentication |
| Caddy | Ubuntu package 2.6.2, loopback web router and internal NAS adapter |
| Tailscale | 1.104.1; Serve HTTPS 443; Funnel not enabled |
| Secure MCP Tunnel client | v0.0.16 installed; no live OpenAI tunnel configured |

Release binaries and the desktop rootfs were checksum-verified before use.
The original published desktop layer digest is
`sha256:34545039e6f89135031d329eb5042fb318593d7526274d939a5743f49f81980e`;
its published agent revision is `cb07383`. The tested API and guest are from
different revisions; the runtime checks below establish the exercised paths,
not compatibility of every possible feature.

## Service and data configuration

- Services enabled: `cb-host-net`, `cbsbx-cocoond`, `cbsbx-sandboxd`,
  `gw`, `cocoonbox-api`, `caddy`, `tailscaled`, and PostgreSQL.
- `cbsbx` quota: 200 GiB; apt cache quota: 20 GiB.
- Guest: 4 vCPUs, 8 GiB RAM, 40 GiB logical disk; warm target 0.
- State endpoint: `127.0.0.1:19000`; presigned guest endpoint:
  `10.203.0.1:19000`; region `us-east-1`; `secure: false` refers to this
  internal adapter hop. The adapter uses HTTPS to the NAS.
- State saves: every 15 minutes. Parking is disabled. User idle hibernate: 2 h.
- Single API process; shared-cluster and conditional-write modes are disabled.
- API secrets/configuration: `/etc/cocoonbox`, root-only, delivered to the API
  through systemd credentials. NAS credentials are in `store.env`.
- Gateway secrets: `/etc/gw/env`; `DEEPSEEK_API_KEY` authenticates directly to
  DeepSeek Platform. No OpenRouter or Anthropic credential is required.
- Root egress CA signing material: `/root/cocoonbox-egress-root`; sandboxd
  receives only the root certificate and its node intermediate/key in
  `/etc/cocoonbox/egress-ca`. Back up the root key offline.
- Database backup timer: `cb-pg-dump.timer`; disk alert and apt cache timers
  are enabled. Local database dumps do not replace an off-host backup of the
  database and master/signing keys. No off-host database/key backup was set up.
- Staging files and operator diagnostic scripts:
  `/home/lucas/cocoonbox-setup`. These scripts read root-only credentials and
  do not print credential values. Initial account creation is a one-time step.
- `/etc/fstab.pre-cocoonbox` and `/etc/caddy/Caddyfile.pre-cocoonbox` preserve
  the original files. No filesystem was formatted during setup.

The repository's quota scripts now discover the actual backing mount point
with `findmnt`, allowing `/srv/cb` to be a directory on the `/srv` XFS mount.
Networking/config generation also permits the internal S3 adapter port 19000.

## Verified behavior

- Cocoon doctor: 34 passes, zero warnings/failures after dependency installation.
- Console HTTP 200, valid Tailscale TLS from both the Mac and Ubuntu host.
- Actual password sign-in over HTTPS, authenticated user response, Secure cookie.
- Authenticated native gRPC `Boxes/List` and streamed `Boxes/Watch` through
  Tailscale and Caddy. Caddy explicitly accepts h2c from Tailscale Serve.
- Authenticated control MCP, box boot, shell command, desktop screenshot and
  accessibility tree. Guest Xorg/session/agent/toolhost all active.
- Guest HTTPS egress returned HTTP 200 with TLS verification successful.
- Hibernate and wake, followed by a successful desktop screenshot.
- NAS PUT/GET/HEAD/ListObjectsV2, presigned GET/PUT and bulk delete. The temporary
  compatibility-test object was removed.
- Real state save to NAS. Rebase onto a replacement VM restored 33 chunks;
  a workspace marker survived, proving guest download/restore from NAS.
  The marker was then removed from the live workspace.

Services are enabled for boot, but a full host reboot was not performed.

## ChatGPT Secure MCP Tunnel next step

The client is installed, but a tunnel ID, associated OpenAI organization /
ChatGPT workspace, and runtime API key are still needed before starting it.
No ChatGPT tool call through OpenAI has been verified yet. The runtime key
should have tunnel Read + Use permissions; it is separate from an LLM gateway
provider key.

The desktop tools live at `/cocoonbox.mcp/cb-b023b84c576f`; lifecycle tools
are a separate endpoint at `/cocoonbox.mcp/control`. A hibernated box needs
the lifecycle endpoint's `box_ensure` before desktop tools can reach it.
The tunnel integration must account for both endpoints rather than connect
only lifecycle tools and claim desktop operation is working.

Authentication also needs an explicit choice. OAuth discovery can travel
through Secure MCP Tunnel, but the authorization server is not automatically
tunneled. A tailnet-only OAuth origin needs verification for the entire
authorization/token flow. The installed client alternatively supports static
MCP headers via secret references (`--mcp.extra-headers`); that can authenticate
the tunnel's local hop using a suitably scoped Cocoonbox credential while
OpenAI's tunnel controls authenticate access to the tunnel. Do not expose an
unauthenticated public MCP endpoint.

Reference: https://developers.openai.com/api/docs/guides/secure-mcp-tunnels

### Tunnel profile prepared on 2026-10-18

Tunnel ID: `tunnel_6ad4b528f2548191b379fd2bf2c1455c`.
`/etc/cocoonbox-tunnel.yaml` now maps only channel `main` to isabel's desktop
endpoint `/cocoonbox.mcp/cb-24f8650800cd`. The old account-wide control channel
was removed when the user requested isolation. Sources are
`deploy/host/tunnel.cocoonbox.yaml.example` and
`deploy/host/cocoonbox-tunnel.service`. The service is installed and enabled,
and skips startup unless a nonempty runtime key exists at
`/etc/cocoonbox/tunnel-api-key`. The user supplied the runtime key locally;
its mode is root-only 0600. The service started successfully on 2026-10-18
at 12:13 UTC, authenticated to OpenAI, and fetched tunnel metadata (name:
`cocoonbox`). `/healthz` returned `live` and `/readyz` returned `ready`.
The main MCP startup probe initialized `cocoonbox-agent` successfully.
A direct local `computer_state` call also returned desktop state and an image.
The user subsequently confirmed a successful ChatGPT desktop call against
the original main box. After switching to isabel, another ChatGPT call is
needed to verify the new routing from the existing plugin.

Save only the OpenAI runtime key in that file using `sudoedit`, restrict it to
root with `chmod 600`, then start `cocoonbox-tunnel`. The key requires Tunnels
Read + Use permissions in the owning Platform organization. It is independent
of DeepSeek model access. systemd credentials provide both this key and the
dedicated user's Cocoonbox token to a dynamic unprivileged service identity.
The token authenticates the loopback MCP hop; no secret is embedded in the
profile or process arguments. The health/admin listener is loopback-only at
`127.0.0.1:18203`. No inbound firewall port or Tailscale Funnel is added.

Before calling this ready, verify `/readyz`, actual tunnel connectivity, and a
real ChatGPT tool call. In ChatGPT's custom MCP plugin setup, choose Tunnel
and this ID; no additional OAuth is configured because the client injects the
local bearer credential. Associate the tunnel with the intended ChatGPT
workspace in Platform settings. Everyone allowed to use this connection acts
as the configured Cocoonbox user: do not share it broadly.

The channel is desktop-only and cannot wake a hibernated box. A scoped bridge
or separate lifecycle connection would be needed for unattended wake/sleep
use. Hibernation remains enabled.

### Dedicated isabel desktop

Created on 2026-10-18 at the user's request:

- Box name `isabel`, ID `cb-24f8650800cd`, image `cbdeepseek:v1`.
- Owner is a dedicated non-admin service user `chatgpt-isabel`, ID
  `u-7477f64c1a97`, with max boxes/cap/running all 1.
- An existing, unrelated user named `isabel` was found and left unchanged.
- Service credential `/etc/cocoonbox/isabel-api-token` replaces the original
  main user's token in systemd LoadCredential. The tunnel process no longer
  receives the main user's token. Password/token/metadata files are root-only.
- `setup-isabel.py` creates the dedicated identity idempotently;
  `check-isabel.py` verifies desktop access and rejects main-box access.
- Live tests: isabel ensured running, 19 tools discovered, screenshot returned
  actual image content, main desktop and main lifecycle access denied using
  the isabel credential. Main box remained running and unchanged.
- Prior profile and service unit retained as `.before-isabel` backups. These
  backups are for operator recovery and must not be re-enabled casually.

This is a dedicated persistent desktop for the current tunnel, not automatic
per-conversation isolation. All ChatGPT conversations using this plugin share
isabel. The user plans a separate tunnel for a different box later. Each future
tunnel should get its own profile, service, health port and scoped identity.

## Direct DeepSeek models

Configured and verified on 2026-10-18:

```text
MODEL_MAIN=deepseek-flash
MODEL_COMPUTER=deepseek-flash
```

The gateway uses its `deepseek` preset at `https://api.deepseek.com`, with
`DEEPSEEK_API_KEY` referenced from the root-only gateway environment file.
The model allowlist is `deepseek-flash` for the `cocoonbox-boxes` tenant.
Existing user keys, database state and daily caps were preserved.

The deployment seed is `deploy/host/gateway.deepseek.yaml.example`. The
configuration was published to the gateway's PostgreSQL config store as well
as installed in `/etc/gw/gateway.yaml`; editing the seed file alone does not
replace a nonempty live config store. The previous seed is backed up at
`/etc/gw/gateway.yaml.before-deepseek`.

Gateway token accounting uses the published peak list rates ($0.30 / $1.20
per million input/output tokens, cache reads at 2% of input). DeepSeek's
off-peak charges are lower; this ledger is therefore not an exact invoice
during off-peak periods. Reference checked 2026-10-18:
https://api-docs.deepseek.com/quick_start/pricing/

The custom image `cbdeepseek:v1` was cut from `cbcore:v23` with both model
defaults embedded, without embedding the provider key. Its imported image
digest is `sha256:587a8b00df8c50350cf02c680f8894b58a93ce1ed88ac8c0b9ded7595a14f041`;
the exported tarball SHA-256 is
`4368326ea89a32f85397005a86589fba335607f4b25e6fa981852fd83ba47f1f`.
The API default/rebase target is `cbdeepseek:v1`; pools for this image and the
original `cbcore:v23` remain configured with warm target 0. The original image
is retained. The user's box was saved to NAS and rebased onto the custom image.

The image exporter now accepts `CB_MODEL_MAIN` and `CB_MODEL_COMPUTER` while
retaining the upstream defaults when those variables are absent. The cut used:

```sh
sudo env CB_MODEL_MAIN=deepseek-flash CB_MODEL_COMPUTER=deepseek-flash \
  image/cut.sh -f cbdeepseek cbcore:v23 v1
```

Live tests exercised a main-agent shell tool round trip and the computer
subagent's `computer_state` / `computer_act`: opening Mousepad, typing a test
string into a new unsaved document, and visually verifying it. Usage records
identified `deepseek-flash` for both roles. Tests created named setup-check
conversations and an unsaved test document; existing user files were not changed.

The image's optional tier classifier still names `typesafe/jev-1.14`. It is
not provisioned by the direct DeepSeek configuration. The existing supported
fallback lets the main model choose tools; it logs a router-unavailable note
and does not prevent the verified tool or computer-use flows.

External ChatGPT/Codex clients continue to use their own model access; the
provider key configured here powers the resident Cocoonbox agent.
