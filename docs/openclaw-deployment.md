# Fedora openclaw deployment assessment

Historical assessment: the host was subsequently reinstalled with Ubuntu.
See [the current deployment record](ubuntu-cocoonbox-deployment.md).

Inspected 2026-10-18, repository commit `6d8e515`. Deployment is feasible;
the service is not installed yet. These are live host observations, not a
completed deployment or a tested Fedora installer.

## Verified host state

- SSH alias `openclaw` resolves to `lucaswang@openclaw.tailb695d.ts.net`.
- Fedora 43 Workstation, x86_64, Ryzen 7 5800H, 16 logical CPUs, AMD-V.
- Approximately 30 GiB RAM (28 GiB available) and 441 GiB disk free.
- `/dev/kvm` is readable/writable by the SSH account; `/dev/vhost-vsock`
  and `/dev/net/tun` exist. A real microVM boot still needs verification.
- Root filesystem is Btrfs; cgroup v2, SELinux enforcing, firewalld active.
- Docker and Podman are present; no running Docker containers were listed.
- No Cocoonbox, sandboxd, Caddy, or PostgreSQL system units were listed.
  Caddy, PostgreSQL server, Go, Rust, and Cargo packages were absent.
- Tailscale 1.102.3 is running, with `lucaswang` as operator and a certificate
  domain of `openclaw.tailb695d.ts.net`.
- Existing Serve configuration owns HTTPS 443 and proxies `/` to
  `127.0.0.1:18789`. HTTPS certificate validation passed, but the response
  was 502; the upstream refused/unavailable connection returned curl status
  000. This is an existing OpenClaw service issue, outside this installation.
- `sudo -n true` fails because a password is required.

## Gaps and implementation choices

| Gap | Evidence and required work |
| --- | --- |
| Host installation | `install-pg`, `install-caddy`, and `install-apt-cache` assume apt/Debian conventions. Use Fedora packages, initialize its PostgreSQL cluster, and adapt service/configuration paths. |
| Storage quota | `deploy/cbsbx/install` unconditionally invokes XFS project quotas on `/srv/cb`. It cannot run unchanged on this Btrfs root. Adapt it to use Btrfs storage with disk monitoring, or explicitly provision a separate XFS filesystem. Do not format the existing disk. |
| Runtime | Install compatible Cocoon, sandboxd, silkd, Cloud Hypervisor and any required Firecracker assets; then verify daemon startup, image import, boot, hibernate and wake. QEMU/libvirt already present do not replace these dependencies. |
| Host networking | The scripts manage iptables and fixed `10.203.0.1` bridge addresses. Integrate and test with Fedora firewalld, Docker and Tailscale, including after reboot. SELinux behavior is untested; investigate actual denials if encountered. |
| Gateway | API validation requires gateway admin URL/token. The supplied gateway seed expects an OpenRouter provider key. Configure an authorized provider and matching guest model names; no provider credentials were inspected or copied. |
| Database and secrets | Create API/gateway PostgreSQL roles and databases, peer mappings, signing key, master key, service credentials and an initial admin account. Root access is needed for system services and networking. |
| Persistent state | The stock API installer requires an S3 endpoint and credentials and forces TLS. Select a local or remote S3-compatible store and configure reachability for both API and guest; local HTTP needs `secure: false`. The API can omit the bucket, but that omits its state-save/rebase durability and is not equivalent to the full setup. |
| HTTPS route | Preserve existing port 443. Proposed endpoint is `https://openclaw.tailb695d.ts.net:8443`, with Tailscale Serve terminating TLS and a loopback Caddy router distributing HTTP and native gRPC to the two API listeners. |
| Clients | Set the same HTTPS origin in API OAuth/Connect settings and macOS/CLI clients. Tailnet clients need access to port 8443; external SaaS OAuth callbacks cannot reach a private origin unless they have suitable network access. |

The candidate router is `deploy/host/Caddyfile.tailscale`. It reuses the
repository's existing route matchers and explicit h2c gRPC upstreams. It has
been validated successfully with Caddy 2.11.7 on the Fedora host, using the
official binary archive after checking its published SHA-512 checksum. The
binary and configuration were staged in `/tmp/cocoonbox-caddy-check.4KUO8u`;
no Caddy server was started. End-to-end routing and Tailscale's gRPC streaming
behavior still need verification. Serving only the console listener does not
expose the native client API.

## Available release and image assets

GitHub's latest release APIs reported Cocoonbox `v0.1.8`, Cocoon `v0.7.2`,
and sandbox `v0.1.17`. Release assets include prebuilt amd64 API, agent,
Cocoon, sandboxd and silkd binaries. These releases have not yet been proven
compatible with each other or this checkout; pin and verify checksums before use.

Anonymous GHCR manifest retrieval succeeded for
`ghcr.io/loomhand/cocoonbox/base:latest`. Its rootfs layer is about 1.11 GB,
named `cocoonbox-cbcore-v23.tar.gz`, built from revision
`cb073839193dde06ee19d5f097bc382611205e80`, with layer digest:

```
sha256:34545039e6f89135031d329eb5042fb318593d7526274d939a5743f49f81980e
```

That image revision differs from this checkout. Verify the immutable manifest
digest, release relationship and agent/API compatibility before deployment.
Only the manifest was fetched; the rootfs has not been downloaded or booted.

## Completion checks

1. All services start with SELinux enforcing and survive reboot.
2. Tailnet HTTPS validates without certificate bypass; existing 443 route is preserved.
3. Admin login and user creation work, with gateway and sandbox tenants provisioned.
4. A real desktop box starts; browser desktop, native gRPC streaming and MCP work.
5. An authorized model call succeeds through the gateway.
6. Save, hibernate, wake and persistent state restore work using the selected store.
7. Database, master/signing keys and state store have a documented backup path.

Root access and provider/storage configuration remain unresolved. No remote
packages, services, firewall rules, storage layout or Serve routes were changed.
