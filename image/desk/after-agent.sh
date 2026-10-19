#!/bin/bash
# Runs as root in the guest after cut.sh -a installs the agent: puts the toolhost's shuttle service spec in place once the agent
# serves as one, then checks that shuttle runs the toolhost and that cbagent reaches both through shuttle's endpoint.
set -euo pipefail

SRC="$(cd "$(dirname "$0")" && pwd)"
SPEC=/etc/cocoonbox/tools.d/cocoonbox.json
DESK=/run/cocoonbox/desk.sock

log() { printf '[desk after agent] %s\n' "$*"; }

# call TOOL: one desk-wire call as cbagent on shuttle's endpoint; prints the answer's text, fails on an error answer
call() {
	runuser -u cbagent -- python3 -c '
import json, socket, struct, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(30); s.connect(sys.argv[1])
body = json.dumps({"agent_id": "cut", "tool": sys.argv[2], "args": {}}).encode()
s.sendall(struct.pack(">I", len(body)) + body)
buf = b""
while len(buf) < 4 or len(buf) < 4 + struct.unpack(">I", buf[:4])[0]:
    chunk = s.recv(65536)
    if not chunk: sys.exit("closed before the answer")
    buf += chunk
out = json.loads(buf[4:])
print(out["text"][:200])
sys.exit(1 if out.get("is_error") else 0)
' "$DESK" "$1"
}

probe=$(/usr/local/bin/cocoonbox-agent toolhost-service 2>&1 || true)
grep -q SHUTTLE_SERVICE_SOCKET <<<"$probe" || { echo "$(/usr/local/bin/cocoonbox-agent --version) has no toolhost-service" >&2; exit 1; }
log "toolhost as shuttle's service"
install -D -m 0644 "$SRC/service/cocoonbox.json" "$SPEC"
systemctl stop cocoonbox-toolhost.service
systemctl --user -M user@ restart cocoon-shuttle.service
systemctl restart cocoonbox-agent.service
for _ in $(seq 60); do [ -S /run/cocoonbox/services/cocoonbox.sock ] && break; sleep 0.5; done
[ "$(systemctl is-active cocoonbox-toolhost.service)" != active ]
[ "$(ps -o ppid= -p "$(pgrep -f '^/usr/local/bin/cocoonbox-agent toolhost-service$')" | tr -d ' ')" = "$(pgrep -f '^/usr/local/bin/cocoon-shuttle serve ')" ]
call desk_screen
call desktop_info
log "done"
