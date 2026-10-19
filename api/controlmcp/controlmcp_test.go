package controlmcp

import (
	"bufio"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"runtime/debug"
	"slices"
	"strings"
	"sync"
	"testing"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	"github.com/loomhand/cocoonbox/api/boxes"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/mcpwire"
)

const (
	aliceToken = "alice-token"
	bobToken   = "bob-token"
	carolToken = "carol-token"

	latestImage = "img:2"
)

func TestInitializeAndToolsList(t *testing.T) {
	srv, _ := newTestServer(t)

	for requested, want := range map[string]string{"2025-06-18": "2025-06-18", "2025-11-25": "2025-11-25", "2024-11-05": "2025-11-25"} {
		rec := serve(srv, http.MethodPost, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"`+requested+`","clientInfo":{"name":"cocoonbox"}}}`)
		init := decode(t, rec)
		info, _ := init.Result["serverInfo"].(map[string]any)
		caps, _ := init.Result["capabilities"].(map[string]any)
		if init.Result["protocolVersion"] != want || info["name"] != "cocoonbox-control" || caps["tools"] == nil || rec.Header().Get("Mcp-Session-Id") == "" {
			t.Fatalf("initialize %s = %+v, session %q", requested, init, rec.Header().Get("Mcp-Session-Id"))
		}
	}
	list := post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":2,"method":"tools/list"}`)
	var names []string
	for _, tool := range list.Result["tools"].([]any) {
		spec := tool.(map[string]any)
		if spec["inputSchema"].(map[string]any)["type"] != "object" || spec["outputSchema"].(map[string]any)["type"] != "object" || spec["description"] == "" {
			t.Errorf("%v: incomplete spec %v", spec["name"], spec)
		}
		names = append(names, spec["name"].(string))
	}
	if want := []string{"box_list", "box_status", "box_ensure", "box_create", "box_hibernate", "box_save", "box_upgrade", "box_delete", "usage"}; !slices.Equal(names, want) {
		t.Fatalf("tools = %v, want %v", names, want)
	}
	if ping := post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":3,"method":"ping"}`); ping.Error != nil || ping.Result == nil {
		t.Fatalf("ping = %+v", ping)
	}
}

func TestBuildVersion(t *testing.T) {
	rev := "71e9ca3cdd4816ead9287321d39ddde9c13570aa"
	for _, tc := range []struct {
		settings []debug.BuildSetting
		want     string
	}{
		{nil, "(devel)"},
		{[]debug.BuildSetting{{Key: "vcs.modified", Value: "true"}}, "(devel)"},
		{[]debug.BuildSetting{{Key: "vcs.revision", Value: rev}, {Key: "vcs.modified", Value: "false"}}, "71e9ca3cdd48"},
		{[]debug.BuildSetting{{Key: "vcs.revision", Value: rev}, {Key: "vcs.modified", Value: "true"}}, "71e9ca3cdd48-dirty"},
		{[]debug.BuildSetting{{Key: "vcs.revision", Value: "abc"}}, "abc"},
	} {
		if got := buildVersion(tc.settings); got != tc.want {
			t.Errorf("buildVersion(%v) = %q, want %q", tc.settings, got, tc.want)
		}
	}
}

func TestTransportRejects(t *testing.T) {
	srv, fake := newTestServer(t)
	cases := []struct {
		name, method, token, body string
		want                      int
	}{
		{"no token", http.MethodPost, "", `{"jsonrpc":"2.0","id":1,"method":"tools/list"}`, http.StatusUnauthorized},
		{"unknown token", http.MethodPost, "mallory", `{"jsonrpc":"2.0","id":1,"method":"tools/list"}`, http.StatusUnauthorized},
		{"unauthenticated call", http.MethodPost, "", `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"box_hibernate"}}`, http.StatusUnauthorized},
		{"get stream", http.MethodGet, aliceToken, "", http.StatusMethodNotAllowed},
		{"delete session", http.MethodDelete, aliceToken, "", http.StatusMethodNotAllowed},
		{"parse error", http.MethodPost, aliceToken, `{"jsonrpc":`, http.StatusBadRequest},
		{"batch", http.MethodPost, aliceToken, `[{"jsonrpc":"2.0","id":1,"method":"ping"}]`, http.StatusBadRequest},
		{"notification", http.MethodPost, aliceToken, `{"jsonrpc":"2.0","method":"notifications/initialized"}`, http.StatusAccepted},
	}
	for _, tc := range cases {
		rec := serve(srv, tc.method, tc.token, tc.body)
		if rec.Code != tc.want {
			t.Errorf("%s: status %d, want %d (%s)", tc.name, rec.Code, tc.want, rec.Body)
		}
		if tc.want == http.StatusUnauthorized && rec.Body.Len() != 0 {
			t.Errorf("%s: 401 carries a body %q", tc.name, rec.Body)
		}
	}
	if calls := fake.recorded(); len(calls) != 0 {
		t.Fatalf("rejected requests reached Boxes: %v", calls)
	}
}

func TestUnknownMethodAndTool(t *testing.T) {
	srv, fake := newTestServer(t)

	for body, want := range map[string]int{
		`{"jsonrpc":"2.0","id":1,"method":"resources/list"}`:                                                       mcpwire.CodeMethodNotFound,
		`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"shell","arguments":{"command":"id"}}}`:    mcpwire.CodeInvalidParams,
		`{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"box_ensure","arguments":{"wait":"yes"}}}`: mcpwire.CodeInvalidParams,
		`{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"box_status","arguments":[]}}`:             mcpwire.CodeInvalidParams,
	} {
		rec := serve(srv, http.MethodPost, aliceToken, body)
		var raw map[string]json.RawMessage
		if err := json.Unmarshal(rec.Body.Bytes(), &raw); err != nil {
			t.Fatalf("decode %s: %v", rec.Body, err)
		}
		var rpcErr mcpwire.Error
		if _, hasResult := raw["result"]; hasResult || json.Unmarshal(raw["error"], &rpcErr) != nil || rpcErr.Code != want {
			t.Errorf("%s = %s, want error %d without a result", body, rec.Body, want)
		}
	}
	if calls := fake.recorded(); len(calls) != 0 {
		t.Fatalf("refused calls reached Boxes: %v", calls)
	}
}

func TestUpgradeRequiresConfirm(t *testing.T) {
	srv, fake := newTestServer(t)

	for _, args := range []string{``, `,"arguments":{}`, `,"arguments":{"confirm":false}`} {
		resp := post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"box_upgrade"`+args+`}}`)
		if got := refusal(t, resp); got != string(errUpgradeUnconfirmed) {
			t.Fatalf("upgrade%s = %q", args, got)
		}
	}
	if slices.ContainsFunc(fake.recorded(), func(c fakeCall) bool { return c.method == "Rebase" }) {
		t.Fatalf("unconfirmed upgrade called Rebase: %v", fake.recorded())
	}

	resp := post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"box_upgrade","arguments":{"confirm":true}}}`)
	got := structured(t, resp)
	if got["box_id"] != boxes.BoxID("alice") || got["state"] != "running" || got["image"] != latestImage || got["operation_id"] == "" {
		t.Fatalf("confirmed upgrade = %v", got)
	}
	if !slices.Contains(fake.recorded(), fakeCall{method: "Rebase", user: "alice", box: boxes.BoxID("alice")}) {
		t.Fatalf("confirmed upgrade calls = %v", fake.recorded())
	}
	resp = post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"box_upgrade","arguments":{"confirm":true}}}`)
	if got := refusal(t, resp); got != string(errUpgradeCurrent) {
		t.Fatalf("upgrade on the latest image = %q", got)
	}
}

func TestToolsActOnlyOnCallersBox(t *testing.T) {
	srv, fake := newTestServer(t)
	alice := boxes.BoxID("alice")

	for _, call := range []string{
		`{"name":"box_status"}`,
		`{"name":"box_ensure"}`,
		`{"name":"box_save"}`,
		`{"name":"box_upgrade","arguments":{"confirm":true}}`,
		`{"name":"box_hibernate"}`,
	} {
		got := structured(t, post(t, srv, bobToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":`+call+`}`))
		if got["box_id"] != boxes.BoxID("bob") {
			t.Fatalf("bob %s reported box %v", call, got["box_id"])
		}
	}
	for _, call := range []string{
		`{"name":"box_status","arguments":{"box_id":"` + alice + `"}}`,
		`{"name":"box_hibernate","arguments":{"box_id":"` + alice + `"}}`,
		`{"name":"box_save","arguments":{"box_id":"` + alice + `"}}`,
		`{"name":"box_upgrade","arguments":{"confirm":true,"box_id":"` + alice + `"}}`,
		`{"name":"box_upgrade","arguments":{"confirm":true,"image":"img:1"}}`,
	} {
		resp := post(t, srv, bobToken, `{"jsonrpc":"2.0","id":2,"method":"tools/call","params":`+call+`}`)
		if resp.Error == nil || resp.Error.Code != mcpwire.CodeInvalidParams {
			t.Fatalf("bob %s = %+v, want invalid params", call, resp)
		}
	}
	for _, tool := range []string{"box_status", "box_ensure", "box_hibernate", "box_save", "box_upgrade", "box_delete"} {
		call := `{"name":"` + tool + `","arguments":{"confirm":true,"box":"` + alice + `"}}`
		if tool == "box_status" || tool == "box_ensure" || tool == "box_hibernate" || tool == "box_save" {
			call = `{"name":"` + tool + `","arguments":{"box":"` + alice + `"}}`
		}
		if got := refusal(t, post(t, srv, bobToken, `{"jsonrpc":"2.0","id":3,"method":"tools/call","params":`+call+`}`)); !strings.Contains(got, "You have no box") {
			t.Fatalf("bob %s on alice's box = %q", tool, got)
		}
	}
	for _, c := range fake.recorded() {
		if c.user != "bob" || c.box == alice {
			t.Fatalf("bob's token reached %+v", c)
		}
	}
}

func TestBoxCreatePassesTheImage(t *testing.T) {
	srv, fake := newTestServer(t)
	for _, tt := range []struct{ args, image string }{
		{`{"name":"plain"}`, ""},
		{`{"name":"phone","image":"cbdroid"}`, "cbdroid"},
	} {
		structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"box_create","arguments":`+tt.args+`}}`))
		fake.mu.Lock()
		got := fake.extra["alice"][len(fake.extra["alice"])-1]
		fake.mu.Unlock()
		if got.GetImage() != tt.image {
			t.Errorf("box_create %s created %v, want image %q", tt.args, got, tt.image)
		}
	}
	resp := post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"box_create","arguments":{"name":"x","image":"nosuch"}}}`)
	if got := refusal(t, resp); got != `image "nosuch" is not offered` {
		t.Errorf("box_create of an image not offered = %q", got)
	}
}

func TestBoxArgumentPicksTheBox(t *testing.T) {
	srv, fake := newTestServer(t)
	created := structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"box_create","arguments":{"name":"test"}}}`))
	if created["box_id"] != "cb-test" || created["name"] != "test" || created["operation_id"] == "" {
		t.Fatalf("box_create = %v", created)
	}
	fake.mu.Lock()
	fake.extra["alice"][0].State = controlv1.BoxState_BOX_STATE_RUNNING
	fake.mu.Unlock()
	list := structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"box_list"}}`))
	boxesList, _ := list["boxes"].([]any)
	if len(boxesList) != 2 || boxesList[1].(map[string]any)["name"] != "test" || boxesList[1].(map[string]any)["state"] != "running" {
		t.Fatalf("box_list = %v", list)
	}
	for _, ref := range []string{"test", "cb-test"} {
		got := structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"box_status","arguments":{"box":"`+ref+`"}}}`))
		if got["box_id"] != "cb-test" {
			t.Fatalf("box_status of %s = %v", ref, got)
		}
	}
	structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"box_save","arguments":{"box":"test"}}}`))
	structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"box_ensure","arguments":{"box":"test"}}}`))
	if got := refusal(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"box_delete","arguments":{"box":"test"}}}`)); got != string(errDeleteUnconfirmed) {
		t.Fatalf("unconfirmed delete = %q", got)
	}
	if got := refusal(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"box_delete","arguments":{"confirm":true}}}`)); got != string(errDeleteUnnamed) {
		t.Fatalf("delete without a box = %q", got)
	}
	deleted := structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"box_delete","arguments":{"box":"test","confirm":true}}}`))
	if deleted["box_id"] != "cb-test" || deleted["operation_id"] == "" {
		t.Fatalf("box_delete = %v", deleted)
	}
	want := []fakeCall{{"Create", "alice", "test"}, {"SaveState", "alice", "cb-test"}, {"Ensure", "alice", "cb-test"}, {"Delete", "alice", "cb-test"}}
	if got := fake.recorded(); !slices.Equal(got, want) {
		t.Fatalf("calls = %v, want %v", got, want)
	}
}

func TestABoxWithoutAMachineIsAbsent(t *testing.T) {
	srv, fake := newTestServer(t)
	fake.mu.Lock()
	fake.states["alice"], fake.images["alice"] = controlv1.BoxState_BOX_STATE_UNSPECIFIED, ""
	fake.mu.Unlock()
	got := structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"box_status"}}`))
	if got["state"] != "absent" || got["upgrade_available"] != false || got["name"] != "main" {
		t.Fatalf("status of a box without a machine = %v", got)
	}
	if text := refusal(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"box_save"}}`)); text != "The box is absent." {
		t.Fatalf("save of a box without a machine = %q", text)
	}
}

func TestEnsureStreamsProgress(t *testing.T) {
	srv, fake := newTestServer(t)
	fake.setState("alice", controlv1.BoxState_BOX_STATE_HIBERNATED)
	got := structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"box_status"}}`))
	want := map[string]any{"box_id": boxes.BoxID("alice"), "name": "main", "state": "hibernated", "image": "img:1", "latest_image": latestImage, "upgrade_available": true, "last_saved_at_ms": float64(0)}
	if !mapsEqual(got, want) {
		t.Fatalf("status = %v, want %v", got, want)
	}

	rec := serve(srv, http.MethodPost, aliceToken, `{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"box_ensure","_meta":{"progressToken":"tok"}}}`)
	if rec.Code != http.StatusOK || rec.Header().Get("Content-Type") != "text/event-stream" {
		t.Fatalf("status %d, content type %q", rec.Code, rec.Header().Get("Content-Type"))
	}
	msgs := sseMessages(t, rec.Body.String())
	if len(msgs) != 3 {
		t.Fatalf("got %d messages, want 2 progress + 1 response: %v", len(msgs), msgs)
	}
	for i, message := range []string{"waking: restoring memory snapshot", "done: running"} {
		params, _ := msgs[i]["params"].(map[string]any)
		if msgs[i]["method"] != "notifications/progress" || params["progressToken"] != "tok" || params["progress"] != float64(i+1) || params["message"] != message {
			t.Fatalf("progress %d = %v", i, msgs[i])
		}
		if _, ok := params["total"]; ok {
			t.Fatalf("progress %d carries a total: %v", i, params)
		}
	}
	final := msgs[2]["result"].(map[string]any)["structuredContent"].(map[string]any)
	if msgs[2]["id"] != float64(7) || final["state"] != "running" || final["operation_id"] == "" {
		t.Fatalf("response = %v", msgs[2])
	}

	got = structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"box_ensure"}}`))
	if got["state"] != "running" || got["operation_id"] != "" {
		t.Fatalf("ensure of a running box = %v", got)
	}
	fake.setState("alice", controlv1.BoxState_BOX_STATE_HIBERNATED)
	got = structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"box_ensure","arguments":{"wait":false}}}`))
	if got["state"] != "hibernated" || got["operation_id"] == "" {
		t.Fatalf("ensure without wait = %v", got)
	}
}

func TestSaveReportsBytesAndRefusals(t *testing.T) {
	srv, fake := newTestServer(t)

	rec := serve(srv, http.MethodPost, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"box_save","_meta":{"progressToken":5}}}`)
	msgs := sseMessages(t, rec.Body.String())
	if len(msgs) != 3 || msgs[0]["params"].(map[string]any)["message"] != "saving: uploading (40/40 bytes)" {
		t.Fatalf("save stream = %v", msgs)
	}
	got := msgs[2]["result"].(map[string]any)["structuredContent"].(map[string]any)
	want := map[string]any{"box_id": boxes.BoxID("alice"), "last_saved_at_ms": float64(1700), "operation_id": got["operation_id"], "bytes": float64(40)}
	if !mapsEqual(got, want) || got["operation_id"] == "" {
		t.Fatalf("save = %v, want %v", got, want)
	}

	fake.failNext = true
	if got := refusal(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"box_save"}}`)); got != "Save failed: save failed" {
		t.Fatalf("failed save = %q", got)
	}
	fake.setState("alice", controlv1.BoxState_BOX_STATE_HIBERNATED)
	for _, tool := range []string{"box_save", "box_hibernate"} {
		if got := refusal(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"`+tool+`"}}`)); got != "The box is hibernated." {
			t.Fatalf("%s of a hibernated box = %q", tool, got)
		}
	}
	if got := refusal(t, post(t, srv, carolToken, `{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"box_hibernate"}}`)); got != "The box is absent." {
		t.Fatalf("hibernate without a box = %q", got)
	}
	got = structured(t, post(t, srv, carolToken, `{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"box_status"}}`))
	if want := map[string]any{"box_id": "", "name": "", "state": "absent", "image": "", "latest_image": "", "upgrade_available": false, "last_saved_at_ms": float64(0)}; !mapsEqual(got, want) {
		t.Fatalf("status without a box = %v", got)
	}

	fake.ensureErr = status.Error(codes.Unavailable, "ensure box failed")
	if resp := post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"box_ensure"}}`); resp.Error == nil || resp.Error.Code != mcpwire.CodeInternal {
		t.Fatalf("ensure on a failing backend = %+v", resp)
	}
}

func TestUsageReportsTheCallersMeter(t *testing.T) {
	srv, fake := newTestServer(t)

	got := structured(t, post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"usage"}}`))
	jev := map[string]any{"model": "typesafe/jev-1.13", "requests": float64(3), "prompt_tokens": float64(900), "completion_tokens": float64(30), "total_tokens": float64(930), "cost_micros": float64(120)}
	today := map[string]any{"since_ms": float64(1000), "requests": float64(3), "prompt_tokens": float64(900), "completion_tokens": float64(30), "total_tokens": float64(930), "cost_micros": float64(120), "models": []any{jev}}
	month := map[string]any{"since_ms": float64(10), "requests": float64(0), "prompt_tokens": float64(0), "completion_tokens": float64(0), "total_tokens": float64(0), "cost_micros": float64(0), "models": []any{}}
	want := map[string]any{"today": today, "month": month, "daily_cap_micros": float64(5_000_000), "default_cap": true, "remaining_today_micros": float64(4_999_880)}
	if !mapsEqual(got, want) {
		t.Fatalf("usage = %v, want %v", got, want)
	}
	if calls := fake.recorded(); len(calls) != 1 || calls[0] != (fakeCall{method: "Usage", user: "alice"}) {
		t.Fatalf("calls = %v, want one Usage as alice", calls)
	}
	if resp := post(t, srv, bobToken, `{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"usage"}}`); resp.Error == nil || resp.Error.Code != mcpwire.CodeInternal {
		t.Fatalf("usage while gw is down = %+v", resp)
	}
	if resp := post(t, srv, aliceToken, `{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"usage","arguments":{"box":"x"}}}`); resp.Error == nil || resp.Error.Code != mcpwire.CodeInvalidParams {
		t.Fatalf("usage with a box = %+v", resp)
	}
}

type rpcReply struct {
	ID     any            `json:"id"`
	Result map[string]any `json:"result"`
	Error  *mcpwire.Error `json:"error"`
}

type fakeCall struct {
	method, user, box string
}

// fakeBoxes owns boxes by user like boxes.Service; carol has none, and every operation finishes after two events.
type fakeBoxes struct {
	controlv1.UnimplementedBoxesServer

	mu        sync.Mutex
	calls     []fakeCall
	states    map[string]controlv1.BoxState
	images    map[string]string
	saved     map[string]int64
	ops       map[string]string
	extra     map[string][]*controlv1.Box
	failNext  bool
	ensureErr error
}

func (f *fakeBoxes) Ensure(ctx context.Context, req *controlv1.EnsureRequest) (*controlv1.EnsureResponse, error) {
	user := auth.User(ctx)
	f.record("Ensure", user, req.GetBoxId())
	if f.ensureErr != nil {
		return nil, f.ensureErr
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	box := f.boxLocked(user)
	f.states[user] = controlv1.BoxState_BOX_STATE_RUNNING
	if box.GetState() == controlv1.BoxState_BOX_STATE_RUNNING {
		return &controlv1.EnsureResponse{Box: box}, nil
	}
	return &controlv1.EnsureResponse{Box: box, Operation: f.startLocked(user, "wake")}, nil
}

func (f *fakeBoxes) List(ctx context.Context, _ *controlv1.ListBoxesRequest) (*controlv1.ListBoxesResponse, error) {
	user := auth.User(ctx)
	f.mu.Lock()
	defer f.mu.Unlock()
	if _, ok := f.states[user]; !ok {
		return &controlv1.ListBoxesResponse{}, nil
	}
	return &controlv1.ListBoxesResponse{Boxes: append([]*controlv1.Box{f.boxLocked(user)}, f.extra[user]...)}, nil
}

func (f *fakeBoxes) Create(ctx context.Context, req *controlv1.CreateBoxRequest) (*controlv1.EnsureResponse, error) {
	user := auth.User(ctx)
	f.record("Create", user, req.GetName())
	if req.GetImage() == "nosuch" {
		return nil, status.Errorf(codes.InvalidArgument, "image %q is not offered", req.GetImage())
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	box := &controlv1.Box{Id: "cb-" + req.GetName(), Name: req.GetName(), State: controlv1.BoxState_BOX_STATE_STARTING, Image: req.GetImage()}
	f.extra[user] = append(f.extra[user], box)
	return &controlv1.EnsureResponse{Box: box, Operation: f.startLocked(user, "create")}, nil
}

func (f *fakeBoxes) Delete(ctx context.Context, ref *controlv1.BoxRef) (*controlv1.Operation, error) {
	return f.operate(ctx, "Delete", ref.GetBoxId())
}

func (f *fakeBoxes) Hibernate(ctx context.Context, ref *controlv1.BoxRef) (*controlv1.Operation, error) {
	return f.operate(ctx, "Hibernate", ref.GetBoxId())
}

func (f *fakeBoxes) SaveState(ctx context.Context, ref *controlv1.BoxRef) (*controlv1.Operation, error) {
	return f.operate(ctx, "SaveState", ref.GetBoxId())
}

func (f *fakeBoxes) Rebase(ctx context.Context, req *controlv1.RebaseRequest) (*controlv1.Operation, error) {
	return f.operate(ctx, "Rebase", req.GetBoxId())
}

func (f *fakeBoxes) WatchOperation(req *controlv1.WatchOperationRequest, stream grpc.ServerStreamingServer[controlv1.OperationEvent]) error {
	user := auth.User(stream.Context())
	f.mu.Lock()
	owner, ok := f.ops[req.GetOperationId()]
	fail := f.failNext
	f.failNext = false
	f.mu.Unlock()
	if !ok || owner != user {
		return status.Error(codes.NotFound, "operation not found")
	}
	id := req.GetOperationId()
	first := &controlv1.OperationEvent{OperationId: id, Phase: controlv1.Phase_PHASE_WAKING, Detail: "restoring memory snapshot"}
	last := &controlv1.OperationEvent{OperationId: id, Phase: controlv1.Phase_PHASE_DONE, Detail: "running"}
	if strings.HasPrefix(id, "save") {
		first = &controlv1.OperationEvent{OperationId: id, Phase: controlv1.Phase_PHASE_SAVING, Detail: "uploading", DoneBytes: 40, TotalBytes: 40}
		last = &controlv1.OperationEvent{OperationId: id, Phase: controlv1.Phase_PHASE_DONE, Detail: "snapshot s1"}
	}
	if fail {
		last = &controlv1.OperationEvent{OperationId: id, Phase: controlv1.Phase_PHASE_FAILED, Detail: "save failed"}
	}
	for _, ev := range []*controlv1.OperationEvent{first, last} {
		if err := stream.Send(ev); err != nil {
			return err
		}
	}
	return nil
}

func (f *fakeBoxes) Usage(ctx context.Context, _ *controlv1.UsageRequest) (*controlv1.UsageResponse, error) {
	user := auth.User(ctx)
	f.record("Usage", user, "")
	if user != "alice" {
		return nil, status.Error(codes.Unavailable, "read gw usage failed")
	}
	jev := &controlv1.ModelUsage{Model: "typesafe/jev-1.13", Requests: 3, PromptTokens: 900, CompletionTokens: 30, TotalTokens: 930, CostMicros: 120}
	return &controlv1.UsageResponse{
		Today:          &controlv1.UsagePeriod{SinceMs: 1000, Requests: 3, PromptTokens: 900, CompletionTokens: 30, TotalTokens: 930, CostMicros: 120, Models: []*controlv1.ModelUsage{jev}},
		Month:          &controlv1.UsagePeriod{SinceMs: 10},
		DailyCapMicros: 5_000_000, DefaultCap: true, RemainingTodayMicros: 4_999_880,
	}, nil
}

func (f *fakeBoxes) operate(ctx context.Context, method, box string) (*controlv1.Operation, error) {
	user := auth.User(ctx)
	f.record(method, user, box)
	if box != boxes.BoxID(user) && !slices.ContainsFunc(f.extra[user], func(b *controlv1.Box) bool { return b.GetId() == box }) {
		return nil, boxes.ErrNoBox
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	kind := "op"
	switch method {
	case "Hibernate":
		f.states[user] = controlv1.BoxState_BOX_STATE_HIBERNATED
	case "SaveState":
		kind = "save"
		f.saved[user] = 1700
	case "Rebase":
		f.images[user] = latestImage
	}
	op := f.startLocked(user, kind)
	op.BoxId = box
	return op, nil
}

func (f *fakeBoxes) boxLocked(user string) *controlv1.Box {
	return &controlv1.Box{Id: boxes.BoxID(user), Name: "main", State: f.states[user], Image: f.images[user], LatestImage: latestImage, LastSavedAtMs: f.saved[user]}
}

func (f *fakeBoxes) startLocked(user, kind string) *controlv1.Operation {
	id := kind + "-" + user + "-" + string(rune('a'+len(f.ops)))
	f.ops[id] = user
	return &controlv1.Operation{Id: id, BoxId: boxes.BoxID(user)}
}

func (f *fakeBoxes) setState(user string, state controlv1.BoxState) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.states[user] = state
}

func (f *fakeBoxes) record(method, user, box string) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.calls = append(f.calls, fakeCall{method: method, user: user, box: box})
}

func (f *fakeBoxes) recorded() []fakeCall {
	f.mu.Lock()
	defer f.mu.Unlock()
	return slices.Clone(f.calls)
}

func newTestServer(t *testing.T) (*Server, *fakeBoxes) {
	t.Helper()
	users := staticUsers{}
	for _, token := range []string{aliceToken, bobToken, carolToken} {
		sum := sha256.Sum256([]byte(token))
		users[hex.EncodeToString(sum[:])] = strings.TrimSuffix(token, "-token")
	}
	running := controlv1.BoxState_BOX_STATE_RUNNING
	fake := &fakeBoxes{
		states: map[string]controlv1.BoxState{"alice": running, "bob": running},
		images: map[string]string{"alice": "img:1", "bob": "img:1"},
		saved:  map[string]int64{},
		ops:    map[string]string{},
		extra:  map[string][]*controlv1.Box{},
	}
	return New(fake, auth.New(users, nil)), fake
}

func serve(srv *Server, method, token, body string) *httptest.ResponseRecorder {
	req := httptest.NewRequest(method, Path, strings.NewReader(body))
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Accept", "application/json, text/event-stream")
	rec := httptest.NewRecorder()
	srv.ServeHTTP(rec, req)
	return rec
}

func post(t *testing.T, srv *Server, token, body string) rpcReply {
	t.Helper()
	return decode(t, serve(srv, http.MethodPost, token, body))
}

func decode(t *testing.T, rec *httptest.ResponseRecorder) rpcReply {
	t.Helper()
	if rec.Code != http.StatusOK || rec.Header().Get("Content-Type") != "application/json" {
		t.Fatalf("status %d, content type %q: %s", rec.Code, rec.Header().Get("Content-Type"), rec.Body)
	}
	var reply rpcReply
	if err := json.Unmarshal(rec.Body.Bytes(), &reply); err != nil {
		t.Fatalf("decode %s: %v", rec.Body, err)
	}
	return reply
}

// refusal returns the text of an isError result, which carries no structured content.
func refusal(t *testing.T, reply rpcReply) string {
	t.Helper()
	content, _ := reply.Result["content"].([]any)
	if reply.Result["isError"] != true || len(content) != 1 || reply.Result["structuredContent"] != nil {
		t.Fatalf("not a refusal: %+v", reply)
	}
	return content[0].(map[string]any)["text"].(string)
}

// structured returns a successful result's structured content and checks the text block carries the same JSON.
func structured(t *testing.T, reply rpcReply) map[string]any {
	t.Helper()
	got, ok := reply.Result["structuredContent"].(map[string]any)
	content, _ := reply.Result["content"].([]any)
	if !ok || reply.Result["isError"] != false || len(content) != 1 {
		t.Fatalf("not a structured result: %+v", reply)
	}
	var text map[string]any
	if err := json.Unmarshal([]byte(content[0].(map[string]any)["text"].(string)), &text); err != nil || !mapsEqual(text, got) {
		t.Fatalf("text block %v does not mirror %v", content[0], got)
	}
	return got
}

func mapsEqual(a, b map[string]any) bool {
	x, _ := json.Marshal(a)
	y, _ := json.Marshal(b)
	return string(x) == string(y)
}

func sseMessages(t *testing.T, body string) []map[string]any {
	t.Helper()
	var msgs []map[string]any
	scanner := bufio.NewScanner(strings.NewReader(body))
	for scanner.Scan() {
		data, ok := strings.CutPrefix(scanner.Text(), "data: ")
		if !ok {
			continue
		}
		var msg map[string]any
		if err := json.Unmarshal([]byte(data), &msg); err != nil {
			t.Fatalf("decode event %q: %v", data, err)
		}
		msgs = append(msgs, msg)
	}
	return msgs
}

type staticUsers map[string]string

func (u staticUsers) ByTokenSHA256(_ context.Context, sum string) (string, bool) {
	id, ok := u[sum]
	return id, ok
}

func (staticUsers) Lifetime(string, string) (context.Context, func()) {
	return context.Background(), func() {}
}
