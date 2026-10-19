package boxes

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/loomhand/cocoonbox/api/auth"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
	"github.com/loomhand/cocoonbox/api/machines/sandboxd"
)

const (
	nodeToken           = "root-token"
	sandboxdMetaMachine = "cb_machine"
	sandboxdMetaOwner   = "cb_owner"
)

func TestRebaseAfterCapacityRefusalsEndsRunningOnTheNewMachine(t *testing.T) {
	node := newFakeNode(t)
	node.add(aliceBox, "desktop:v1", time.Now().Add(-time.Hour))
	node.refusals = 2
	backend, err := sandboxd.New(t.Context(), sandboxd.Options{URL: node.srv.URL, Token: nodeToken, Image: "desktop:v1", Flavors: []machines.Flavor{defaultFlavor}, DiskGiB: 40, AgentPort: 7443})
	if err != nil {
		t.Fatalf("sandboxd.New: %v", err)
	}
	st := newFakeStore()
	agent := newFakeAgent(st)
	agent.chunks = []string{hexID("c1")}
	svc := New(backend, testDefaults, newFakeRegistry())
	svc.EnableState(agent, st, &fakeKeys{})
	ctx := auth.WithUser(t.Context(), alice)
	rebase := func() *controlv1.OperationEvent {
		t.Helper()
		op, err := svc.Rebase(ctx, &controlv1.RebaseRequest{BoxId: aliceBox})
		if err != nil {
			t.Fatalf("Rebase: %v", err)
		}
		events := watch(t, svc, ctx, op.GetId(), "")
		return events[len(events)-1]
	}
	boxState := func() *controlv1.Box {
		t.Helper()
		box, err := svc.Get(ctx, &controlv1.GetBoxRequest{BoxId: aliceBox})
		if err != nil {
			t.Fatalf("Get: %v", err)
		}
		return box
	}

	for i := range 2 {
		if last := rebase(); last.GetPhase() != controlv1.Phase_PHASE_FAILED || last.GetDetail() != machines.ErrCapacity.Error() {
			t.Fatalf("refused rebase %d ended with %v, want FAILED %q", i, last, machines.ErrCapacity)
		}
		if box := boxState(); box.GetState() != controlv1.BoxState_BOX_STATE_RUNNING {
			t.Errorf("box after refused rebase %d = %v, want RUNNING on the old machine", i, box)
		}
	}
	for sec := time.Now().Unix(); time.Now().Unix() == sec; {
		time.Sleep(10 * time.Millisecond)
	}
	if last := rebase(); last.GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("rebase after the refusals ended with %v", last)
	}

	if box := boxState(); box.GetState() != controlv1.BoxState_BOX_STATE_RUNNING || box.GetFailureReason() != "" {
		t.Errorf("box after the rebase = %v, want RUNNING", box)
	}
	ms, _ := backend.List(ctx, alice)
	if len(ms) != 1 || !strings.HasPrefix(ms[0].ID, aliceBox+"-r") || ms[0].State != machines.StateRunning {
		t.Errorf("machines after the rebase = %+v, want only the new one", ms)
	}
	if ids := node.ids(); len(ids) != 1 {
		t.Errorf("claims on the node = %v, want only the new one", ids)
	}
}

type nodeRow struct {
	ID  string `json:"id"`
	Key struct {
		Template string `json:"template"`
		Size     string `json:"size"`
	} `json:"key"`
	Metadata  map[string]string `json:"metadata"`
	ClaimedAt time.Time         `json:"claimed_at"`
}

type fakeNode struct {
	srv *httptest.Server

	mu       sync.Mutex
	rows     map[string]*nodeRow
	next     int
	refusals int
}

func newFakeNode(t *testing.T) *fakeNode {
	t.Helper()
	n := &fakeNode{rows: map[string]*nodeRow{}}
	mux := http.NewServeMux()
	mux.HandleFunc("POST /v1/claim", func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Template string            `json:"template"`
			Size     string            `json:"size"`
			Metadata map[string]string `json:"metadata"`
		}
		_ = json.NewDecoder(r.Body).Decode(&req)
		n.mu.Lock()
		defer n.mu.Unlock()
		if n.refusals > 0 {
			n.refusals--
			nodeJSON(w, http.StatusTooManyRequests, map[string]string{"error": "node claim quota reached: 12 live claims, cap 12"})
			return
		}
		row := n.addLocked(req.Metadata[sandboxdMetaMachine], req.Template, time.Now())
		row.Key.Size = req.Size
		nodeJSON(w, http.StatusOK, map[string]string{"id": row.ID})
	})
	mux.HandleFunc("GET /v1/sandboxes", func(w http.ResponseWriter, r *http.Request) {
		k, v, _ := strings.Cut(r.URL.Query().Get("metadata"), "=")
		n.mu.Lock()
		defer n.mu.Unlock()
		rows := []nodeRow{}
		for _, row := range n.rows {
			if k == "" || row.Metadata[k] == v {
				rows = append(rows, *row)
			}
		}
		nodeJSON(w, http.StatusOK, map[string]any{"sandboxes": rows})
	})
	mux.HandleFunc("GET /v1/sandboxes/{id}", n.sandbox(func(w http.ResponseWriter, _ *http.Request, row *nodeRow) {
		nodeJSON(w, http.StatusOK, row)
	}))
	mux.HandleFunc("GET /v1/sandboxes/{id}/env", n.sandbox(func(w http.ResponseWriter, _ *http.Request, _ *nodeRow) {
		nodeJSON(w, http.StatusOK, map[string]any{"env": map[string]any{}})
	}))
	mux.HandleFunc("POST /v1/sandboxes/{id}/{verb}", n.sandbox(func(w http.ResponseWriter, r *http.Request, row *nodeRow) {
		if r.PathValue("verb") == "release" {
			delete(n.rows, row.ID)
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	mux.HandleFunc("GET /v1/sandboxes/{id}/ports/{port}", n.sandbox(func(w http.ResponseWriter, _ *http.Request, _ *nodeRow) {
		conn, rw, err := http.NewResponseController(w).Hijack()
		if err != nil {
			return
		}
		_, _ = rw.WriteString("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n")
		_ = rw.Flush()
		_ = conn.Close()
	}))
	n.srv = httptest.NewServer(mux)
	t.Cleanup(n.srv.Close)
	return n
}

func (n *fakeNode) add(machine, template string, claimedAt time.Time) {
	n.mu.Lock()
	defer n.mu.Unlock()
	n.addLocked(machine, template, claimedAt).Key.Size = "xlarge"
}

func (n *fakeNode) addLocked(machine, template string, claimedAt time.Time) *nodeRow {
	n.next++
	row := &nodeRow{ID: "sb_" + strconv.Itoa(n.next), Metadata: map[string]string{sandboxdMetaMachine: machine, sandboxdMetaOwner: alice}, ClaimedAt: claimedAt.UTC()}
	row.Key.Template = template
	n.rows[row.ID] = row
	return row
}

func (n *fakeNode) ids() []string {
	n.mu.Lock()
	defer n.mu.Unlock()
	var ids []string
	for _, row := range n.rows {
		ids = append(ids, row.Metadata[sandboxdMetaMachine])
	}
	return ids
}

func (n *fakeNode) sandbox(next func(http.ResponseWriter, *http.Request, *nodeRow)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		n.mu.Lock()
		defer n.mu.Unlock()
		row := n.rows[r.PathValue("id")]
		if row == nil {
			nodeJSON(w, http.StatusNotFound, map[string]string{"error": "unknown sandbox"})
			return
		}
		next(w, r, row)
	}
}

func nodeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}
