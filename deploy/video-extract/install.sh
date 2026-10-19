#!/bin/bash
# Run inside the guest as root. Does not restart services; the caller does that after verification.
set -euo pipefail
src=$(cd "$(dirname "$0")" && pwd)
dest=/home/user/.local/share/cocoonbox/video-extract
export DEBIAN_FRONTEND=noninteractive
if ! command -v ffmpeg >/dev/null || ! command -v python3 >/dev/null; then
    apt-get update -qq
    apt-get install -y --no-install-recommends ffmpeg python3
fi
install -d -o user -g user -m 0755 "$dest"
install -o user -g user -m 0755 "$src/extract.py" "$dest/extract.py"
install -o user -g user -m 0644 "$src/GUIDE.md" "$dest/GUIDE.md"
install -d -o user -g user -m 0755 /workspace/flows
install -o user -g user -m 0644 "$src/chatgpt.mts" /workspace/flows/bilibili-extract.mts
if [ ! -s "$dest/yt-dlp" ]; then
    scratch=$(mktemp -d)
    trap 'rm -rf "$scratch"' EXIT
    curl -fL --retry 2 --max-time 180 https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp -o "$scratch/yt-dlp"
    curl -fL --retry 2 --max-time 60 https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS -o "$scratch/SHA2-256SUMS"
    (cd "$scratch" && grep -E '  yt-dlp$' SHA2-256SUMS | sha256sum --check --strict)
    install -o user -g user -m 0644 "$scratch/yt-dlp" "$dest/yt-dlp"
fi
install -d -m 0755 /etc/cocoonbox/tools.d /etc/cocoonbox/routing.d
install -m 0644 "$src/bilibili.json" /etc/cocoonbox/tools.d/bilibili.json
printf '%s\n' 'For Bilibili video text extraction, call bilibili_run with code: "", screenshot: false and params: {bilibili_link: URL}; poll its job_id, then retrieve the document. Use mode: "ocr" for visible text.' > /etc/cocoonbox/routing.d/bilibili.md
runuser -u user -- python3 "$dest/yt-dlp" --version
runuser -u user -- python3 -c 'import json, pathlib; p=pathlib.Path("/etc/cocoonbox/tools.d/bilibili.json"); d=json.loads(p.read_text()); assert pathlib.Path(d["bin"]).is_file(); assert len(d["description"]) <= 2048'
echo 'Installed bilibili_run. First install: restart cocoonbox-toolhost and cocoonbox-agent.'
echo 'Upgrade with the same tool path: restart only cocoonbox-agent to refresh the descriptor.'
echo 'Preserve active workers and avoid restarting their toolhost until they finish.'
