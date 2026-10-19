#!/bin/bash
# Provisions the Android box base (Ubuntu 24.04, docker, scrcpy-rfb, ashmem, the UI server, shuttle and its desk runtime) as root.
# Runs from a copy of image/android in the base build (base/Dockerfile), after /var/lib/docker holds the baked container.
set -euo pipefail

DOCKER_VERSION=5:29.8.1-1~ubuntu.24.04~noble
PACKAGES=(adb libjpeg-turbo8 sudo locales iptables dnscrypt-proxy redsocks git xz-utils btrfs-progs polkitd)
BUILD_PACKAGES=(gcc-13 make libc6-dev)
NODE_VERSION=24.21.0
ARCH=$(dpkg --print-architecture)
case $ARCH in
amd64)
	NODE_ARCH=x64 NODE_SHA256=fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6
	SCRCPY_RFB="scrcpy-rfb-linux-amd64 c54fef8d5fd6b95e2cd595a54d89b7f34e85de0ab3137e4965936c3850ddb28f"
	SU1000="su1000-linux-amd64 d1e3681254937afa319fcd43702ac53a2290c01b9e3b1fa750195a66e149d2ed"
	;;
arm64)
	NODE_ARCH=arm64 NODE_SHA256=6ad1325edbdb5649c379b75a237147a666c95d4f9ae8d340fef2d1575d289ad2
	SCRCPY_RFB="scrcpy-rfb-linux-arm64 dab2f1fe80935ad98ed5f0d3b77dc6a693d2feb6e1533e4cf909cde204f8706d"
	SU1000="su1000-linux-arm64 72ca7905078089d4328a40267575ba258036ca20304844e3272ffff927a48c66"
	;;
*) echo "unsupported architecture $ARCH" >&2; exit 1 ;;
esac
NODE_TARBALL="node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz"
NODE_URL="https://nodejs.org/dist/v${NODE_VERSION}/${NODE_TARBALL}"
DESK=$(cd "$(dirname "$0")/../desk/rootfs" && pwd)
# the gw endpoint of the agent's contract (export-rootfs.sh), for the plan executor and scripts
GW_BASE_URL=http://10.203.0.1:18090/v1
# cocoonstack/scrcpy-rfb commit-tagged release master-827baf531 (tag protected by ruleset; 827baf531715d6b86482c7ac6c63fd31030c58aa, run 39217674630); the jar run as shell is vanilla scrcpy 4.1
SCRCPY_URL=https://github.com/cocoonstack/scrcpy-rfb/releases/download/master-827baf531
SCRCPY_FILES=(
	"$SCRCPY_RFB /usr/local/bin/scrcpy-rfb 0755"
	"$SU1000 /usr/local/share/scrcpy/su1000 0755"
	"scrcpy-server-secure.jar 8f027776c89888d2fe5cab5be4bfd1a460119d210054ce21b2f6bf53f1fe820e /usr/local/share/scrcpy/scrcpy-server.jar 0644"
	"scrcpy-server.jar deacb991ed2509715160ffdc7907e47b4160eb30d1566217e9047fd5b8850cae /usr/local/share/scrcpy/scrcpy-server-shell.jar 0644"
)

SRC="$(cd "$(dirname "$0")" && pwd)"
export DEBIAN_FRONTEND=noninteractive

log() { printf '[provision] %s\n' "$*"; }

setup_packages() {
	log "packages"
	printf '#!/bin/sh\nexit 101\n' > /usr/sbin/policy-rc.d
	chmod 0755 /usr/sbin/policy-rc.d
	install -m 0755 -d /etc/apt/keyrings
	[ -f /etc/apt/keyrings/docker.asc ] || curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
	echo "deb [arch=$ARCH signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu noble stable" > /etc/apt/sources.list.d/docker.list
	apt-get update -qq
	apt-get install -y -qq --no-install-recommends "linux-modules-extra-$(ls /lib/modules | sort -V | tail -1)" \
		"docker-ce=$DOCKER_VERSION" "docker-ce-cli=$DOCKER_VERSION" containerd.io "${PACKAGES[@]}" >/dev/null
	apt-get purge -y -qq openssh-server openssh-sftp-server >/dev/null 2>&1 || true
	rm -f /usr/sbin/policy-rc.d
}

# the 24.04 kernel has no ashmem, which Android 16 gralloc needs; the module is a port of the staging driver to this kernel
setup_ashmem() {
	local kver headers
	kver=$(ls /lib/modules | sort -V | tail -1)
	headers=linux-headers-$kver
	log "ashmem for $kver"
	apt-get install -y -qq --no-install-recommends "$headers" "${BUILD_PACKAGES[@]}" >/dev/null
	rm -rf /tmp/ashmem && cp -r "$SRC/ashmem" /tmp/ashmem
	make -s -C "/lib/modules/$kver/build" M=/tmp/ashmem CC=gcc-13 modules
	install -D -m 0644 /tmp/ashmem/ashmem_linux.ko "/lib/modules/$kver/updates/ashmem_linux.ko"
	depmod -a "$kver"
	rm -rf /tmp/ashmem
	apt-get purge -y -qq --auto-remove "$headers" "${BUILD_PACKAGES[@]}" >/dev/null
	modinfo -k "$kver" ashmem_linux >/dev/null
	apt-get clean
}

setup_users() {
	log "users"
	if id ubuntu >/dev/null 2>&1; then
		usermod -l user -d /home/user -m ubuntu
		groupmod -n user ubuntu
	fi
	[ "$(id -u user)" = 1000 ] || { echo "user must be uid 1000" >&2; exit 1; }
	for g in sudo adm; do gpasswd -d user "$g" >/dev/null 2>&1 || true; done
	passwd -l user >/dev/null
	id cbagent >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /var/lib/cocoonbox --shell /usr/sbin/nologin cbagent
	printf 'Defaults:user env_keep += "http_proxy https_proxy no_proxy HTTP_PROXY HTTPS_PROXY NO_PROXY ANDROID_SERIAL"\nuser ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/90-cocoonbox-user
	chmod 0440 /etc/sudoers.d/90-cocoonbox-user
	visudo -cqf /etc/sudoers.d/90-cocoonbox-user
	passwd -l root >/dev/null
	usermod -c "cocoonbox user" user
	sed -i -E 's/^# *(en_US\.UTF-8 UTF-8)$/\1/' /etc/locale.gen
	locale-gen >/dev/null
	printf 'LANG=en_US.UTF-8\n' > /etc/default/locale
}

install_rootfs() {
	log "rootfs files"
	(cd "$SRC/rootfs" && find . -type f) | while read -r f; do
		case "$f" in
		./usr/local/lib/cocoonbox/*) install -D -m 0755 "$SRC/rootfs/$f" "/${f#./}" ;;
		*) install -D -m 0644 "$SRC/rootfs/$f" "/${f#./}" ;;
		esac
	done
}

setup_scrcpy() {
	log "scrcpy-rfb"
	local f name sum dest mode tmp
	tmp=$(mktemp -d)
	for f in "${SCRCPY_FILES[@]}"; do
		read -r name sum dest mode <<<"$f"
		curl -fsSL "$SCRCPY_URL/$name" -o "$tmp/$name"
		echo "$sum  $tmp/$name" | sha256sum -c --quiet -
		install -D -m "$mode" "$tmp/$name" "$dest"
	done
	rm -rf "$tmp"
	! ldd /usr/local/bin/scrcpy-rfb | grep 'not found'
}

# the UI server dumps the accessibility tree from a resident app_process, without uiautomator's start and idle wait
setup_ui() {
	log "UI server"
	install -D -m 0644 "$SRC/ui/cb-ui.jar" /usr/local/share/cocoonbox/cb-ui.jar
}

# GUI plans and scripts run the desk runtime of cbcore on Node; the toolhost serves its calls from Android
setup_desk() {
	log "node $NODE_VERSION and the desk runtime"
	local dir=/opt/node/$NODE_VERSION tmp
	if [ ! -x "$dir/bin/node" ]; then
		tmp=$(mktemp -d)
		curl -fsSL "$NODE_URL" -o "$tmp/$NODE_TARBALL"
		echo "$NODE_SHA256  $tmp/$NODE_TARBALL" | sha256sum -c --quiet -
		install -d "$dir"
		tar -xJf "$tmp/$NODE_TARBALL" -C "$dir" --strip-components=1 --no-same-owner
		rm -rf "$tmp"
	fi
	for b in node npm npx; do ln -sfn "$dir/bin/$b" "/usr/local/bin/$b"; done
	# shellcheck source=image/desk/shuttle.sh
	. "$DESK/../shuttle.sh"
	tmp=$(mktemp -d)
	curl -fsSL "$SHUTTLE_URL" -o "$tmp/$SHUTTLE_TARBALL"
	install_shuttle "$tmp/$SHUTTLE_TARBALL"
	install_shuttle_runtime "$tmp/$SHUTTLE_TARBALL"
	rm -rf "$tmp"
	install -D -m 0755 "$DESK/usr/local/bin/desk" /usr/local/bin/desk
	install -D -m 0755 "$DESK/usr/local/lib/cocoonbox/shuttle-config" /usr/local/lib/cocoonbox/shuttle-config
	install -d /etc/cocoonbox
	printf 'GW_BASE_URL=%s\n' "$GW_BASE_URL" > /etc/cocoonbox/desk.env
	chmod 0644 /etc/cocoonbox/desk.env
	write_shuttle_config
}

# shuttle's box config: models through gw, no local controls, and its adb backend on the box's redroid with shuttle's UI server
write_shuttle_config() {
	cat >/etc/cocoonbox/shuttle.toml.tmp <<EOF
base_url = "$GW_BASE_URL"
decisions_url = "$GW_BASE_URL/decisions"
api_key = "unprovisioned"
decide_build = "typesafe/jev-1.14-########"

[safety]
indicator = false
confirm = "off"
kill_hotkey = "off"

[tools]
script_run = true

[android]
serial = "127.0.0.1:5555"
ui_jar = "/opt/cocoon-shuttle/$SHUTTLE_VERSION/android/cb-ui.jar"
scrcpy_jar = "/usr/local/share/scrcpy/scrcpy-server.jar"
su = "/usr/local/share/scrcpy/su1000"
EOF
	chmod 0644 /etc/cocoonbox/shuttle.toml.tmp
	mv -f /etc/cocoonbox/shuttle.toml.tmp /etc/cocoonbox/shuttle.toml
}

setup_cocoonbox_dirs() {
	log "cocoonbox dirs"
	# a container build mounts /etc/hosts read-only; the cut adds the line then
	grep -q ' localhost.localdomain' /etc/hosts || printf '127.0.0.1 localhost.localdomain\n' >> /etc/hosts 2>/dev/null || true
	install -d -m 0755 -o user -g user /workspace
	install -d -m 0755 -o root -g root /etc/cocoonbox
	install -d /var/lib/redroid-data
	systemd-tmpfiles --create /etc/tmpfiles.d/cocoonbox.conf
}

# docker stays off until the box has its identity, so each box cold-boots its own Android
enable_units() {
	log "units"
	[ ! -d /run/systemd/system ] || systemctl daemon-reload
	systemctl disable docker.service docker.socket containerd.service >/dev/null 2>&1
	systemctl is-active -q docker.service && { echo "docker is running in the template" >&2; exit 1; }
	systemctl mask dnscrypt-proxy.socket dnscrypt-proxy.service dnscrypt-proxy-resolvconf.service redsocks.service >/dev/null 2>&1
	systemctl enable cocoonbox-android.path cocoonbox-android-proxy.socket cocoonbox-session.service cocoon-shuttle.service >/dev/null
}

setup_packages
setup_ashmem
setup_users
install_rootfs
setup_scrcpy
setup_ui
setup_desk
setup_cocoonbox_dirs
enable_units
log "done"
