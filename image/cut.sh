#!/bin/bash
# Cuts desktop template <family>:<vN> on the cbsbx cocoon root: boot <base template>, provision, seal, stop, export, import.
# usage: sudo image/cut.sh [-n] [-p] [-a <agent binary>] [-f <family>] [-r <runtime dir>]... <base template> <vN>
#   -n prints the steps and changes nothing; -p reruns provision.sh; each -r adds a runtime: the directory goes into the guest and its provision.sh runs, in order;
#   -f cbcore without -r adds image/desk and checks the guest with image/clean-gate.sh after the agent step;
#   every step goes to ~/ops-audit.md
set -Eeuo pipefail
export LC_ALL=C

DRY=0
PROVISION=0
AGENT=""
FAMILY=cbdesk
RUNTIMES=()
while getopts npa:f:r: o; do
	case $o in
	n) DRY=1 ;;
	p) PROVISION=1 ;;
	a) AGENT=$OPTARG ;;
	f) FAMILY=$OPTARG ;;
	r) RUNTIMES+=("$(realpath "$OPTARG")") ;;
	*) exit 2 ;;
	esac
done
shift $((OPTIND - 1))
BASE=${1:?usage: cut.sh [-n] [-p] [-a agent] [-f family] [-r runtime]... <base template> <vN>}
V=${2:?usage: cut.sh [-n] [-p] [-a agent] [-f family] [-r runtime]... <base template> <vN>}
S=/srv/cb/sbx
COCOON=$S/bin/cocoon-cbsbx
PROXY=http://127.0.0.1:3128
NO_PROXY=localhost,127.0.0.1,::1
EGRESS_PORT=2049
AUDIT=${AUDIT:-$(getent passwd "${SUDO_USER:-root}" | cut -d: -f6)/ops-audit.md}
case $FAMILY in
cbdesk) VM=cbcut-$V TARBALL=$S/build/cocoonbox-desktop-$V.tar.gz ;;
*[!a-z0-9]* | [!a-z]*) echo "family $FAMILY is not [a-z][a-z0-9]*" >&2; exit 2 ;;
*) VM=cbcut-$FAMILY-$V TARBALL=$S/build/cocoonbox-$FAMILY-$V.tar.gz ;;
esac
TEMPLATE=$FAMILY:$V
SRC="$(cd "$(dirname "$0")" && pwd)"
if [ "$FAMILY" = cbcore ] && [ ${#RUNTIMES[@]} = 0 ]; then
	RUNTIMES=("$SRC/desk")
fi
for R in "${RUNTIMES[@]}"; do
	[ -f "$R/provision.sh" ] || { echo "runtime $R has no provision.sh" >&2; exit 2; }
done
STEP=start
PROXY_PID=""

step() {
	STEP=$1
	printf '%s [cut] %s %s\n' "$(date -u +%FT%TZ)" "$TEMPLATE" "$1" | tee -a "$AUDIT"
}
fail() {
	printf '%s [cut] %s FAILED at %s: %s\n' "$(date -u +%FT%TZ)" "$TEMPLATE" "$STEP" "$BASH_COMMAND" | tee -a "$AUDIT" >&2
	stop_proxy
	echo "vm $VM is kept; remove it with: sudo $COCOON vm rm --force $VM" >&2
}
stop_proxy() {
	[ -z "$PROXY_PID" ] || kill "$PROXY_PID" 2>/dev/null || true
	PROXY_PID=""
}
guest() { "$COCOON" vm exec "$@"; }
active() { guest "$VM" -- systemctl -q is-active cocoonbox-xorg cocoonbox-session 2>/dev/null; }

trap fail ERR
trap 'stop_proxy' EXIT
if [ -n "$AGENT" ] && [ ! -x "$AGENT" ]; then
	echo "agent binary $AGENT is not executable" >&2
	exit 1
fi
if [ -e "$TARBALL" ]; then
	echo "$TARBALL exists" >&2
	exit 1
fi
if [ "$DRY" = 1 ]; then
	echo "boot $BASE as $VM"
	[ "$PROVISION" = 0 ] || echo "provision"
	for R in "${RUNTIMES[@]}"; do echo "runtime $R"; done
	if [ -n "$AGENT" ]; then
		echo "agent $AGENT"
		for R in "${RUNTIMES[@]}"; do [ ! -f "$R/after-agent.sh" ] || echo "runtime $R after agent"; done
	fi
	[ "$FAMILY" != cbcore ] || echo "clean gate"
	printf '%s\n' seal stop "export $TARBALL" "import $TEMPLATE"
	exit 0
fi

step "boot $BASE as $VM"
"$COCOON" vm run --name "$VM" --cpu 4 --memory 8G --storage 40G --nics 0 --no-direct-io "$BASE"
for _ in $(seq 180); do active && break; sleep 1; done
active

if [ "$PROVISION" = 1 ] || [ ${#RUNTIMES[@]} -gt 0 ]; then
	VSOCK=$("$COCOON" vm inspect "$VM" | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d[0] if isinstance(d,list) else d)["vsock_socket"])')
	python3 "$SRC/cut-proxy.py" "${VSOCK}_$EGRESS_PORT" &
	PROXY_PID=$!
fi
if [ "$PROVISION" = 1 ]; then
	step "provision"
	tar -C "$SRC" -cf - provision.sh rootfs vendor | guest -i "$VM" -- sh -c 'rm -rf /tmp/cb-image && mkdir -p /tmp/cb-image && tar -xf - -C /tmp/cb-image'
	guest -e "http_proxy=$PROXY" -e "https_proxy=$PROXY" -e "no_proxy=$NO_PROXY" "$VM" -- bash /tmp/cb-image/provision.sh
	guest "$VM" -- rm -rf /tmp/cb-image
fi
for R in "${RUNTIMES[@]}"; do
	N=$(basename "$R")
	step "runtime $N"
	tar -C "$(dirname "$R")" -cf - "$N" | guest -i "$VM" -- sh -c 'rm -rf /tmp/cb-image && mkdir -p /tmp/cb-image && tar -xf - -C /tmp/cb-image'
	guest -e "http_proxy=$PROXY" -e "https_proxy=$PROXY" -e "no_proxy=$NO_PROXY" "$VM" -- bash "/tmp/cb-image/$N/provision.sh"
	guest "$VM" -- rm -rf /tmp/cb-image
done
stop_proxy
if [ -n "$AGENT" ]; then
	step "agent $(sha256sum "$AGENT" | cut -c1-16)"
	guest -i "$VM" -- sh -c 'cat > /tmp/cocoonbox-agent' < "$AGENT"
	guest "$VM" -- sh -c 'install -m 0755 /tmp/cocoonbox-agent /usr/local/bin/cocoonbox-agent && rm /tmp/cocoonbox-agent && systemctl enable --now cocoonbox-toolhost cocoonbox-agent'
	for R in "${RUNTIMES[@]}"; do
		[ -f "$R/after-agent.sh" ] || continue
		N=$(basename "$R")
		step "runtime $N after agent"
		tar -C "$(dirname "$R")" -cf - "$N" | guest -i "$VM" -- sh -c 'rm -rf /tmp/cb-image && mkdir -p /tmp/cb-image && tar -xf - -C /tmp/cb-image'
		guest "$VM" -- bash "/tmp/cb-image/$N/after-agent.sh"
		guest "$VM" -- rm -rf /tmp/cb-image
	done
fi

if [ "$FAMILY" = cbcore ]; then
	step "clean gate"
	guest -i "$VM" -- bash -s < "$SRC/clean-gate.sh"
fi

step "seal"
guest -i "$VM" -- bash -s < "$SRC/seal.sh"
step "stop"
"$COCOON" vm stop "$VM"
step "export $TARBALL"
"$SRC/export-rootfs.sh" "$VM" "$S/bin/silkd" "$TARBALL"
"$COCOON" vm rm "$VM"
step "import"
"$COCOON" image import "$TEMPLATE" "$TARBALL"
step "done: $(du -h "$TARBALL" | cut -f1) $(sha256sum "$TARBALL" | cut -c1-16)"
