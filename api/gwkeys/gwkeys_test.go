package gwkeys

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"maps"
	"net/http"
	"net/http/httptest"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

const (
	adminToken = "tenant-admin-token"
	owner      = "u-3f9a2c41d07e"
	other      = "u-77b0e1aa9c42"
)

var (
	keyFormat = regexp.MustCompile(`^gw-[0-9a-f]{64}$`)
	idFormat  = regexp.MustCompile(`^sha256:[0-9a-f]{32}$`)
)

func TestAddCreatesAGeneratedKey(t *testing.T) {
	gw, c := newFakeGW(t)
	key, id, err := c.Add(t.Context(), owner)
	if err != nil || !keyFormat.MatchString(key) || id != akID(key) {
		t.Fatalf("Add = %q, %q, %v", key, id, err)
	}
	got := gw.key(akID(key))
	want := map[string]any{"ak_id": akID(key), "product": "cocoonbox", "tenant": "cocoonbox-boxes", "owner": owner, "qps": 40.0, "daily_token_quota": 50e6, "tokens_per_minute": 2.4e6}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Errorf("created %v, want %v", got, want)
	}
	second, _, err := c.Add(t.Context(), other)
	if err != nil || second == key || gw.key(akID(key)) == nil {
		t.Errorf("Add for another owner = %v, want a different key and the first kept", err)
	}
}

func TestAddReplacesTheOwnersKeys(t *testing.T) {
	gw, c := newFakeGW(t)
	gw.put("gw-k1", owner)
	gw.put("gw-k2", owner)
	key, _, err := c.Add(t.Context(), owner)
	if err != nil || key == "gw-k1" || key == "gw-k2" {
		t.Fatalf("Add = %q, %v, want a fresh key", key, err)
	}
	again, _, err := c.Add(t.Context(), owner)
	if err != nil || again == key {
		t.Fatalf("second Add = %q, %v, want a fresh key", again, err)
	}
	if ids := gw.idsOf(owner); !slices.Equal(ids, []string{akID(again)}) {
		t.Errorf("keys of %s = %v, want only the last one", owner, ids)
	}
}

func TestRevokeDeletesOnlyThatOwner(t *testing.T) {
	gw, c := newFakeGW(t)
	gw.put("gw-old", owner)
	gw.put("gw-new", other)
	gw.put("gw-other-tenant", owner)
	gw.keys[akID("gw-other-tenant")]["tenant"] = "cocoonbox"
	if err := c.Revoke(t.Context(), owner); err != nil {
		t.Fatalf("Revoke: %v", err)
	}
	if gw.key(akID("gw-old")) != nil || gw.key(akID("gw-new")) == nil || gw.key(akID("gw-other-tenant")) == nil {
		t.Errorf("keys after Revoke = %v", gw.order)
	}
	if err := c.Revoke(t.Context(), owner); err != nil {
		t.Errorf("Revoke of an absent key: %v", err)
	}
	gw.put("gw-gone", owner)
	gw.vanish = akID("gw-gone")
	if err := c.Revoke(t.Context(), owner); err != nil {
		t.Errorf("Revoke when DELETE answers 404: %v", err)
	}
}

func TestListFiltersByOwner(t *testing.T) {
	gw, c := newFakeGW(t)
	for i := range pageSize {
		gw.put("gw-filler"+strconv.Itoa(i), "cb-000000000000")
	}
	gw.put("gw-last", owner)
	if err := c.Revoke(t.Context(), owner); err != nil {
		t.Fatalf("Revoke: %v", err)
	}
	if gw.key(akID("gw-last")) != nil || gw.lists != 1 {
		t.Errorf("key of %s kept or the tenant scanned: lists %d", owner, gw.lists)
	}
}

func TestErrorsNeverCarryTheKey(t *testing.T) {
	gw, c := newFakeGW(t)
	gw.cut = true
	if key, _, err := c.Add(t.Context(), owner); err == nil || key != "" || strings.Contains(err.Error(), gw.last) {
		t.Errorf("Add with the POST response cut = %q, %v", key, err)
	}
	gw.cut = false
	key, _, err := c.Add(t.Context(), owner)
	if err != nil || !slices.Equal(gw.idsOf(owner), []string{akID(key)}) {
		t.Errorf("retried Add = %v, keys %v, want the orphan revoked", err, gw.idsOf(owner))
	}
	bad, _ := New(gw.srv.URL, "wrong")
	if _, _, err := bad.Add(t.Context(), owner); err == nil || !strings.Contains(err.Error(), "401") {
		t.Errorf("Add with a wrong token: %v", err)
	}
}

func TestUsageReadsTheOwnersRollup(t *testing.T) {
	gw, c := newFakeGW(t)
	gw.usage = `{"usage":[{"user_id":"u-3f9a2c41d07e","model":"m1","requests":2,"prompt_tokens":24,"completion_tokens":6,"total_tokens":30,"cost_micros":120,"vendor_cost_micros":0}]}`
	since := time.Date(2026, 10, 2, 0, 0, 0, 0, time.UTC)
	got, err := c.Usage(t.Context(), owner, since)
	if err != nil || len(got) != 1 || got[0] != (Usage{Model: "m1", Requests: 2, PromptTokens: 24, CompletionTokens: 6, TotalTokens: 30, CostMicros: 120}) {
		t.Fatalf("Usage = %+v, %v", got, err)
	}
	if gw.usageUser != owner || gw.usageSince != strconv.FormatInt(since.Unix(), 10) {
		t.Errorf("usage queried for %q since %q, want %q since %d", gw.usageUser, gw.usageSince, owner, since.Unix())
	}
}

func TestDailyCapRoundTripsEachForm(t *testing.T) {
	gw, c := newFakeGW(t)
	for _, tt := range []struct {
		name string
		set  *int64
		wire string
	}{
		{"cap", new(int64(5_000_000)), "5000000"},
		{"unlimited", new(int64(Unlimited)), `"unlimited"`},
		{"inherit", nil, "null"},
	} {
		t.Run(tt.name, func(t *testing.T) {
			if err := c.SetDailyCap(t.Context(), owner, tt.set); err != nil {
				t.Fatalf("SetDailyCap: %v", err)
			}
			if got := gw.budgetWire(owner); got != tt.wire {
				t.Errorf("gw holds %s, want %s", got, tt.wire)
			}
			got, err := c.DailyCap(t.Context(), owner)
			if err != nil || (got == nil) != (tt.set == nil) || (got != nil && *got != *tt.set) {
				t.Errorf("DailyCap = %v, %v, want %v", got, err, tt.set)
			}
		})
	}
	gw.budgets[other] = json.RawMessage(`"lots"`)
	if _, err := c.DailyCap(t.Context(), other); err == nil {
		t.Error("DailyCap of an unknown word succeeded")
	}
}

func TestDailyCapsWalksEveryPage(t *testing.T) {
	gw, c := newFakeGW(t)
	gw.budgetPage = 2
	want := map[string]int64{"a": 1, "admin": Unlimited, "b": 0, "c": 5_000_000}
	for user, micros := range want {
		if err := c.SetDailyCap(t.Context(), user, &micros); err != nil {
			t.Fatalf("SetDailyCap: %v", err)
		}
	}
	got, err := c.DailyCaps(t.Context())
	if err != nil || fmt.Sprint(got) != fmt.Sprint(want) {
		t.Errorf("DailyCaps = %v, %v, want %v", got, err, want)
	}
}

func TestSetEnabledExpiresAndRestoresOnlyThatOwnersKeys(t *testing.T) {
	gw, c := newFakeGW(t)
	gw.put("gw-k1", owner)
	gw.put("gw-k2", owner)
	gw.put("gw-other", other)
	if err := c.SetEnabled(t.Context(), owner, false); err != nil {
		t.Fatalf("SetEnabled false: %v", err)
	}
	for _, ak := range []string{"gw-k1", "gw-k2"} {
		if got := gw.key(akID(ak))["expires_at_epoch_secs"]; got != 0.0 {
			t.Errorf("expiry of %s = %v, want 0", ak, got)
		}
	}
	if _, set := gw.key(akID("gw-other"))["expires_at_epoch_secs"]; set {
		t.Errorf("another owner's key got an expiry")
	}
	if err := c.SetEnabled(t.Context(), owner, true); err != nil {
		t.Fatalf("SetEnabled true: %v", err)
	}
	for _, ak := range []string{"gw-k1", "gw-k2"} {
		if got, set := gw.key(akID(ak))["expires_at_epoch_secs"]; !set || got != nil {
			t.Errorf("expiry of %s = %v (set %t), want null", ak, got, set)
		}
	}
}

func TestNewRejectsBadURL(t *testing.T) {
	for _, base := range []string{"", "10.203.0.1:18090", "unix:///run/gw.sock"} {
		if _, err := New(base, adminToken); err == nil {
			t.Errorf("New(%q) succeeded", base)
		}
	}
}

type fakeGW struct {
	srv *httptest.Server

	mu     sync.Mutex
	keys   map[string]map[string]any
	order  []string
	lists  int
	vanish string
	cut    bool
	last   string

	usage      string
	usageUser  string
	usageSince string

	budgets    map[string]json.RawMessage
	budgetPage int
}

func newFakeGW(t *testing.T) (*fakeGW, *Client) {
	t.Helper()
	gw := &fakeGW{keys: map[string]map[string]any{}, budgets: map[string]json.RawMessage{}, budgetPage: 200}
	gw.srv = httptest.NewServer(http.HandlerFunc(gw.serve))
	t.Cleanup(gw.srv.Close)
	c, err := New(gw.srv.URL, adminToken)
	if err != nil {
		t.Fatalf("New: %v", err)
	}
	return gw, c
}

func (g *fakeGW) put(ak, owner string) {
	id := akID(ak)
	g.keys[id] = map[string]any{"ak_id": id, "tenant": "cocoonbox-boxes", "owner": owner}
	g.order = append(g.order, id)
}

func (g *fakeGW) key(id string) map[string]any {
	g.mu.Lock()
	defer g.mu.Unlock()
	return g.keys[id]
}

func (g *fakeGW) idsOf(owner string) []string {
	g.mu.Lock()
	defer g.mu.Unlock()
	var ids []string
	for _, id := range g.order {
		if g.keys[id]["owner"] == owner {
			ids = append(ids, id)
		}
	}
	return ids
}

func (g *fakeGW) budgetWire(user string) string {
	g.mu.Lock()
	defer g.mu.Unlock()
	return string(orNull(g.budgets[user]))
}

func (g *fakeGW) serve(w http.ResponseWriter, r *http.Request) {
	g.mu.Lock()
	defer g.mu.Unlock()
	if r.Header.Get("Authorization") != "Bearer "+adminToken {
		writeError(w, http.StatusUnauthorized, "invalid admin token")
		return
	}
	id, hasID := strings.CutPrefix(r.URL.Path, "/admin/keys/")
	budgetUser, hasBudget := strings.CutPrefix(r.URL.Path, "/admin/tenants/cocoonbox-boxes/users/")
	budgetUser, hasBudget = strings.CutSuffix(budgetUser, "/budget")
	switch {
	case r.Method == http.MethodGet && r.URL.Path == "/admin/tenants/cocoonbox-boxes/users":
		users := slices.Sorted(maps.Keys(g.budgets))
		users = slices.DeleteFunc(users, func(u string) bool { return u <= r.URL.Query().Get("after") })
		page := map[string]any{"next": nil}
		if len(users) > g.budgetPage {
			users = users[:g.budgetPage]
		}
		if len(users) == g.budgetPage {
			page["next"] = users[len(users)-1]
		}
		var rows []map[string]any
		for _, u := range users {
			rows = append(rows, map[string]any{"user": u, "daily_cost_quota_micros": g.budgets[u], "monthly_cost_quota_micros": nil})
		}
		page["users"] = rows
		_ = json.NewEncoder(w).Encode(page)
	case r.Method == http.MethodGet && hasBudget:
		_ = json.NewEncoder(w).Encode(map[string]any{"user": budgetUser, "daily_cost_quota_micros": orNull(g.budgets[budgetUser])})
	case r.Method == http.MethodPut && hasBudget:
		var body map[string]json.RawMessage
		_ = json.NewDecoder(r.Body).Decode(&body)
		if daily := body["daily_cost_quota_micros"]; string(daily) == "null" {
			delete(g.budgets, budgetUser)
		} else {
			g.budgets[budgetUser] = daily
		}
		_ = json.NewEncoder(w).Encode(map[string]any{"user": budgetUser, "daily_cost_quota_micros": body["daily_cost_quota_micros"]})
	case r.Method == http.MethodGet && r.URL.Path == "/admin/keys":
		g.lists++
		q := r.URL.Query()
		if q.Get("owner") == "" {
			writeError(w, http.StatusBadRequest, "owner must be non-empty")
			return
		}
		limit, _ := strconv.Atoi(q.Get("limit"))
		var page []map[string]any
		for _, k := range g.order {
			if g.keys[k]["tenant"] == q.Get("tenant") && g.keys[k]["owner"] == q.Get("owner") {
				page = append(page, g.keys[k])
			}
		}
		page = page[:min(limit, len(page))]
		_ = json.NewEncoder(w).Encode(map[string]any{"count": len(page), "offset": 0, "keys": page})
	case r.Method == http.MethodGet && r.URL.Path == "/admin/usage/users":
		g.usageUser, g.usageSince = r.URL.Query().Get("user"), r.URL.Query().Get("since")
		_, _ = w.Write([]byte(g.usage))
	case r.Method == http.MethodPost && r.URL.Path == "/admin/keys":
		var body map[string]any
		_ = json.NewDecoder(r.Body).Decode(&body)
		if _, ok := body["ak"]; ok {
			writeError(w, http.StatusBadRequest, "the client chose the key")
			return
		}
		raw := make([]byte, 32)
		_, _ = rand.Read(raw)
		ak := "gw-" + hex.EncodeToString(raw)
		body["ak_id"] = akID(ak)
		g.keys[akID(ak)] = body
		g.order = append(g.order, akID(ak))
		g.last = ak
		w.WriteHeader(http.StatusCreated)
		resp := fmt.Sprintf(`{"ak_id":%q,"status":"created","ak":%q}`, akID(ak), ak)
		if g.cut {
			_, _ = w.Write([]byte(resp[:len(resp)-1]))
			w.(http.Flusher).Flush()
			panic(http.ErrAbortHandler)
		}
		_, _ = w.Write([]byte(resp))
	case r.Method == http.MethodPatch && hasID:
		if g.keys[id] == nil {
			writeError(w, http.StatusNotFound, "key "+id+" not found")
			return
		}
		var body map[string]any
		_ = json.NewDecoder(r.Body).Decode(&body)
		maps.Copy(g.keys[id], body)
		_ = json.NewEncoder(w).Encode(g.keys[id])
	case r.Method == http.MethodDelete && hasID:
		if !idFormat.MatchString(id) {
			writeError(w, http.StatusBadRequest, "the client deleted by raw key")
			return
		}
		if g.keys[id] == nil || id == g.vanish {
			writeError(w, http.StatusNotFound, "key "+id+" not found")
			return
		}
		delete(g.keys, id)
		g.order = slices.DeleteFunc(g.order, func(k string) bool { return k == id })
	default:
		writeError(w, http.StatusNotFound, "not found")
	}
}

// akID is gw's access_key_id: sha256: plus the first 16 digest bytes in hex.
func akID(ak string) string {
	sum := sha256.Sum256([]byte(ak))
	return "sha256:" + hex.EncodeToString(sum[:16])
}

func orNull(raw json.RawMessage) json.RawMessage {
	if len(raw) == 0 {
		return json.RawMessage("null")
	}
	return raw
}

func writeError(w http.ResponseWriter, status int, message string) {
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(map[string]any{"error": map[string]any{"message": message}})
}
