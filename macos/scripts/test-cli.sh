#!/bin/bash
# End-to-end test of the cocoonbox CLI and its stdio MCP bridge against MockBox:
# lifecycle commands over gRPC, the bridge's tool list while the box is
# hibernated, the wake on the first box-tool call with progress, the control
# tools and their refusals, --no-wake, the headers helper, several boxes:
# routing by `box`, the default box, the running limit, --box and box_delete,
# the app's socket: reported box-tool calls and a default box the app sets, and rsh with rsync
# and git over it (MockBox runs Exec commands on this Mac in a test workspace), and flows list,
# export, import and rm, box schedule set, list and clear, keys add, ls, rotate and rm, devices and devices revoke, logout --everywhere, a release-built
# bridge naming the keys at initialize, and login through the browser and logout. Nothing touches
# the login keychain or the real HOME: `login` stores the mock token in a
# throwaway keychain file (CB_KEYCHAIN_FILE), and HOME is a test directory.
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/toolchain.sh
swift build --product MockBox >/dev/null
swift build --product cocoonbox-cli >/dev/null
swift build -c release --product cocoonbox-cli >/dev/null
bin="$(swift build --show-bin-path)"
release="$(swift build -c release --show-bin-path)/cocoonbox-cli"

port=50071
work="$(mktemp -d)"
export COCOONBOX_ENDPOINT="http://127.0.0.1:$port"
export CB_KEYCHAIN_SERVICE=io.github.loomhand.cocoonbox.test
export COCOONBOX_CACHE_DIR="$work/cache"
export COCOONBOX_CONFIG_DIR="$work/config"
export HOME="$work/home"
export CFFIXED_USER_HOME="$work/home"
export CB_KEYCHAIN_FILE="$work/test.keychain-db"
mkdir -p "$HOME" "$work/workspace"
security create-keychain -p test "$CB_KEYCHAIN_FILE"
security set-keychain-settings "$CB_KEYCHAIN_FILE"
security unlock-keychain -p test "$CB_KEYCHAIN_FILE"

mkdir -p "$work/box" "$work/bin"
ln -s "$PWD/scripts/offload-box-stub" "$work/bin/cocoonbox-agent"
HOME="$work/box" PATH="$work/bin:$PATH" "$bin/MockBox" --port "$port" --boot-ms 1500 --workspace "$work/workspace" >"$work/mockbox.log" 2>&1 &
mock=$!
cleanup() {
  kill "$mock" 2>/dev/null || true
  security delete-keychain "$CB_KEYCHAIN_FILE" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT
sleep 1
echo mock-token | "$bin/cocoonbox-cli" login

python3 - "$bin/cocoonbox-cli" "$work/mockbox.log" "$release" <<'PY'
import json, subprocess, sys, time

cli, mocklog, release = sys.argv[1], sys.argv[2], sys.argv[3]
failures = []

def check(name, ok, detail=""):
    print(("ok   " if ok else "FAIL ") + name + (f"  ({detail})" if detail and not ok else ""))
    if not ok:
        failures.append(name)

def run(*args):
    return subprocess.run([cli, *args], capture_output=True, text=True, timeout=60)

status = run("status", "--json")
check("status over gRPC", json.loads(status.stdout)["state"] == "hibernated", status.stdout + status.stderr)

class Bridge:
    def __init__(self, *flags, env=None, binary=None):
        self.p = subprocess.Popen([binary or cli, "mcp", *flags], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1, env=env)
        self.next = 1

    def send(self, method, params=None, notify=False):
        message = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        if not notify:
            message["id"] = self.next
            self.next += 1
        self.p.stdin.write(json.dumps(message) + "\n")
        self.p.stdin.flush()
        return message.get("id")

    def call(self, method, params=None):
        id = self.send(method, params)
        seen = []
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("bridge closed stdout")
            message = json.loads(line)
            if message.get("id") == id:
                return message, seen
            seen.append(message)

    def tool(self, name, arguments=None, token=None):
        params = {"name": name, "arguments": arguments or {}}
        if token:
            params["_meta"] = {"progressToken": token}
        answer, seen = self.call("tools/call", params)
        return answer["result"], seen

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)

def text(result):
    return " ".join(c.get("text", "") for c in result.get("content", []))

# Connect (mode B): the bridge merges the door's connector tools, routes their
# calls to it with no box wake, and passes the door's list_changed on.
import os, urllib.request
def mock_connect(params):
    req = urllib.request.Request(os.environ["COCOONBOX_ENDPOINT"] + "/cocoonbox.mcp/connect", data=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "mock/connect", "params": params}).encode(),
                                 headers={"Authorization": "Bearer mock-token", "Content-Type": "application/json"}, method="POST")
    urllib.request.urlopen(req, timeout=10).read()

mock_connect({"grants": [{"id": "mockhub", "tools": [{"name": "list_issues", "read_only": True}, {"name": "create_issue"}]}]})
c = Bridge()
c.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "connect-client", "version": "1"}})
c.send("notifications/initialized", notify=True)
tools, _ = c.call("tools/list")
names = [t["name"] for t in tools["result"]["tools"]]
check("tools/list merges the connector tools after the box tools", len(names) == 19 and names[-2:] == ["mockhub_list_issues", "mockhub_create_issue"], names)
result, _ = c.tool("mockhub_list_issues", {"state": "open"})
check("a connector tool call goes to the Connect door", text(result) == 'mockhub_list_issues ran with {"state":"open"}', result)
check("and leaves the box hibernated", json.loads(run("status", "--json").stdout)["state"] == "hibernated")
time.sleep(0.5)
mock_connect({"grants": []})
changed, deadline = False, time.time() + 5
while not changed and time.time() < deadline:
    time.sleep(0.2)
    _, seen = c.call("ping")
    changed = any(m.get("method") == "notifications/tools/list_changed" for m in seen)
tools, _ = c.call("tools/list")
check("a disconnect reaches the client as list_changed, and the tools leave", changed and len(tools["result"]["tools"]) == 17, tools)
c.close()

b = Bridge()
init, _ = b.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "9"}})
check("initialize answered by the bridge", init["result"]["serverInfo"]["name"] == "cocoonbox")
b.send("notifications/initialized", notify=True)
tools, _ = b.call("tools/list")
names = [t["name"] for t in tools["result"]["tools"]]
check("tools/list while hibernated: 9 control + the 8 box tools every family serves", len(names) == 17 and "usage" in names and "box_ensure" in names and "box_create" in names and "computer_act" in names and not any(n.startswith("browser_") for n in names), names)
shell_tool = next(t for t in tools["result"]["tools"] if t["name"] == "shell")
check("box tools take a box argument", "box" in shell_tool["inputSchema"].get("properties", {}), shell_tool)
listed, _ = b.call("resources/list")
uris = [r["uri"] for r in listed["result"]["resources"]]
check("resources/list while hibernated lists none and wakes nothing", uris == [], listed)

result, _ = b.tool("box_status")
check("box_status via the control MCP", result["structuredContent"]["state"] == "hibernated", result)
result, _ = b.tool("usage")
check("usage via the control MCP, with no box", result["structuredContent"]["today"]["requests"] == 5 and "box" not in json.dumps(result["structuredContent"]), result)

started = time.time()
result, seen = b.tool("shell", {"command": "uname -a"}, token="t1")
progress = [m["params"]["message"] for m in seen if m.get("method") == "notifications/progress"]
changed = any(m.get("method") == "notifications/tools/list_changed" for m in seen)
check("first box-tool call wakes the box and runs", "Linux 6.12.0 (mockbox)" in text(result), result)
check("wake progress reaches the client with its token", any(p.startswith("waking:") for p in progress) and all(m["params"]["progressToken"] == "t1" for m in seen if m.get("method") == "notifications/progress"), progress)
check("tools/list_changed after the box's own list arrives", changed)
print(f"     wake + call took {time.time() - started:.1f} s; progress: {progress}")
result, seen = b.tool("shell", {"command": "sleep 360; echo done"}, token="t3")
progress = [m["params"] for m in seen if m.get("method") == "notifications/progress"]
check("the box's progress on a long call reaches the client with its token", [p["progressToken"] for p in progress] == ["t3"] and "sleep 360" in text(result), seen)

result, _ = b.tool("box_upgrade")
check("box_upgrade without confirm is refused", result.get("isError") and "confirm: true" in text(result), result)
result, seen = b.tool("box_save", token="t2")
check("box_save returns bytes", result["structuredContent"]["bytes"] > 0 and any(m.get("method") == "notifications/progress" for m in seen), result)
result, _ = b.tool("box_hibernate")
check("box_hibernate", result["structuredContent"]["state"] == "hibernated", result)
result, _ = b.tool("read_file", {"path": "/workspace/notes.md"})
check("a box tool after hibernate wakes the box again", "Notes" in text(result), result)
b.close()

time.sleep(1)
log = open(mocklog).read()
check("the box sees the client's own name", "rpc=initialize client=test-client" in log)
check("closing stdin ends the box session", "mcp DELETE /cocoonbox.mcp/cb-mock00000000" in log)
t = Bridge()
t.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "term-client", "version": "1"}})
t.send("notifications/initialized", notify=True)
time.sleep(2)
t.p.terminate()
t.p.wait(timeout=10)
time.sleep(1)
log = open(mocklog).read()
check("SIGTERM ends the box session too", log.count("mcp DELETE /cocoonbox.mcp/cb-mock00000000") >= 2, log[-400:])
import signal
c = Bridge()
c.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "int-client", "version": "1"}})
c.send("notifications/initialized", notify=True)
time.sleep(2)
c.p.send_signal(signal.SIGINT)
time.sleep(0.5)
c.p.kill()
c.p.wait(timeout=10)
time.sleep(1)
log = open(mocklog).read()
check("SIGINT then SIGKILL 0.5 s later, as Claude Code stops a server, ends the box session", log.count("mcp DELETE /cocoonbox.mcp/cb-mock00000000") >= 3, log[-400:])
h = Bridge()
h.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "hup-client", "version": "1"}})
h.send("notifications/initialized", notify=True)
time.sleep(2)
h.p.send_signal(signal.SIGHUP)
h.p.wait(timeout=10)
time.sleep(1)
log = open(mocklog).read()
check("SIGHUP, as a closed terminal sends, ends the box session", log.count("mcp DELETE /cocoonbox.mcp/cb-mock00000000") >= 4, log[-400:])
check("no request reached MockBox unauthorised", "auth=ok" in log and "auth=bad" not in log and "auth=missing" not in log)

subprocess.run([cli, "hibernate"], capture_output=True, timeout=60)
n = Bridge("--no-wake")
n.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test-client", "version": "9"}})
result, _ = n.tool("list_dir", {"path": "/"})
check("--no-wake refuses with a hint", result.get("isError") and "box_ensure" in text(result), result)
n.close()

flowwork = os.path.join(os.path.dirname(mocklog), "flows")
os.makedirs(flowwork)
r = run("flows")
check("flows lists an empty library", r.returncode == 0 and r.stdout.strip() == "no saved flows", r.stdout + r.stderr)
seed = os.path.join(flowwork, "seed.json")
with open(seed, "w") as f:
    json.dump({"version": 1, "flows": [
        {"name": "mousepad-save-fruit.json", "content": '{"name":"mousepad-save-fruit","steps":[]}', "image": "cbcore:v1"},
        {"name": "save-note.mts", "content": "export function main() {}\n"},
    ]}, f)
r = run("flows", "import", seed, "--json")
stored = json.loads(r.stdout)["flows"] if r.returncode == 0 else []
check("flows import stores each flow with its image", [(x["name"], x["rev"], x["image"]) for x in stored] == [("mousepad-save-fruit.json", 1, "cbcore:v1"), ("save-note.mts", 2, "")], r.stdout + r.stderr)
r = run("flows", "import", seed)
check("importing the same content keeps the revs", r.returncode == 0 and "imported mousepad-save-fruit.json as rev 1" in r.stdout, r.stdout + r.stderr)
r = run("flows", "list", "--json")
listed = json.loads(r.stdout)
check("flows list --json: newest first, no content", [x["name"] for x in listed["flows"]] == ["save-note.mts", "mousepad-save-fruit.json"] and "content" not in listed["flows"][0], r.stdout)
out = os.path.join(flowwork, "out.json")
r = run("flows", "export", "mousepad-save-fruit.json", "-o", out)
exported = json.load(open(out)) if r.returncode == 0 else {}
check("flows export -o writes the named flow with its content", exported.get("version") == 1 and [x["name"] for x in exported["flows"]] == ["mousepad-save-fruit.json"] and exported["flows"][0]["content"] == '{"name":"mousepad-save-fruit","steps":[]}' and "exported 1 flows" in r.stderr, r.stdout + r.stderr)
r = run("flows", "export")
check("flows export without names prints every flow", r.returncode == 0 and len(json.loads(r.stdout)["flows"]) == 2, r.stdout + r.stderr)
r = run("flows", "rm", "mousepad-save-fruit.json")
check("flows rm deletes it", r.returncode == 0 and r.stdout.strip() == "deleted mousepad-save-fruit.json", r.stdout + r.stderr)
r = run("flows", "rm", "mousepad-save-fruit.json")
check("flows rm of a missing flow fails, naming it", r.returncode == 1 and "no mousepad-save-fruit.json" in r.stderr, r.stderr)
r = run("flows", "export", "mousepad-save-fruit.json")
check("flows export of a missing flow fails", r.returncode == 1 and "not found" in r.stderr, r.stderr)
r = subprocess.run([cli, "flows", "import", "-"], input=open(out).read(), capture_output=True, text=True, timeout=60)
check("flows import - reads the export from stdin and brings the flow back", r.returncode == 0 and "imported mousepad-save-fruit.json as rev 4" in r.stdout, r.stdout + r.stderr)
bad = os.path.join(flowwork, "bad.json")
with open(bad, "w") as f:
    json.dump({"version": 1, "flows": [{"name": "../escape.json", "content": "{}"}]}, f)
r = run("flows", "import", bad)
check("flows import refuses a bad name", r.returncode == 1 and "escape" in r.stderr, r.stderr)
with open(bad, "w") as f:
    f.write("not json")
r = run("flows", "import", bad)
check("flows import refuses a file that is no export", r.returncode == 1 and "is no cocoonbox flows export" in r.stderr, r.stderr)

r = run("box", "schedule", "set", "main", "--cron", "0 9 * * *", "--tz", "Asia/Singapore", "--awake-for", "30m")
check("box schedule set prints the schedule", r.returncode == 0 and r.stdout.startswith('main cb-mock00000000 "0 9 * * *" Asia/Singapore, awake 30 min, next '), r.stdout + r.stderr)
r = run("box", "schedule", "list", "--json")
listed = json.loads(r.stdout)["schedules"] if r.returncode == 0 else []
check("box schedule list --json", [(x["name"], x["cron"], x["tz"], x["awake_for_s"]) for x in listed] == [("main", "0 9 * * *", "Asia/Singapore", 1800)], r.stdout + r.stderr)
r = run("box", "schedule", "set", "main", "--cron", "0 9 * * *", "--tz", "Mars/Olympus")
check("box schedule set with an unknown zone fails with the api's reason", r.returncode == 1 and "must name an IANA zone" in r.stderr, r.stderr)
r = run("box", "schedule", "set", "main", "--cron", "0 9 * * *", "--tz", "UTC", "--awake-for", "soon")
check("box schedule set refuses a bad --awake-for", r.returncode == 1 and "--awake-for is a duration" in r.stderr, r.stderr)
r = run("box", "schedule", "set", "main", "--cron", "0 9 * * *")
check("box schedule set without --tz prints the usage", r.returncode == 1 and "usage: cocoonbox box schedule" in r.stderr, r.stderr)
r = run("box", "schedule", "set", "nope", "--cron", "0 9 * * *", "--tz", "UTC")
check("box schedule set names a missing box", r.returncode == 1 and "you have no box nope" in r.stderr, r.stderr)
r = run("box", "schedule", "clear", "main")
check("box schedule clear", r.returncode == 0 and r.stdout.strip() == "cleared the wake schedule of main", r.stdout + r.stderr)
r = run("box", "schedule", "list")
check("box schedule list after clear", r.returncode == 0 and r.stdout.strip() == "no wake schedules", r.stdout + r.stderr)
check("box schedule leaves the box hibernated", json.loads(run("status", "--json").stdout)["state"] == "hibernated")

r = run("usage", "--json")
got = json.loads(r.stdout) if r.returncode == 0 else {}
check("usage --json", got.get("today", {}).get("cost_micros") == 4001500 and [(m["model"], m["requests"]) for m in got["today"]["models"]] == [("anthropic/claude-sonnet-5", 2), ("typesafe/jev-1.13", 3)] and (got["daily_cap_micros"], got["default_cap"], got["remaining_today_micros"]) == (5000000, True, 998500), r.stdout + r.stderr)
r = run("usage")
check("usage prints today, the month and the cap", r.returncode == 0 and r.stdout.splitlines() == ["today (UTC): 5 requests, 2030 tokens, $4.0015", "  anthropic/claude-sonnet-5: 2 requests, 1000 in, 100 out, $4.0000", "  typesafe/jev-1.13: 3 requests, 900 in, 30 out, $0.0015", "this month: 5 requests, 2030 tokens, $4.0015", "  anthropic/claude-sonnet-5: 2 requests, 1000 in, 100 out, $4.0000", "  typesafe/jev-1.13: 3 requests, 900 in, 30 out, $0.0015", "daily cap: $5.0000 (default), $0.9985 left today"], r.stdout + r.stderr)

def keys(*args, value=None):
    return subprocess.run([cli, "keys", *args], input=value, capture_output=True, text=True, timeout=60)

def release_instructions():
    r = Bridge(binary=release, env={**os.environ, "COCOONBOX_TOKEN": "mock-token"})
    try:
        init, _ = r.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "release-client", "version": "1"}})
    except RuntimeError:
        return None
    finally:
        r.p.stdin.close()
        r.p.wait(timeout=10)
    return init["result"]["instructions"]

secret = "synthetic-cli-0123456789"
r = keys("ls")
check("keys ls without keys", r.returncode == 0 and r.stdout.strip() == "no keys", r.stdout + r.stderr)
instructions = release_instructions()
check("a release-built bridge answers initialize without keys", instructions is not None and "API keys" not in instructions, instructions)
r = keys("add", "ATTIO_API_KEY", "--host", "api.attio.com,*.attio.com", "--json", value=secret + "\n")
added = json.loads(r.stdout) if r.returncode == 0 else {}
check("keys add reads the value from stdin and never prints it", added.get("hosts") == ["api.attio.com", "*.attio.com"] and added.get("hint") == "6789" and secret not in r.stdout + r.stderr, r.stdout + r.stderr)
r = keys("add", "ATTIO_API_KEY", "--host", "api.attio.com", value=secret)
check("keys add of a taken name fails with the api's reason", r.returncode == 1 and "already named" in r.stderr, r.stderr)
r = keys("add", "SERP", "--query", "api_key", value=secret)
check("keys add without --host prints the usage", r.returncode == 1 and "usage: cocoonbox keys" in r.stderr, r.stderr)
r = keys("add", "SERP", "--host", "serpapi.com", "--query", "api_key", value="\n")
check("keys add refuses an empty value", r.returncode == 1 and "no value read" in r.stderr, r.stderr)
r = keys("add", "SERP", "--host", "serpapi.com", "--query", "api_key", value="serp-synthetic-abcdefgh")
check("keys add sends a query key", r.returncode == 0 and "?api_key={}" in r.stdout and "efgh" in r.stdout, r.stdout + r.stderr)
instructions = release_instructions()
check("a release-built bridge names the keys at initialize", instructions is not None and "ATTIO_API_KEY for api.attio.com" in instructions and "SERP for serpapi.com as query ?api_key=$SERP" in instructions, instructions)
r = keys("ls")
check("keys ls shows names, hosts, send-as and the last 4", r.returncode == 0 and "ATTIO_API_KEY  api.attio.com,*.attio.com  Authorization: Bearer {}  \u20266789" in r.stdout and secret not in r.stdout, r.stdout + r.stderr)
r = keys("rotate", "ATTIO_API_KEY", value="synthetic-rotated-wxyz")
check("keys rotate takes the new value from stdin", r.returncode == 0 and "\u2026wxyz" in r.stdout, r.stdout + r.stderr)
r = keys("rm", "ATTIO_API_KEY")
check("keys rm", r.returncode == 0 and "revoke it at the vendor too" in r.stdout, r.stdout + r.stderr)
r = keys("rm", "ATTIO_API_KEY")
check("keys rm of a missing key fails", r.returncode == 1 and "NotFound" in r.stderr.replace("notFound", "NotFound"), r.stderr)
check("keys rm SERP", keys("rm", "SERP").returncode == 0)

r = run("devices", "--json")
devices = json.loads(r.stdout or "{}").get("devices", [])
check("devices --json lists the devices and marks this one", r.returncode == 0 and [d["client"] for d in devices] == ["cocoonbox-app", "cocoonbox-cli"] and [d["current"] for d in devices] == [False, True], r.stdout + r.stderr)
r = run("devices")
check("devices marks this one with *", r.returncode == 0 and "* dev_cli0000000000000  cocoonbox-cli  laptop" in r.stdout and "never used" in r.stdout, r.stdout + r.stderr)
r = run("devices", "revoke", "dev_app0000000000000")
check("devices revoke", r.returncode == 0 and "revoked dev_app0000000000000" in r.stdout and "dev_app" not in run("devices").stdout, r.stdout + r.stderr)
r = run("devices", "revoke", "dev_gone")
check("devices revoke of an unknown device fails", r.returncode == 1 and "device not found" in r.stderr, r.stderr)
r = run("devices", "revoke")
check("devices revoke without an id prints the usage", r.returncode == 1 and "usage: cocoonbox devices" in r.stderr, r.stderr)

headers = run("mcp-headers")
check("mcp-headers prints the Authorization header", json.loads(headers.stdout) == {"Authorization": "Bearer mock-token"}, headers.stdout)

ensure = run("ensure")
check("ensure over gRPC with progress on stderr", "waking" in ensure.stderr and "running" in ensure.stdout, ensure.stdout + ensure.stderr)
upgrade = run("upgrade")
check("upgrade without --yes refuses", upgrade.returncode != 0 and "--yes" in upgrade.stderr, upgrade.stderr)

# Several boxes: main runs here after `ensure`.
m = Bridge()
m.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "multi-client", "version": "1"}})
m.send("notifications/initialized", notify=True)
result, _ = m.tool("box_create", {"name": "test"})
check("box_create beside a running box is refused, naming it", result.get("isError") and "box main (cb-mock00000000) is running" in text(result), result)
result, _ = m.tool("box_hibernate")
check("box_hibernate without box acts on the default box, main", result["structuredContent"]["box_id"] == "cb-mock00000000" and result["structuredContent"]["state"] == "hibernated", result)
result, _ = m.tool("box_create", {"name": "test"})
check("box_create test", result.get("structuredContent", {}).get("name") == "test" and result["structuredContent"]["state"] == "running", result)
test_id = result.get("structuredContent", {}).get("box_id", "")
result, _ = m.tool("box_list")
listed = [(b["name"], b["state"]) for b in result["structuredContent"]["boxes"]]
check("box_list lists both, oldest first", listed == [("main", "hibernated"), ("test", "running")], listed)
result, _ = m.tool("shell", {"box": "test", "command": "uname -n"})
check("shell with box=test runs in test", "on test" in text(result), result)
result, _ = m.tool("shell", {"box": "main", "command": "uname -n"})
check("waking main while test runs is refused by the running limit", result.get("isError") and "box test (" + test_id + ") is running" in text(result), result)
result, _ = m.tool("shell", {"box": "cb-mock99999999", "command": "id"})
check("another user's box id is refused", result.get("isError") and "no box" in text(result), result)
m.close()

d = Bridge()
d.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "default-client", "version": "1"}})
result, _ = d.tool("shell", {"command": "uname -n"})
check("without box and --box, the default is the newest running box", "on test" in text(result), result)
d.close()

# The app's socket: the bridge names its agent, reports each box-tool call and
# takes the default box the app sends, first in answer to its hello, then on a change.
import os, socket, threading
class FakeApp:
    def __init__(self, path, default):
        self.default, self.hellos, self.calls, self.conns = default, [], [], []
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.bind(path)
        self.s.listen(4)
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        while True:
            try:
                c, _ = self.s.accept()
            except OSError:
                return
            self.conns.append(c)
            threading.Thread(target=self.read, args=(c,), daemon=True).start()

    def read(self, c):
        for line in c.makefile("r"):
            m = json.loads(line)
            if "box" in m:
                self.calls.append(m)
                continue
            self.hellos.append(m)
            c.sendall((json.dumps({"box": self.default}) + "\n").encode())

    def push(self, box):
        for c in self.conns:
            c.sendall((json.dumps({"box": box}) + "\n").encode())
        time.sleep(0.3)

    def close(self):
        for c in self.conns:
            c.close()
        self.s.close()

def wait_for(condition, seconds=5):
    deadline = time.time() + seconds
    while not condition() and time.time() < deadline:
        time.sleep(0.05)
    return condition()

app_sock = os.path.join(os.path.dirname(mocklog), "app.sock")
app = FakeApp(app_sock, test_id)
linked_env = dict(os.environ, COCOONBOX_APP_SOCKET=app_sock, COCOONBOX_AGENT="agent-1")
l = Bridge("--box", "main", env=linked_env)
l.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "linked-client", "version": "1"}})
check("the bridge names its agent on the app's socket", wait_for(lambda: app.hellos == [{"agent": "agent-1"}]), app.hellos)
time.sleep(0.3)
started = time.time() * 1000
result, _ = l.tool("shell", {"command": "uname -n"})
check("the default the app answers the hello with beats --box", "on test" in text(result), result)
result, _ = l.tool("shell", {"box": "test", "command": "uname -n"})
check("shell with box=test still runs in test", "on test" in text(result), result)
check("each routed box call is reported with agent, box id and time",
      wait_for(lambda: len(app.calls) == 2) and [(c["agent"], c["box"]) for c in app.calls] == [("agent-1", test_id)] * 2
      and all(started - 1000 <= c["at"] <= time.time() * 1000 for c in app.calls), app.calls)
app.push("cb-mock00000000")
result, _ = l.tool("box_status")
check("a default box pushed by the app reaches control tools with no restart", result["structuredContent"]["box_id"] == "cb-mock00000000", result)
l.tool("box_list")
l.tool("box_status", {"box": "test"})
time.sleep(0.3)
check("control tools are not reported, with or without box", len(app.calls) == 2, app.calls)
result, _ = l.tool("shell", {"command": "uname -n"})
check("and box tools: shell goes to hibernated main, refused by the running limit", result.get("isError") and "box test (" + test_id + ") is running" in text(result), result)
app.push(test_id)
result, _ = l.tool("shell", {"command": "uname -n"})
check("a second change also applies at the next call", "on test" in text(result), result)
check("box-tool reports follow the default", wait_for(lambda: len(app.calls) == 4) and [c["box"] for c in app.calls[2:]] == ["cb-mock00000000", test_id], app.calls)
app.close()
os.unlink(app_sock)
time.sleep(0.3)
result, _ = l.tool("shell", {"command": "uname -n"})
check("after the app quits, the bridge keeps working with the last default", "on test" in text(result), result)
l.close()

for label, env in (("with no socket", dict(os.environ, COCOONBOX_APP_SOCKET=app_sock, COCOONBOX_AGENT="agent-1")),
                   ("with the socket path unset", dict(os.environ, COCOONBOX_AGENT="agent-1"))):
    n = Bridge("--box", "test", env=env)
    n.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "unlinked-client", "version": "1"}})
    result, _ = n.tool("shell", {"command": "uname -n"})
    check("the bridge works " + label + ", with its --box", "on test" in text(result), result)
    n.close()

p = Bridge("--box", "main")
p.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pinned-client", "version": "1"}})
result, _ = p.tool("box_status")
check("--box main is the default of control tools", result["structuredContent"]["box_id"] == "cb-mock00000000", result)
result, _ = p.tool("box_hibernate", {"box": "test"})
check("box_hibernate with box=test", result["structuredContent"]["box_id"] == test_id and result["structuredContent"]["state"] == "hibernated", result)
result, _ = p.tool("shell", {"command": "uname -n"})
check("--box main wakes main for a box tool", "on main" in text(result), result)
result, _ = p.tool("box_delete", {"box": "test"})
check("box_delete without confirm is refused", result.get("isError") and "confirm: true" in text(result), result)
result, _ = p.tool("box_delete", {"confirm": True})
check("box_delete without box is refused", result.get("isError") and "Name the box" in text(result), result)
result, _ = p.tool("box_delete", {"box": "test", "confirm": True})
check("box_delete test", result.get("structuredContent", {}).get("box_id") == test_id, result)
result, _ = p.tool("box_list")
check("box_list after delete", [b["name"] for b in result["structuredContent"]["boxes"]] == ["main"], result)
p.close()

# box create: a new box runs the default image.
r = run("box", "create")
check("box create without a name prints its usage", r.returncode != 0 and "usage: cocoonbox box create" in r.stderr, r.stderr)
run("hibernate", "--box", "main", "--yes")
k = Bridge()
k.call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "create-client", "version": "1"}})
r = run("box", "create", "gen1", "--json")
created = json.loads(r.stdout or "{}")
check("box create keeps the default image", created.get("name") == "gen1" and created.get("image") == json.loads(run("status", "--json", "--box", "main").stdout)["latest_image"], r.stdout + r.stderr)
result, _ = k.tool("box_delete", {"box": "gen1", "confirm": True})
result, _ = k.tool("box_list")
check("box_list after the created box is deleted", [b["name"] for b in result["structuredContent"]["boxes"]] == ["main"], result)
r = run("box", "create", "phone", "--image", "cbdroid", "--json")
check("box create --image cbdroid runs the newest cbdroid image", json.loads(r.stdout or "{}").get("image") == "cbdroid:v1", r.stdout + r.stderr)
result, _ = k.tool("box_delete", {"box": "phone", "confirm": True})
r = run("box", "create", "phone", "--image")
check("box create --image without a value prints its usage", r.returncode != 0 and "usage: cocoonbox box create NAME [--image IMAGE]" in r.stderr, r.stderr)
r = run("box", "create", "phone", "--image", "--json")
check("box create --image followed by a flag prints its usage", r.returncode != 0 and "usage: cocoonbox box create NAME [--image IMAGE]" in r.stderr, r.stderr)
for form in (["--image="], ["--image=cbdroid"], ["--image", ""]):
    r = run("box", "create", "phone", *form)
    check("box create %s prints its usage and creates nothing" % " ".join(repr(f) for f in form), r.returncode != 0 and "usage: cocoonbox box create NAME [--image IMAGE]" in r.stderr, r.stderr)
result, _ = k.tool("box_list")
check("box_list after the refused --image forms has no phone", "phone" not in [b["name"] for b in result["structuredContent"]["boxes"]], result)
r = run("box", "create", "exact", "--image", "cbdroid:v1", "--json")
check("box create --image of an exact offered image runs it", json.loads(r.stdout or "{}").get("image") == "cbdroid:v1", r.stdout + r.stderr)
result, _ = k.tool("box_delete", {"box": "exact", "confirm": True})
r = run("box", "create", "phone", "--image", "nosuch")
check("box create --image of an image not offered fails with the api's reason", r.returncode != 0 and 'image "nosuch" is not offered' in r.stderr, r.stderr)
result, _ = k.tool("box_create", {"name": "phone", "image": "cbdroid"})
check("box_create with image cbdroid", result.get("structuredContent", {}).get("state") == "running", result)
result, _ = k.tool("box_list")
check("box_list shows the cbdroid box", [(b["name"], b["image"]) for b in result["structuredContent"]["boxes"]][1:] == [("phone", "cbdroid:v1")], result)
result, _ = k.tool("box_create", {"name": "other", "image": "nosuch"})
check("box_create of an image not offered is a tool error", result.get("isError") and 'image "nosuch" is not offered' in text(result), result)
result, _ = k.tool("box_delete", {"box": "phone", "confirm": True})
result, _ = k.tool("box_list")
check("box_list after the cbdroid box is deleted", [b["name"] for b in result["structuredContent"]["boxes"]] == ["main"], result)
k.close()

import os, urllib.request, urllib.error
def mcp_status(box):
    req = urllib.request.Request(os.environ["COCOONBOX_ENDPOINT"] + "/cocoonbox.mcp/" + box, data=b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}',
                                 headers={"Authorization": "Bearer mock-token", "Content-Type": "application/json"}, method="POST")
    try:
        return urllib.request.urlopen(req, timeout=10).status
    except urllib.error.HTTPError as e:
        return e.code
check("the edge refuses another user's box id with 404", mcp_status("cb-mock99999999") == 404)
time.sleep(1)
log = open(mocklog).read()
check("box tool calls reached each box's own MCP path", "/cocoonbox.mcp/" + test_id + " auth=ok" in log and "rpc=tools/call tool=shell" in log, log[-600:])

# rsh: the remote shell of rsync and git; the command's own exit code, or 255.
import shutil
work = os.path.dirname(mocklog)
workspace = os.path.realpath(os.path.join(work, "workspace"))
def rsh(*args, data=b"", env=None):
    return subprocess.run([cli, "rsh", *args], input=data, capture_output=True, timeout=120, env=env)

subprocess.run([cli, "hibernate", "--box", "main"], capture_output=True, timeout=60)
r = rsh("--box", "main", "echo out; echo err >&2; pwd; exit 7")
check("rsh wakes the box and runs the command", b"waking main" in r.stderr and json.loads(run("status", "--json", "--box", "main").stdout)["state"] == "running", r.stderr)
check("rsh keeps stdout and stderr apart and exits with the command's code", r.returncode == 7 and r.stdout == b"out\n" + workspace.encode() + b"\n" and r.stderr.endswith(b"err\n"), r)
r = rsh("main", "echo", "hosted")
check("without --box the host word names the box", r.returncode == 0 and r.stdout == b"hosted\n", r)
for host in ("box", "main", "u@main"):
    r = rsh("--box", "main", host, "echo", "dropped")
    check("with --box the host word " + host + " is dropped", r.returncode == 0 and r.stdout == b"dropped\n", r)
r = rsh("-t", "--box", "main", "echo", "plain")
check("-t without a terminal on stdin runs without one, with a notice", r.returncode == 0 and r.stdout == b"plain\n" and b"not a terminal" in r.stderr, r)
import pty
def rsh_on_pty(*args):
    primary, secondary = pty.openpty()
    p = subprocess.Popen([cli, "rsh", *args], stdin=secondary, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=dict(os.environ, TERM="xterm-256color"))
    os.close(secondary)
    out, err = p.communicate(timeout=60)
    os.close(primary)
    return p.returncode, out, err
def exec_lines():
    return [l for l in open(mocklog).read().splitlines() if l.startswith("exec ")]
want = len(subprocess.run(["infocmp", "-x", "xterm-256color"], capture_output=True).stdout)
seen = len(exec_lines())
code, out, err = rsh_on_pty("-t", "--box", "main", "echo", "tty")
check("-t on a terminal sends TERM and its local terminfo", code == 0 and exec_lines()[seen:] == [f"exec tty=true term=xterm-256color terminfo={want}"] and want > 1000, (exec_lines()[seen:], want, err))
seen = len(exec_lines())
code, out, err = rsh_on_pty("--box", "main", "echo", "plain")
check("without -t no terminal and no terminfo go along", code == 0 and exec_lines()[seen:] == ["exec tty=false term= terminfo=0"], (exec_lines()[seen:], err))
import signal as sig
for name, signals, want in (("the first SIGINT reaches the command", [sig.SIGINT], 9), ("a second signal ends rsh with 255", [sig.SIGINT, sig.SIGTERM], 255)):
    p = subprocess.Popen([cli, "rsh", "--box", "main", "trap 'echo got-int; exit 9' INT; trap '' TERM; echo ready; while :; do sleep 0.1; done" if want == 9 else "trap '' INT TERM; echo ready; sleep 30"],
                         stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    ready = p.stdout.readline()
    for s_ in signals:
        p.send_signal(s_)
        time.sleep(0.3)
    out, err = p.communicate(timeout=30)
    check(name, ready == b"ready\n" and p.returncode == want and (want != 9 or out == b"got-int\n"), (ready, out, err, p.returncode))
r = rsh("--box", "main", "-l", "u", "uname")
check("-l USER is ignored", r.returncode == 0 and r.stdout.strip() == os.uname().sysname.encode(), r)
data = os.urandom(3 << 20)
r = rsh("--box", "main", "cat", data=data)
check("3 MiB of binary stdin come back through cat unchanged", r.returncode == 0 and r.stdout == data, (len(r.stdout), r.stderr))
r = rsh("--box", "main", "wc", "-c", data=b"four")
check("the end of stdin reaches the command", r.returncode == 0 and r.stdout.strip() == b"4", r)
for label, args in (("an unknown box", ["--box", "nosuch", "true"]), ("an unknown option (git's -G probe)", ["-G", "main"]),
                    ("no command", ["--box", "main"]), ("no box", ["nosuch", "true"])):
    r = rsh(*args)
    check("rsh exits 255 on " + label, r.returncode == 255, r)
if shutil.which("rsync"):
    src = os.path.join(work, "src")
    os.makedirs(os.path.join(src, "sub"))
    open(os.path.join(src, "a.txt"), "w").write("a")
    open(os.path.join(src, "sub", "b.bin"), "wb").write(os.urandom(200_000))
    r = subprocess.run(["rsync", "-a", "-e", cli + " rsh --box main", src + "/", "box:dest/"], capture_output=True, timeout=120)
    dest = os.path.join(workspace, "dest")
    same = all(open(os.path.join(src, f), "rb").read() == open(os.path.join(dest, f), "rb").read() for f in ("a.txt", "sub/b.bin")) if r.returncode == 0 else False
    check("rsync -e 'cocoonbox rsh --box main' copies a tree", same, r.stderr)
else:
    print("SKIP rsync: not installed")
repo = os.path.join(work, "repo")
subprocess.run(["git", "init", "-q", repo], check=True)
subprocess.run(["git", "-C", repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "first"], check=True)
clone = os.path.join(work, "clone")
for variant in ("", "simple"):
    target = clone + variant
    env = dict(os.environ, GIT_SSH_COMMAND=cli + " rsh")
    if variant:
        env["GIT_SSH_VARIANT"] = variant
    r = subprocess.run(["git", "clone", "-q", "main:" + repo, target], capture_output=True, timeout=120, env=env)
    log = subprocess.run(["git", "-C", target, "log", "--format=%s"], capture_output=True, text=True).stdout if r.returncode == 0 else ""
    check("git clone with GIT_SSH_COMMAND='cocoonbox rsh'" + (" and GIT_SSH_VARIANT=" + variant if variant else ""), log.strip() == "first", r.stderr)

# offload and back through the CLI; MockBox's HOME is the box's, with the box packer stub on its PATH.
offrepo = os.path.join(work, "offrepo")
os.makedirs(offrepo)
def git_in(*args):
    return subprocess.run(["git", "-C", offrepo, "-c", "user.name=t", "-c", "user.email=t@t", *args], capture_output=True, text=True, check=True).stdout.strip()
git_in("init", "-q", "-b", "main")
open(os.path.join(offrepo, "a.txt"), "w").write("one\n")
git_in("add", ".")
git_in("commit", "-qm", "first")
open(os.path.join(offrepo, "a.txt"), "a").write("uncommitted\n")
def cb(*args, data=None):
    return subprocess.run([cli, *args], capture_output=True, text=True, timeout=120, cwd=offrepo, input=data)
path = cb("offload", "handover-path").stdout.strip()
check("offload handover-path names the repo key's directory", "/offload/" in path and path.endswith("/handover.md"), path)
headings = ["Goal and definition of done", "User preferences and constraints", "Current state", "Decisions made", "Files touched",
            "Next steps", "Commands", "Environment facts", "Gotchas learned", "Open questions"]
open(path, "w").write("# Hand-over: finish a.txt\n\nFrom: Mac\n\n" + "".join(f"## {h}\nnone\n\n" for h in headings[:-1]))
r = cb("offload", "--kind", "claude", "--box", "main")
check("offload refuses a hand-over missing a heading", r.returncode == 1 and "missing \"## Open questions\"" in r.stderr, r)
open(path, "a").write("## Open questions\nnone\n")
r = cb("offload", "--kind", "claude", "--box", "main")
check("offload copies the repo and starts the box session", r.returncode == 0 and "offloaded to main" in r.stdout and "tmux cb-off-" in r.stdout, r)
copy = next((l.split("copy ")[1].split(",")[0] for l in r.stdout.splitlines() if "copy " in l), "")
check("the box copy holds the uncommitted change", copy and open(os.path.join(copy, "a.txt")).read() == "one\nuncommitted\n", copy)
r = cb("offload", "status", "--json")
check("offload status shows the session working", r.returncode == 0 and json.loads(r.stdout)["sessions"][0]["status"] == "working", r)
r = cb("offload", "send", "also", "fix", "b")
check("offload send reaches the box agent", r.returncode == 0 and "sent to the box agent: pasted" in r.stdout, r)
r = cb("offload", "inbox")
check("offload inbox with nothing posted", r.returncode == 0 and "nothing new from the box agent (working)" in r.stdout, r)
r = cb("offload", "--kind", "claude", "--box", "main", "--json")
check("a second offload is refused with exit 3 and the box's reason", r.returncode == 3 and "bring the last offload back first" in r.stdout, r)
r = run("hibernate")
check("hibernate refuses while the box agent works", r.returncode == 1 and "a box agent is working on offrepo (claude)" in r.stderr, r.stderr)
r = run("upgrade")
check("upgrade names the working box agent", r.returncode == 1 and "restarts it from its last hand-over" in r.stderr, r.stderr)
open(os.path.join(work, "box", "stub-auto-back"), "w").close()
r = cb("back")
check("back fetches, lands and prints the box agent's hand-over", r.returncode == 0 and r.stdout.startswith("unchanged") and "# Hand-over: finish a.txt" in r.stdout, r)
check("back leaves the uncommitted change", open(os.path.join(offrepo, "a.txt")).read() == "one\nuncommitted\n")
r = cb("offload", "status", "--json")
check("after back the session is stopped", json.loads(r.stdout)["sessions"][0]["status"] == "stopped", r)
r = cb("back")
check("back with nothing in the box fails and names the last hand-over", r.returncode == 1 and "nothing of this repo is in a box" in r.stderr and path.replace("handover.md", "back.md") in r.stderr, r)
r = cb("offload", "--handover-prompt", "--kind", "codex")
check("offload --handover-prompt names the hand-over path, the offload and the template", r.returncode == 0 and r.stdout.startswith("OFFLOAD: ") and f"to {path} " in r.stdout and " offload --kind codex" in r.stdout and "## Open questions" in r.stdout, r)
r = subprocess.run([os.path.basename(cli), "offload", "--kind", "claude", "--box", "main"], capture_output=True, text=True, timeout=120, cwd=offrepo,
                   env={**os.environ, "PATH": os.path.dirname(cli) + ":" + os.environ["PATH"]})
check("an offload run by its name on PATH copies the repo", r.returncode == 0 and "offloaded to main" in r.stdout, r)
r = cb("offload", "inbox")
check("offload inbox leaves out the last offload's entries", r.returncode == 0 and "nothing new from the box agent" in r.stdout, r)
r = cb("back")
check("the second offload comes back", r.returncode == 0 and r.stdout.startswith("unchanged"), r)
r = cb("offload", "secrets", "add", "file:.env")
check("offload secrets add records the entry", r.returncode == 0 and r.stdout.strip() == "file:.env", r)
r = cb("offload", "secrets", "remove", "file:.env")
check("offload secrets remove", r.stdout.strip() == "no secrets opted in", r)

# App sign-in: a CLI with no item of its own copies the app's. The app's item is
# the CLI's own under a service one ".cli" shorter, so the CLI writes every item
# and each item's ACL trusts the binary that reads it: no Keychain prompt.
if not os.environ.get("CB_KEYCHAIN_FILE", "").startswith(os.path.dirname(mocklog)):
    sys.exit("refusing to run the sign-in tests outside the throwaway keychain")
def as_service(service, *args, data=None, binary=None):
    return subprocess.run([binary or cli, *args], input=data, capture_output=True, text=True, timeout=60, env=dict(os.environ, CB_KEYCHAIN_SERVICE=service))
def app_service(name):
    seed = os.environ["CB_KEYCHAIN_SERVICE"] + "." + name
    login = as_service(seed, "login", data="mock-token\n")
    check("the app's item for " + name, login.returncode == 0, login.stderr)
    return seed + ".cli"
def signed_in_alone(service):
    as_service(service.removesuffix(".cli"), "logout")
    alone = as_service(service, "status", "--json")
    return alone.returncode == 0 and alone.stderr == ""

service = os.environ["CB_KEYCHAIN_SERVICE"] + ".none.cli"
none = as_service(service, "status", "--json")
check("with no sign-in at all the CLI says to log in", none.returncode != 0 and "run cocoonbox login" in none.stderr and "--from-app" not in none.stderr, none.stderr)
bundled = os.path.join(work, "CocoonBox.app", "Contents", "MacOS", "cocoonbox-cli")
os.makedirs(os.path.dirname(bundled))
shutil.copy2(cli, bundled)
none = as_service(service, "status", "--json", binary=bundled)
check("the CLI in the app's bundle also suggests login --from-app", none.returncode != 0 and "cocoonbox login --from-app" in none.stderr, none.stderr)
service = app_service("status")
via_app = as_service(service, "status", "--json")
check("with only the app's item a command works and says so once", via_app.returncode == 0 and via_app.stderr.count("using the CocoonBox app's sign-in") == 1, via_app.stderr)
check("and the CLI keeps it without the app's item", signed_in_alone(service))

# Writes stay inside the HOME the CLI is given: every file command runs under a
# sandbox that denies writes outside the test directory and the process temp.
import os, hashlib, pathlib
work = os.path.dirname(mocklog)
home = os.path.join(work, "confined-home")
os.makedirs(os.path.join(home, ".cursor"))
profile = os.path.join(work, "confine.sb")
open(profile, "w").write(f"""(version 1)
(allow default)
(deny file-write* (require-all (require-not (subpath "{os.path.realpath(work)}")) (require-not (subpath "/private/var/folders")) (require-not (subpath "/dev"))))
""")
def confined(*args, extra=None):
    env = dict(os.environ, HOME=home, **(extra or {}))
    env.pop("COCOONBOX_CONFIG_DIR", None)
    env.pop("COCOONBOX_CACHE_DIR", None)
    return subprocess.run(["sandbox-exec", "-f", profile, cli, *args], capture_output=True, text=True, timeout=60, env=env)

cursor = os.path.join(home, ".cursor", "mcp.json")
open(cursor, "w").write(json.dumps({"mcpServers": {"other": {"command": "other-server", "args": ["x"]}}, "keep": 1}))
done = confined("setup", "cursor")
merged = json.load(open(cursor))
check("setup cursor merges into an existing mcp.json", done.returncode == 0 and merged["mcpServers"]["other"] == {"command": "other-server", "args": ["x"]} and merged["keep"] == 1 and merged["mcpServers"]["cocoonbox"]["args"] == ["mcp"], done.stderr + json.dumps(merged))
jsonc = '{\n  // a comment\n  "mcpServers": {"other": {"command": "other-server"}}\n}\n'
open(cursor, "w").write(jsonc)
refused = confined("setup", "cursor")
check("setup cursor leaves a file it cannot parse unchanged", refused.returncode != 0 and open(cursor).read() == jsonc, refused.stderr)
for args in (["plugin", "export", os.path.join(home, "plugins")], ["install"]):
    ran = confined(*args)
    check("confined: " + " ".join(args[:2]), ran.returncode == 0, ran.stderr)
check("install links inside the given HOME", os.path.islink(os.path.join(home, ".local", "bin", "cocoonbox")))
if shutil.which("claude"):
    env = dict(os.environ, HOME=home, CLAUDE_CONFIG_DIR=os.path.join(home, ".claude"))
    for target in ("plugins", "plugins/cocoonbox"):
        r = subprocess.run(["claude", "plugin", "validate", "--strict", os.path.join(home, target)], capture_output=True, text=True, timeout=120, env=env)
        check("claude plugin validate --strict " + target, r.returncode == 0 and "Validation passed" in r.stdout, r.stdout + r.stderr)
    setups = [confined("setup", "claude", extra={"CLAUDE_CONFIG_DIR": env["CLAUDE_CONFIG_DIR"]}) for _ in range(2)]
    installed = json.load(open(os.path.join(home, ".claude", "plugins", "installed_plugins.json")))["plugins"].get("cocoonbox@cocoonbox", [])
    check("setup claude installs the plugin into the given CLAUDE_CONFIG_DIR", setups[0].returncode == 0 and len(installed) == 1 and installed[0]["version"] == "0.16.0", setups[0].stderr + json.dumps(installed))
    check("setup claude again changes nothing and succeeds", setups[1].returncode == 0 and "already installed" in setups[1].stdout + setups[1].stderr and "already at the latest version" in setups[1].stdout + setups[1].stderr, setups[1].stdout + setups[1].stderr)
    service = app_service("setup")
    adopted = confined("setup", "claude", extra={"CLAUDE_CONFIG_DIR": env["CLAUDE_CONFIG_DIR"], "CB_KEYCHAIN_SERVICE": service})
    check("setup claude copies the app's sign-in to the CLI", adopted.returncode == 0 and "using the CocoonBox app's sign-in" in adopted.stderr, adopted.stdout + adopted.stderr)
    check("and the CLI keeps it without the app's item", signed_in_alone(service))
else:
    print("SKIP claude plugin validate: Claude Code is not installed")

# Browser sign-in: BROWSER plays the browser. It follows the consent page's URL to MockBox, which allows at once and
# sends it back to the CLI's loopback port with a code, which the CLI redeems with PKCE; logout revokes the token.
stub = os.path.join(os.path.dirname(mocklog), "stub-browser")
open(stub, "w").write('#!/bin/sh\nexec curl -fsSL -o /dev/null "$1"\n')
os.chmod(stub, 0o755)
seen = len(open(mocklog).read())
r = subprocess.run([cli, "login", "--browser"], capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL, env=dict(os.environ, BROWSER=stub))
log = open(mocklog).read()[seen:]
check("login --browser signs in through the consent page", r.returncode == 0 and "signed in to" in r.stdout and "authorize=allowed device=" in log and "token=issued" in log, r.stdout + r.stderr + log)
check("the browser sign-in's token works", run("status", "--json").returncode == 0)
r = run("logout")
check("logout revokes the token at the server", r.returncode == 0 and "revoked=mock-token" in open(mocklog).read()[seen:], r.stderr)
r = run("status", "--json")
check("after logout the CLI is signed out", r.returncode != 0 and "run cocoonbox login" in r.stderr, r.stderr)
r = subprocess.run([cli, "login", "--token", "-"], input="mock-token\n", capture_output=True, text=True, timeout=30)
seen = len(open(mocklog).read())
r = run("logout", "--everywhere")
check("logout --everywhere signs out everywhere at the server and here", r.returncode == 0 and "signed out everywhere" in r.stdout and "account=signed-out-everywhere" in open(mocklog).read()[seen:] and "run cocoonbox login" in run("status").stderr, r.stdout + r.stderr)
# A token copied from the app is the app's device: the CLI's logout deletes its own item and revokes nothing.
for how in ("login --from-app", "an implicit copy"):
    service = app_service("revoke-" + how.split()[-1])
    seen = len(open(mocklog).read())
    copied = as_service(service, *(["login", "--from-app"] if how.startswith("login") else ["status", "--json"]))
    out = as_service(service, "logout")
    check("logout after " + how + " revokes nothing", copied.returncode == 0 and out.returncode == 0 and "revoked=" not in open(mocklog).read()[seen:], copied.stderr + out.stderr)
# --invite signs up through the browser: the consent page's URL carries the code on to the GitHub signup.
seen = len(open(mocklog).read())
r = subprocess.run([cli, "login", "--invite", "cbi_test"], capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL, env=dict(os.environ, BROWSER=stub))
log = open(mocklog).read()[seen:]
check("login --invite carries the invite on the consent page", r.returncode == 0 and "signed in to" in r.stdout and "authorize=allowed" in log and "invite=yes" in log and "&invite=cbi_test" in log, r.stdout + r.stderr + log)
r = subprocess.run([cli, "login", "--invite", "cbi_test", "--token", "-"], capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL)
check("login --invite refuses --token", r.returncode != 0 and "--invite signs up through the browser" in r.stderr, r.stderr)

# invite create and list read an invite-scoped token from the environment or a file, never from argv.
noenv = {k: v for k, v in os.environ.items() if not k.startswith("COCOONBOX_INVITE_TOKEN")}
def invite(*args, extra=None):
    return subprocess.run([cli, "invite", *args], capture_output=True, text=True, timeout=30, env=dict(noenv, **(extra or {})))
seen = len(open(mocklog).read())
r = invite("create", "--days", "3", extra={"COCOONBOX_INVITE_TOKEN": "mock-invite-token"})
check("invite create prints a one-use link", r.returncode == 0 and r.stdout.splitlines()[0] == os.environ["COCOONBOX_ENDPOINT"] + "/cb/?invite=cbi_mock" and "one use" in r.stdout and "invites=created days=3" in open(mocklog).read()[seen:], r.stdout + r.stderr)
tokenfile = os.path.join(os.path.dirname(mocklog), "invite-token")
open(tokenfile, "w").write("mock-invite-token\n")
r = invite("list", "--token-file", tokenfile)
check("invite list with a token file", r.returncode == 0 and "inv_mock" in r.stdout and "uses left 1" in r.stdout, r.stdout + r.stderr)
r = invite("create", "--json", extra={"COCOONBOX_INVITE_TOKEN_FILE": tokenfile})
check("invite create --json with the token file from the environment", r.returncode == 0 and json.loads(r.stdout)["code"] == "cbi_mock", r.stdout + r.stderr)
r = invite("create")
check("invite create without a token says where to put one", r.returncode != 0 and "no invite token" in r.stderr, r.stderr)
r = invite("create", extra={"COCOONBOX_INVITE_TOKEN": "cbit_wrong"})
check("invite create with a wrong token shows the api's refusal", r.returncode != 0 and "invalid invite token" in r.stderr, r.stderr)
r = invite("create", "--days", "9", extra={"COCOONBOX_INVITE_TOKEN": "mock-invite-token"})
check("invite create past 7 days shows the api's refusal", r.returncode != 0 and "1 to 7 days" in r.stderr, r.stderr)
r = subprocess.run([cli, "login", "--token", "abc"], capture_output=True, text=True, timeout=30, stdin=subprocess.DEVNULL)
check("login --token takes only -", r.returncode != 0 and "--token takes -" in r.stderr, r.stderr)
r = subprocess.run([cli, "login", "--token", "-"], input="mock-token\n", capture_output=True, text=True, timeout=30)
check("login --token - reads a pasted token", r.returncode == 0 and "signed in to" in r.stdout, r.stderr)

print(f"{len(failures)} failed" if failures else "all passed")
sys.exit(1 if failures else 0)
PY
