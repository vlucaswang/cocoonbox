#!/bin/bash
# Provisions the cocoonbox desktop guest (Ubuntu 24.04 xface base) in place; idempotent.
# Runs as root inside the guest from an unpacked copy of image/ (see cut.sh -p).
set -euo pipefail

# loomhand/cua dev, built by cua-driver.sh; cut.sh -p brings it in as vendor/
CUA_COMMIT=65a232e40abbb4f2665b26b46dc30bdfb8e33489
CUA_VERSION=0.36.0-loomhand.5
CUA_SHA256=e54c05edb2c96953da381b9a196f0dbb8970a9930c60832c7112f96e9e9ffffb
CUA_TARBALL="cua-driver-rs-${CUA_VERSION}-linux-x86_64.tar.gz"
MISE_VERSION=2026.10.0
MISE_SHA256=6ae3d2bda39cca86713501317edf623b59b75cd8afa2e31c20b1ee6500b4b739
MISE_TARBALL="mise-v${MISE_VERSION}-linux-x64.tar.gz"
MISE_URL="https://github.com/jdx/mise/releases/download/v${MISE_VERSION}/${MISE_TARBALL}"
NODE_VERSION=24.21.0
NODE_SHA256=fd8e59d5a511510f6a298afb548f18c7d2b1be404d8b4a27d94fbe49f56cb2d6
NODE_TARBALL="node-v${NODE_VERSION}-linux-x64.tar.xz"
NODE_URL="https://nodejs.org/dist/v${NODE_VERSION}/${NODE_TARBALL}"
CHROME_URL=https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb
PACKAGES=(
	xserver-xorg-core xserver-xorg-video-dummy xserver-xorg-input-libinput
	tigervnc-xorg-extension xauth x11-xserver-utils x11-utils xinput
	at-spi2-core libatk-adaptor dbus-user-session dconf-cli
	gnome-themes-extra-data mousepad nftables sudo libnss-myhostname libnss3-tools rsync git gh tmux unzip
	build-essential ncurses-bin tesseract-ocr
	libreoffice-calc libreoffice-writer libreoffice-impress libreoffice-gtk3
	locales fonts-noto-cjk fonts-noto-color-emoji
)

SRC="$(cd "$(dirname "$0")" && pwd)"
export DEBIAN_FRONTEND=noninteractive

log() { printf '[provision] %s\n' "$*"; }

setup_packages() {
	log "packages"
	apt-get update -qq
	apt-get install -y -qq --no-install-recommends "${PACKAGES[@]}" >/dev/null
	systemctl disable --now xrdp.service xrdp-sesman.service >/dev/null 2>&1 || true
	pkill -x xrdp || true
	pkill -x xrdp-sesman || true
	systemctl disable --now cocoonbox-vnc.service >/dev/null 2>&1 || true
	rm -f /etc/systemd/system/cocoonbox-vnc.service
	systemctl daemon-reload
	systemctl reset-failed cocoonbox-vnc.service >/dev/null 2>&1 || true
	rm -f /etc/ssh/sshd_config.d/10-cocoonbox.conf /etc/systemd/system/cocoonbox-hostkeys.service /etc/systemd/system/multi-user.target.wants/cocoonbox-hostkeys.service
	apt-get purge -y -qq xrdp xorgxrdp xscreensaver xscreensaver-data x11vnc openssh-server openssh-sftp-server >/dev/null 2>&1 || true
	local deb
	deb=$(mktemp --suffix=.deb)
	curl -fsSL "$CHROME_URL" -o "$deb"
	# dpkg -i always unpacks, restoring the stock launcher the base image patched
	dpkg -i "$deb" >/dev/null || apt-get install -y -qq -f >/dev/null
	rm -f "$deb"
	rm -f /etc/xdg/autostart/google-chrome.desktop /etc/xdg/autostart/xscreensaver.desktop
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
	usermod -aG input user
	passwd -l user >/dev/null

	id cbagent >/dev/null 2>&1 || useradd --system --no-create-home --home-dir /var/lib/cocoonbox --shell /usr/sbin/nologin cbagent
	if id deploy >/dev/null 2>&1; then
		userdel -r deploy
	fi
	rm -f /etc/sudoers.d/90-cocoonbox-deploy /home/user/.ssh/authorized_keys
	printf 'Defaults:user env_keep += "http_proxy https_proxy no_proxy HTTP_PROXY HTTPS_PROXY NO_PROXY"\nuser ALL=(ALL) NOPASSWD:ALL\n' > /etc/sudoers.d/90-cocoonbox-user
	chmod 0440 /etc/sudoers.d/90-cocoonbox-user
	visudo -cqf /etc/sudoers.d/90-cocoonbox-user

	passwd -l root >/dev/null
	usermod -c "cocoonbox user" user
	sed -i -E 's/^# *(en_US\.UTF-8 UTF-8|zh_CN\.UTF-8 UTF-8)$/\1/' /etc/locale.gen
	locale-gen >/dev/null
	printf 'LANG=en_US.UTF-8\n' > /etc/default/locale
}

install_rootfs() {
	log "rootfs files"
	rm -f /usr/local/lib/cocoonbox/set-agent-env
	(cd "$SRC/rootfs" && find . -type f ! -path './etc/skel-cocoonbox/*') | while read -r f; do
		case "$f" in
		./usr/local/lib/cocoonbox/* | ./usr/local/bin/*) install -D -m 0755 "$SRC/rootfs/$f" "/${f#./}" ;;
		*) install -D -m 0644 "$SRC/rootfs/$f" "/${f#./}" ;;
		esac
	done
	chmod 0755 /etc/nftables.conf
	grep -qF cocoonbox-vault.sh /etc/bash.bashrc || printf '. /etc/profile.d/cocoonbox-vault.sh\n' >> /etc/bash.bashrc
}

setup_desktop() {
	log "desktop"
	local cfg=/home/user/.config xfconf=/home/user/.config/xfce4/xfconf/xfce-perchannel-xml
	install -d -o user -g user "$cfg" "$cfg/xfce4" "$cfg/xfce4/xfconf" "$xfconf" "$cfg/systemd" "$cfg/systemd/user" "$cfg/systemd/user/default.target.wants"
	install -m 0644 -o user -g user "$SRC"/rootfs/etc/skel-cocoonbox/.config/xfce4/xfconf/xfce-perchannel-xml/*.xml "$xfconf/"
	[ -f "$xfconf/xfce4-panel.xml" ] || install -m 0644 -o user -g user /etc/xdg/xfce4/panel/default.xml "$xfconf/xfce4-panel.xml"
	sed -i 's|name="ThemeName" type="string" value="Greybird"|name="ThemeName" type="string" value="Adwaita"|' /etc/xdg/xfce4/xfconf/xfce-perchannel-xml/xsettings.xml
	ln -sfn /etc/systemd/user/cua-driver.service "$cfg/systemd/user/default.target.wants/cua-driver.service"
	chown -h user:user "$cfg/systemd/user/default.target.wants/cua-driver.service"
	dconf update
	install -d -m 0755 /var/lib/systemd/linger
	touch /var/lib/systemd/linger/user
}

setup_cua_driver() {
	log "cua-driver $CUA_VERSION"
	local dir=/opt/cua-driver/$CUA_VERSION tarball="$SRC/vendor/$CUA_TARBALL"
	if [ ! -x "$dir/cua-driver" ]; then
		[ -f "$tarball" ] || { echo "missing $tarball: build it with COMMIT=$CUA_COMMIT VERSION=$CUA_VERSION image/cua-driver.sh image/vendor" >&2; exit 1; }
		echo "$CUA_SHA256  $tarball" | sha256sum -c --quiet -
		install -d "$dir"
		tar -xzf "$tarball" -C "$dir" --strip-components=1 --no-same-owner
	fi
	ln -sfn "$dir/cua-driver" /usr/local/bin/cua-driver
	find /opt/cua-driver -mindepth 1 -maxdepth 1 ! -name "$CUA_VERSION" -exec rm -rf {} +
	runuser -u user -- env HOME=/home/user /usr/local/bin/cua-driver telemetry disable >/dev/null
	runuser -u user -- env HOME=/home/user /usr/local/bin/cua-driver telemetry reset-id >/dev/null
}

setup_mise() {
	log "mise $MISE_VERSION"
	local dir=/opt/mise/$MISE_VERSION tmp
	if [ ! -x "$dir/bin/mise" ]; then
		tmp=$(mktemp -d)
		curl -fsSL "$MISE_URL" -o "$tmp/$MISE_TARBALL"
		echo "$MISE_SHA256  $tmp/$MISE_TARBALL" | sha256sum -c --quiet -
		install -d "$dir"
		tar -xzf "$tmp/$MISE_TARBALL" -C "$dir" --strip-components=1
		rm -rf "$tmp"
	fi
	ln -sfn "$dir/bin/mise" /usr/local/bin/mise
}

setup_node() {
	log "node $NODE_VERSION"
	local dir=/opt/node/$NODE_VERSION tmp
	if [ ! -x "$dir/bin/node" ]; then
		tmp=$(mktemp -d)
		curl -fsSL "$NODE_URL" -o "$tmp/$NODE_TARBALL"
		echo "$NODE_SHA256  $tmp/$NODE_TARBALL" | sha256sum -c --quiet -
		install -d "$dir"
		tar -xJf "$tmp/$NODE_TARBALL" -C "$dir" --strip-components=1 --no-same-owner
		rm -rf "$tmp"
	fi
	for b in node npm npx corepack; do ln -sfn "$dir/bin/$b" "/usr/local/bin/$b"; done
	find /opt/node -mindepth 1 -maxdepth 1 ! -name "$NODE_VERSION" -exec rm -rf {} +
}

setup_cocoonbox_dirs() {
	log "cocoonbox dirs"
	install -d -m 0755 -o user -g user /workspace
	install -d -m 0755 -o root -g root /etc/cocoonbox
	systemd-tmpfiles --create /etc/tmpfiles.d/cocoonbox.conf
	apt-mark showmanual | LC_ALL=C sort -u > /etc/cocoonbox/packages.baseline
	if [ ! -f /etc/cocoonbox/golden-marker ]; then
		printf 'cocoonbox golden provisioned %s\n' "$(date -u +%FT%TZ)" > /etc/cocoonbox/golden-marker
	fi
}

enable_units() {
	log "units"
	systemctl daemon-reload
	udevadm control --reload
	udevadm trigger --name-match=uinput
	systemctl enable --now nftables.service cocoonbox-machine-id.path cocoonbox-hostname.path cocoonbox-egress-ca.path cocoonbox-github-git.path cocoonbox-vault.path 'run-cocoonbox\x2doffload.mount'
	systemctl --global enable cocoonbox-offload-inbox.service
	systemctl enable cocoonbox-github-git.service cocoonbox-vault.service
	nft -f /etc/nftables.conf
	systemctl reload systemd-logind
	systemctl list-units 'session-*.scope' --state=abandoned --no-legend --plain | awk '{print $1}' | xargs -r systemctl stop
	systemctl enable cocoonbox-xorg.service cocoonbox-session.service
	systemctl start user@1000.service
	systemctl restart cocoonbox-xorg.service cocoonbox-session.service
	systemctl --user -M user@ daemon-reload
	systemctl --user -M user@ restart cua-driver.service cocoonbox-offload-inbox.service
}

setup_packages
setup_users
install_rootfs
setup_desktop
setup_cua_driver
setup_mise
setup_node
setup_cocoonbox_dirs
enable_units
log "done"
