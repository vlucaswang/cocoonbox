#!/bin/bash
# Exports a stopped cbsbx cocoon VM as a flat rootfs tarball for `cocoon image import`; read-only on the VM's disks.
# usage: sudo image/export-rootfs.sh <stopped vm> <silkd binary> <out tarball>
# CB_MODEL_MAIN / CB_MODEL_COMPUTER override the exported image's model defaults.
set -euo pipefail
export LC_ALL=C

VM=${1:?usage: export-rootfs.sh <stopped vm> <silkd> <out tarball>}
SILKD=${2:?silkd binary}
OUT=${3:?out tarball}
COCOON=${COCOON:-/srv/cb/sbx/bin/cocoon-cbsbx}
BLOBS=/srv/cb/sbx/cocoon/oci/blobs
PROXY=http://127.0.0.1:3128
NO_PROXY=localhost,127.0.0.1,::1

log() { printf '[export %s] %s\n' "$(date -u +%T)" "$*"; }

disks=$("$COCOON" vm inspect "$VM" | python3 -c '
import json, sys
d = json.load(sys.stdin)
d = d[0] if isinstance(d, list) else d
if d["state"] != "stopped":
    sys.exit("vm %s is %s, not stopped" % (sys.argv[1], d["state"]))
for s in d["storage_configs"]:
    if s.get("role") in ("layer", "cow"):
        print(s["role"], s["path"])' "$VM")
mapfile -t layers < <(awk '$1 == "layer" {print $2}' <<<"$disks")
cow=$(awk '$1 == "cow" {print $2}' <<<"$disks")
[ "${#layers[@]}" -ge 1 ] || { echo "no layers in vm $VM" >&2; exit 1; }
[ -f "$cow" ] || { echo "no cow disk in vm $VM" >&2; exit 1; }
for l in "${layers[@]}"; do
	case "$l" in "$BLOBS"/*) ;; *) echo "layer outside $BLOBS: $l" >&2; exit 1 ;; esac
	[ -f "$l" ] || { echo "missing layer $l" >&2; exit 1; }
done

B=$OUT.build
rm -rf "$B"
install -d "$B" "$(dirname "$OUT")"
lower=""
for i in "${!layers[@]}"; do
	log "extract layer $i ($(basename "${layers[$i]}"))"
	fsck.erofs --extract="$B/l$i" --xattrs "${layers[$i]}"
	lower="$B/l$i${lower:+:$lower}"
done
log "reflink cow"
cp --reflink=always "$cow" "$B/cow.raw"

add=$B/add
CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
install -d "$add/usr/local/bin" "$add/etc/systemd/system/sysinit.target.wants" "$add/etc/cocoonbox" "$add/etc/apt/apt.conf.d"
printf 'Acquire::http::Proxy "%s";\nAcquire::https::Proxy "%s";\n' "$PROXY" "$PROXY" > "$add/etc/apt/apt.conf.d/90cocoonbox-proxy"
install -m 0755 "$SILKD" "$add/usr/local/bin/silkd"
cat > "$add/etc/systemd/system/silkd.service" <<EOF
[Unit]
Description=silkd (sandbox product daemon: exec, files, sessions over vsock)
DefaultDependencies=no
Before=basic.target

[Service]
Type=simple
ExecStart=/usr/local/bin/silkd
Environment=SILKD_PORT=2048
Environment=http_proxy=$PROXY https_proxy=$PROXY no_proxy=$NO_PROXY,169.254.169.254
Restart=always
RestartSec=2s
LimitNOFILE=65536

[Install]
WantedBy=sysinit.target
EOF
ln -s ../silkd.service "$add/etc/systemd/system/sysinit.target.wants/silkd.service"
ln -s /dev/null "$add/etc/systemd/system/systemd-networkd-wait-online.service"
for u in system/cocoonbox-agent system/cocoonbox-toolhost system/cocoonbox-session user/cocoonbox-offload-inbox user/cua-driver; do
	install -d "$add/etc/systemd/$u.service.d"
	cat > "$add/etc/systemd/$u.service.d/10-sbx-proxy.conf" <<EOF
[Service]
Environment=http_proxy=$PROXY https_proxy=$PROXY HTTP_PROXY=$PROXY HTTPS_PROXY=$PROXY
Environment=no_proxy=$NO_PROXY NO_PROXY=$NO_PROXY
Environment=REQUESTS_CA_BUNDLE=$CA_BUNDLE SSL_CERT_FILE=$CA_BUNDLE NODE_EXTRA_CA_CERTS=$CA_BUNDLE NODE_USE_ENV_PROXY=1
Environment=GOOGLE_OAUTH_ACCESS_TOKEN=@GOOGLE_CALENDAR@
EOF
done
proxy_env=(http_proxy="$PROXY" https_proxy="$PROXY" HTTP_PROXY="$PROXY" HTTPS_PROXY="$PROXY" no_proxy="$NO_PROXY" NO_PROXY="$NO_PROXY"
	REQUESTS_CA_BUNDLE="$CA_BUNDLE" SSL_CERT_FILE="$CA_BUNDLE" NODE_EXTRA_CA_CERTS="$CA_BUNDLE" NODE_USE_ENV_PROXY=1)
install -d "$add/etc/profile.d" "$add/etc/environment.d"
printf 'export %s\n' "${proxy_env[@]}" > "$add/etc/profile.d/cocoonbox-proxy.sh"
printf '%s\n' "${proxy_env[@]}" > "$add/etc/environment.d/90-cocoonbox-proxy.conf"

unshare -m bash -euo pipefail -c '
B=$1; lower=$2; out=$3
mount --make-rprivate /
mkdir -p "$B/m/cow" "$B/m/base" "$B/m/root"
mount -o loop "$B/cow.raw" "$B/m/cow"
mount -t overlay overlay -o ro,metacopy=on,redirect_dir=follow,lowerdir="$B/m/cow/upper:$lower" "$B/m/base"
gid=$(awk -F: "\$1==\"cbagent\"{print \$3}" "$B/m/base/etc/group")
[ -n "$gid" ] || { echo "no cbagent group in the image" >&2; exit 1; }
printf "GW_BASE_URL=http://10.203.0.1:18090/v1\nGW_API_KEY=unprovisioned\nMODEL_MAIN=%s\nMODEL_COMPUTER=%s\nAGENT_LISTEN=127.0.0.1:7443\nCONNECT_URL=http://10.203.0.1:18091/mcp\n" "${CB_MODEL_MAIN:-deepseek/deepseek-v4.1-flash}" "${CB_MODEL_COMPUTER:-anthropic/claude-sonnet-5}" > "$B/add/etc/cocoonbox/agent.env"
chown 0:"$gid" "$B/add/etc/cocoonbox/agent.env"
chmod 0640 "$B/add/etc/cocoonbox/agent.env"
mount -t overlay overlay -o ro,metacopy=on,redirect_dir=follow,lowerdir="$B/add:$B/m/cow/upper:$lower" "$B/m/root"
if grep -q "^EDGE_TOKEN=" "$B/m/root/etc/cocoonbox/agent.env"; then echo "edge token in the exported agent.env" >&2; exit 1; fi
tar -C "$B/m/root" --numeric-owner --xattrs --xattrs-include="*" -cf - . | gzip -1 > "$out.tmp"
mv "$out.tmp" "$out"
' _ "$B" "$lower" "$OUT"
rm -rf "$B"
log "$(sha256sum "$OUT")"
