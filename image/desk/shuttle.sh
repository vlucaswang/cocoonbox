#!/bin/bash
# The shuttle release whose desk runtime (desk.mts, gui_plan.mts, png.mts) cbcore and the Android base install in /opt/desk/lib.
# Sourced by image/desk/provision.sh and image/android/provision.sh; run as `shuttle.sh fetch DIR` it downloads the pinned
# archive into DIR (image/desk/vendor for a cut; cb-image-cut copies the runner's vendor tarballs there).
SHUTTLE_VERSION=v0.1.1
SHUTTLE_SHA256=f9b2e0a8ead8156d2a727be1d1050edfaaf2d3b6740d6d0df23285af66a374d6
SHUTTLE_TARBALL="cocoon-shuttle-${SHUTTLE_VERSION}-linux-x64.tar.gz"
SHUTTLE_URL="https://github.com/loomhand/shuttle/releases/download/${SHUTTLE_VERSION}/${SHUTTLE_TARBALL}"

install_shuttle_runtime() {
	local tarball=$1 tmp
	echo "$SHUTTLE_SHA256  $tarball" | sha256sum -c --quiet -
	tmp=$(mktemp -d)
	tar -xzf "$tarball" -C "$tmp" --no-same-owner cocoon-shuttle/runtime
	for f in desk gui_plan png; do install -D -m 0644 "$tmp/cocoon-shuttle/runtime/$f.mts" "/opt/desk/lib/$f.mts"; done
	rm -rf "$tmp"
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
	set -euo pipefail
	[ "${1:-}" = fetch ] && [ -n "${2:-}" ] || { echo "usage: $0 fetch DIR" >&2; exit 2; }
	mkdir -p "$2"
	curl -fsSL "$SHUTTLE_URL" -o "$2/$SHUTTLE_TARBALL.tmp"
	echo "$SHUTTLE_SHA256  $2/$SHUTTLE_TARBALL.tmp" | sha256sum -c --quiet -
	mv -f "$2/$SHUTTLE_TARBALL.tmp" "$2/$SHUTTLE_TARBALL"
	echo "$2/$SHUTTLE_TARBALL"
fi
