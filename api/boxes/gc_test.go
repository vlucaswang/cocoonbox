package boxes

import (
	"maps"
	"math/rand/v2"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	agentv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/agent/v1"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
	"github.com/loomhand/cocoonbox/api/users"
)

func TestRetainKeepsTheTimeMachineSet(t *testing.T) {
	now := time.Date(2026, 10, 3, 12, 0, 0, 0, time.UTC)
	at := func(id, when string) timelineEntry {
		ts, err := time.Parse(time.RFC3339, when)
		if err != nil {
			t.Fatalf("parse %s: %v", when, err)
		}
		return timelineEntry{ID: id, SavedAtMs: ts.UnixMilli()}
	}
	entries := []timelineEntry{
		at("hour", "2026-10-03T11:00:00Z"),
		at("yesterday", "2026-10-02T13:00:00Z"),
		at("day-early", "2026-10-01T10:00:00Z"),
		at("day-late", "2026-10-01T20:00:00Z"),
		at("sep10-early", "2026-09-10T05:00:00Z"),
		at("sep10-late", "2026-09-10T06:00:00Z"),
		at("sat-w31", "2026-08-01T00:00:00Z"),
		at("sun-w31", "2026-08-02T00:00:00Z"),
		at("mon-w32", "2026-08-03T00:00:00Z"),
		at("head", "2026-07-01T00:00:00Z"),
		at("pinned", "2026-07-01T00:30:00Z"),
		at("after-head", "2026-07-01T01:00:00Z"),
	}
	got := retain(entries, now, map[string]bool{"head": true, "pinned": true})
	want := []string{"after-head", "day-late", "head", "hour", "mon-w32", "pinned", "sep10-late", "sun-w31", "yesterday"}
	if ids := slices.Sorted(maps.Keys(got)); !slices.Equal(ids, want) {
		t.Errorf("kept %v, want %v", ids, want)
	}
}

func TestRetainNeverWantsADroppedSnapshotLater(t *testing.T) {
	rng := rand.New(rand.NewPCG(1, 2))
	start := time.Date(2026, 6, 1, 0, 0, 0, 0, time.UTC)
	var entries []timelineEntry
	for at := start; at.Before(start.Add(120 * 24 * time.Hour)); at = at.Add(time.Duration(rng.IntN(int(6*time.Hour))) + time.Minute) {
		entries = append(entries, timelineEntry{ID: snapshotID(len(entries) + 1), SavedAtMs: at.UnixMilli()})
	}
	last := entries[len(entries)-1].SavedAtMs
	kept := entries
	for step := range 200 {
		now := time.UnixMilli(last).Add(time.Duration(step) * 13 * time.Hour)
		keep := retain(kept, now, nil)
		if all := retain(entries, now, nil); !maps.Equal(keep, all) {
			t.Fatalf("at %s the pruned history keeps %d, the full one %d", now, len(keep), len(all))
		}
		kept = slices.DeleteFunc(slices.Clone(kept), func(e timelineEntry) bool { return !keep[e.ID] })
	}
	if len(kept) > 30 {
		t.Errorf("%d snapshots kept long after the last save, want about one per week", len(kept))
	}
}

func TestGCPrunesSnapshotsAndDeletesTheirChunks(t *testing.T) {
	env := newStateEnv(t)
	p := statePath{user: alice, box: aliceBox}
	for _, chunks := range [][]string{{"c1", "c2"}, {"c2", "c3"}, {"c3", "c4"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	if env.store.has(p.timeline()) {
		t.Fatal("a commit started a timeline the GC has not built")
	}
	r := env.collect(t, GCOptions{DeleteBatch: 1}, 40*24*time.Hour)
	if r.pruned != 2 || r.kept != 1 || r.chunks != 2 || r.stored != 4 || r.indexEntries != 2 || r.chunkBytes != 2*64 {
		t.Errorf("report = %+v", r)
	}
	for _, key := range []string{p.record(snapshotID(1)), p.manifest(snapshotID(2)), p.chunk(hexID("c1")), p.chunk(hexID("c2"))} {
		if env.store.has(key) {
			t.Errorf("%s survived the GC", key)
		}
	}
	if ids := env.timelineIDs(t, p); !slices.Equal(ids, []string{snapshotID(3)}) {
		t.Errorf("timeline = %v, want the head", ids)
	}
	if idx := env.indexIDs(t); !slices.Equal(idx, slices.Sorted(slices.Values(hexIDs("c3", "c4")))) {
		t.Errorf("index = %v, want c3 and c4", idx)
	}

	env.agent.chunks = hexIDs("c1", "c3")
	env.save(t)
	if puts := env.agent.lastUpload().GetPuts(); len(puts) != 1 || puts[0].GetChunkId() != hexID("c1") {
		t.Errorf("save after the GC uploaded %v, want c1 again", puts)
	}
	if ids := env.timelineIDs(t, p); !slices.Equal(ids, []string{snapshotID(3), snapshotID(4)}) {
		t.Errorf("timeline after a save = %v", ids)
	}
	env.restoreHead(t)

	orphan := snapshotID(99)
	for _, key := range []string{p.record(orphan), p.manifest(orphan), p.chunk(hexID("orphan"))} {
		env.store.objects[key] = []byte("x")
	}
	r = env.collect(t, GCOptions{DeleteBatch: 1000}, 0)
	if r.pruned != 1 || r.kept != 2 || r.chunks != 1 || r.indexEntries != 0 {
		t.Errorf("second report = %+v, want the orphan snapshot and chunk only", r)
	}
	for _, key := range []string{p.record(orphan), p.manifest(orphan), p.chunk(hexID("orphan"))} {
		if env.store.has(key) {
			t.Errorf("orphan %s survived the GC", key)
		}
	}
	env.restoreHead(t)
}

func TestGCDryRunWritesNothing(t *testing.T) {
	env := newStateEnv(t)
	for _, chunks := range [][]string{{"c1", "c2"}, {"c2", "c3"}, {"c3"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	before := maps.Clone(env.store.objects)
	dry := env.collect(t, GCOptions{DryRun: true, DeleteBatch: 1000}, 40*24*time.Hour)
	if !maps.EqualFunc(before, env.store.objects, func(a, b []byte) bool { return string(a) == string(b) }) || len(env.store.deleted) != 0 {
		t.Errorf("dry run changed the store: %d objects before, %d after, %d deleted", len(before), len(env.store.objects), len(env.store.deleted))
	}
	real := env.collect(t, GCOptions{DeleteBatch: 1000}, 40*24*time.Hour)
	if dry != real || real.pruned != 2 || real.chunks != 2 {
		t.Errorf("dry run reported %+v, the real pass %+v", dry, real)
	}
}

func TestGCKeepsPinnedSnapshots(t *testing.T) {
	env := newStateEnv(t)
	p := statePath{user: alice, box: aliceBox}
	for _, chunks := range [][]string{{"c1"}, {"c2"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	unpin, _ := env.svc.state.pin(t.Context(), p, snapshotID(1))
	env.collect(t, GCOptions{DeleteBatch: 1000}, 40*24*time.Hour)
	if !env.store.has(p.record(snapshotID(1))) || !env.store.has(p.chunk(hexID("c1"))) {
		t.Error("the GC removed a pinned snapshot")
	}
	unpin()
	env.collect(t, GCOptions{DeleteBatch: 1000}, 40*24*time.Hour)
	if env.store.has(p.record(snapshotID(1))) || env.store.has(p.chunk(hexID("c1"))) || !env.store.has(p.chunk(hexID("c2"))) {
		t.Error("after unpinning, the GC kept the old snapshot or dropped the head")
	}
}

func TestGCWaitsForASaveBetweenItsIndexReadAndCommit(t *testing.T) {
	env := newStateEnv(t)
	p := statePath{user: alice, box: aliceBox}
	for _, chunks := range [][]string{{"c1", "c2"}, {"c2", "c3"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	uploading, release := make(chan struct{}), make(chan struct{})
	var saveOp string
	env.store.before = func(action, key string) {
		if action != "list" || key != p.chunks() || saveOp != "" {
			return
		}
		env.agent.chunks = hexIDs("c1", "c3")
		env.agent.onUpload = func(machines.Endpoint) {
			close(uploading)
			<-release
		}
		op, err := env.svc.SaveState(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
		if err != nil {
			t.Errorf("SaveState: %v", err)
			close(uploading)
			return
		}
		saveOp = op.GetId()
		<-uploading
	}
	done := make(chan gcReport)
	go func() {
		r, err := env.svc.state.collect(env.ctx, alice, GCOptions{DeleteBatch: 1000}, time.Now().Add(40*24*time.Hour), env.live)
		if err != nil {
			t.Errorf("collect: %v", err)
		}
		done <- r
	}()
	<-uploading
	select {
	case r := <-done:
		t.Fatalf("the GC finished during a save that read the index: %+v", r)
	case <-time.After(100 * time.Millisecond):
	}
	close(release)
	r := <-done
	if events := watch(t, env.svc, env.ctx, saveOp, ""); events[len(events)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("save during the GC ended with %v", events[len(events)-1])
	}
	if puts := env.agent.lastUpload().GetPuts(); len(puts) != 0 {
		t.Errorf("the save uploaded %v, want c1 taken from the index", puts)
	}
	if !env.store.has(p.chunk(hexID("c1"))) || r.chunks != 0 {
		t.Errorf("the GC deleted c1, which the save committed during its sweep: %+v", r)
	}
	env.restoreHead(t)
}

func TestGCPrunesWithTheTimelineOfASaveThatCommittedWhileItWaited(t *testing.T) {
	env := newStateEnv(t)
	p := statePath{user: alice, box: aliceBox}
	env.agent.chunks = hexIDs("c1")
	env.save(t)
	env.collect(t, GCOptions{DeleteBatch: 1000}, 0)
	uploading, release := make(chan struct{}), make(chan struct{})
	env.agent.onUpload = func(machines.Endpoint) {
		close(uploading)
		<-release
	}
	env.agent.chunks = hexIDs("c1", "c2")
	op, err := env.svc.SaveState(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("SaveState: %v", err)
	}
	<-uploading
	readTimeline := make(chan struct{})
	var once sync.Once
	env.store.before = func(op, key string) {
		if op == "get" && key == p.timeline() {
			once.Do(func() { close(readTimeline) })
		}
	}
	collected := make(chan error, 1)
	go func() {
		_, err := env.svc.state.collect(env.ctx, alice, GCOptions{DeleteBatch: 1000}, time.Now(), env.live)
		collected <- err
	}()
	<-readTimeline
	close(release)
	watch(t, env.svc, env.ctx, op.GetId(), "")
	if err := <-collected; err != nil {
		t.Fatalf("collect after a save committed while it waited: %v", err)
	}
	if ids := env.timelineIDs(t, p); !slices.Equal(ids, []string{snapshotID(1), snapshotID(2)}) {
		t.Errorf("timeline = %v", ids)
	}
}

func TestGCDeletesChunksWhileNoSaveRuns(t *testing.T) {
	env := newStateEnv(t)
	p := statePath{user: alice, box: aliceBox}
	for _, chunks := range [][]string{{"c1", "c2"}, {"c2", "c3"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	var saveOp string
	env.store.before = func(action, key string) {
		if action != "delete" || !strings.HasPrefix(key, p.chunks()) || saveOp != "" {
			return
		}
		if lock := env.svc.state.userLock(alice); lock.TryRLock() {
			lock.RUnlock()
			t.Error("a chunk delete ran without the exclusive user lock")
		}
		env.agent.chunks = hexIDs("c1", "c3")
		op, err := env.svc.SaveState(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
		if err != nil {
			t.Errorf("SaveState: %v", err)
			return
		}
		saveOp = op.GetId()
	}
	if r := env.collect(t, GCOptions{DeleteBatch: 1000}, 40*24*time.Hour); r.chunks != 1 {
		t.Fatalf("report = %+v, want c1 deleted", r)
	}
	if events := watch(t, env.svc, env.ctx, saveOp, ""); events[len(events)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("save started during the deletes ended with %v", events[len(events)-1])
	}
	if puts := env.agent.lastUpload().GetPuts(); len(puts) != 1 || puts[0].GetChunkId() != hexID("c1") {
		t.Errorf("save after the sweep uploaded %v, want c1 again", puts)
	}
	env.restoreHead(t)
}

func TestASaveAfterPruningPlansOnTheHead(t *testing.T) {
	env := newStateEnv(t)
	env.svc.state.retryDelay = time.Millisecond
	p := statePath{user: alice, box: aliceBox}
	for _, chunks := range [][]string{{"c1"}, {"c1", "c2"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	env.collect(t, GCOptions{DeleteBatch: 1000}, 40*24*time.Hour)
	if env.store.has(p.record(snapshotID(1))) {
		t.Fatal("the GC kept the head's parent")
	}
	env.save(t)
	rec, err := env.svc.state.record(env.ctx, p, snapshotID(3))
	if plan := env.agent.lastPlan(); err != nil || plan.GetParent() != snapshotID(2) || rec.Parent != snapshotID(2) {
		t.Errorf("save after pruning planned on %q, record %+v, %v", plan.GetParent(), rec, err)
	}

	env.agent.chunks = hexIDs("c1", "c2", "c3")
	env.store.failNext(p.index(), commitAttempts)
	op, err := env.svc.SaveState(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("SaveState: %v", err)
	}
	if events := watch(t, env.svc, env.ctx, op.GetId(), ""); events[len(events)-1].GetPhase() != controlv1.Phase_PHASE_FAILED {
		t.Fatalf("save with a failing commit ended with %v", events[len(events)-1])
	}
	if r := env.collect(t, GCOptions{DeleteBatch: 1000}, 2*gcGrace); r.chunks != 1 || env.store.has(p.chunk(hexID("c3"))) {
		t.Fatalf("report = %+v, want the uncommitted c3 deleted", r)
	}
	env.save(t)
	plans := env.agent.allPlans()
	if got := []string{plans[len(plans)-2].GetParent(), plans[len(plans)-1].GetParent()}; !slices.Equal(got, []string{snapshotID(3), ""}) {
		t.Errorf("parents of the plans after the GC took the agent's known chunk = %q, want the head, then none", got)
	}
	env.restoreHead(t)
}

func TestGCRefusesToPruneAHeadMissingFromTheTimeline(t *testing.T) {
	env := newStateEnv(t)
	p := statePath{user: alice, box: aliceBox}
	for _, chunks := range [][]string{{"c1"}, {"c2"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	env.collect(t, GCOptions{DeleteBatch: 1000}, 0)
	if err := putJSON(env.ctx, env.store, p.timeline(), timeline{Snapshots: []timelineEntry{{ID: snapshotID(1)}}}); err != nil {
		t.Fatalf("setup: %v", err)
	}
	before := len(env.store.objects)
	if _, err := env.svc.state.collect(env.ctx, alice, GCOptions{DeleteBatch: 1000}, time.Now(), env.live); err == nil || !strings.Contains(err.Error(), "not in the timeline") {
		t.Errorf("collect = %v, want the head refused", err)
	}
	if len(env.store.objects) != before || len(env.store.deleted) != 0 {
		t.Errorf("the refused pass deleted %v", env.store.deleted)
	}
}

func TestGCSkipsTheUsersWhosePolicySaysSo(t *testing.T) {
	env := newStateEnv(t)
	for _, chunks := range [][]string{{"c1"}, {"c2"}} {
		env.agent.chunks = hexIDs(chunks...)
		env.save(t)
	}
	before := len(env.store.objects)
	env.svc.registry.(*fakeRegistry).setPolicy(alice, users.Policy{NoGC: true})
	env.svc.collectGarbage(env.ctx, GCOptions{DeleteBatch: 1000}, []string{alice, "bob"}, time.Now().Add(40*24*time.Hour))
	if len(env.store.objects) != before {
		t.Errorf("a pass changed the state of a user whose policy skips the gc: %d objects, want %d", len(env.store.objects), before)
	}
	env.svc.registry.(*fakeRegistry).setPolicy(alice, users.Policy{})
	env.svc.collectGarbage(env.ctx, GCOptions{DeleteBatch: 1000}, []string{alice, "bob"}, time.Now().Add(40*24*time.Hour))
	if env.store.has(alice + "/chunks/" + hexID("c1")) {
		t.Error("a pass for every user kept alice's dropped chunk")
	}
}

func TestGCPurgesADeletedBoxAfterItsRetentionAndKeepsSharedChunks(t *testing.T) {
	env := newStateEnv(t)
	gone, other := statePath{user: alice, box: aliceBox}, statePath{user: alice, box: "cb-00000000000b"}
	env.agent.byBox = map[string][]string{gone.box + ":7443": hexIDs("c1", "c2"), other.box + ":7443": hexIDs("c2", "c3")}
	env.addBox(t, other.box)
	env.save(t)
	env.saveBox(t, other.box)
	env.deleteBox(t, gone.box)

	opts := GCOptions{DeleteBatch: 1000, DeletedRetention: 7 * 24 * time.Hour}
	before := maps.Clone(env.store.objects)
	if dry := env.collect(t, GCOptions{DryRun: true, DeleteBatch: 1000, DeletedRetention: opts.DeletedRetention}, 0); dry.deleted != 1 || dry.purged != 0 || len(env.store.objects) != len(before) {
		t.Fatalf("dry run = %+v, wrote %d objects", dry, len(env.store.objects)-len(before))
	}
	if r := env.collect(t, opts, 0); r.deleted != 1 || r.purged != 0 || r.boxes != 2 || !env.store.has(gone.deleted()) {
		t.Fatalf("first pass after the delete = %+v, want the box recorded as deleted and kept", r)
	}
	if r := env.collect(t, opts, 6*24*time.Hour); r.purged != 0 || !env.store.has(gone.head()) || !env.store.has(gone.chunk(hexID("c1"))) {
		t.Fatalf("pass inside the retention = %+v, want the deleted box kept", r)
	}
	dry := env.collect(t, GCOptions{DryRun: true, DeleteBatch: 1000, DeletedRetention: opts.DeletedRetention}, 8*24*time.Hour)
	r := env.collect(t, opts, 8*24*time.Hour)
	if r.purged != 1 || r.boxes != 1 || r.chunks != 1 || r.purgedBytes == 0 || dry != r {
		t.Errorf("pass after the retention = %+v, dry run %+v, want the box and c1 purged", r, dry)
	}
	if left, _ := env.store.List(env.ctx, gone.boxDir()); len(left) != 0 {
		t.Errorf("%d objects of the deleted box survived: %v", len(left), left)
	}
	if env.store.has(gone.chunk(hexID("c1"))) || !env.store.has(other.chunk(hexID("c2"))) || !env.store.has(other.chunk(hexID("c3"))) {
		t.Error("the purge kept c1 or dropped a chunk the other box uses")
	}
	if idx := env.indexIDs(t); !slices.Equal(idx, slices.Sorted(slices.Values(hexIDs("c2", "c3")))) {
		t.Errorf("index = %v, want c2 and c3", idx)
	}
	if _, err := env.svc.state.restore(env.ctx, other, machines.Endpoint{Key: "restore"}, "", noProgress); err != nil {
		t.Errorf("restore of the other box: %v", err)
	}
	if r := env.collect(t, opts, 9*24*time.Hour); r.deleted != 0 || r.chunks != 0 {
		t.Errorf("pass after the purge = %+v, want nothing left to do", r)
	}
}

func TestGCKeepsABoxWhoseDeleteIsStillSaving(t *testing.T) {
	env := newStateEnv(t)
	p := statePath{user: alice, box: aliceBox}
	env.agent.chunks = hexIDs("c1")
	env.save(t)
	env.agent.chunks = hexIDs("c1", "c2")
	uploading, release := make(chan struct{}), make(chan struct{})
	env.agent.onUpload = func(machines.Endpoint) {
		close(uploading)
		<-release
	}
	op, err := env.svc.Delete(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("Delete: %v", err)
	}
	<-uploading
	if _, err := env.svc.SaveState(env.ctx, &controlv1.BoxRef{BoxId: aliceBox}); status.Code(err) != codes.FailedPrecondition {
		t.Errorf("SaveState during the delete = %v, want FailedPrecondition", err)
	}
	opts := GCOptions{DeleteBatch: 1000}
	during := make(chan gcReport)
	go func() {
		r, err := env.svc.state.collect(env.ctx, alice, opts, time.Now(), env.live)
		if err != nil {
			t.Errorf("collect: %v", err)
		}
		during <- r
	}()
	select {
	case r := <-during:
		t.Fatalf("the GC finished during the delete's save: %+v", r)
	case <-time.After(100 * time.Millisecond):
	}
	close(release)
	if events := watch(t, env.svc, env.ctx, op.GetId(), ""); events[len(events)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("delete ended with %v", events[len(events)-1])
	}
	if r := <-during; r.deleted != 0 || env.store.has(p.deleted()) {
		t.Fatalf("pass during the delete's save = %+v, want the box live", r)
	}
	if !env.store.has(p.record(snapshotID(2))) {
		t.Fatal("the delete's save did not commit")
	}
	if r := env.collect(t, opts, 2*gcGrace); r.purged != 1 || r.chunks != 2 {
		t.Errorf("pass after the delete = %+v, want the box and both chunks purged", r)
	}
	if left, _ := env.store.List(env.ctx, alice+"/"); len(left) != 1 || left[0].Key != p.index() {
		t.Errorf("after the purge the user holds %v, want only the index", left)
	}
}

func (e *stateEnv) collect(t *testing.T, opts GCOptions, ahead time.Duration) gcReport {
	t.Helper()
	r, err := e.svc.state.collect(e.ctx, alice, opts, time.Now().Add(ahead), e.live)
	if err != nil {
		t.Fatalf("collect: %v", err)
	}
	return r
}

func (e *stateEnv) live(box string) bool {
	return e.svc.registry.HasBox(alice, box)
}

func (e *stateEnv) addBox(t *testing.T, box string) {
	t.Helper()
	if _, err := e.svc.registry.AddBox(e.ctx, users.Box{ID: box, Owner: alice, Name: box}); err != nil {
		t.Fatalf("AddBox: %v", err)
	}
	e.fake.put(machines.Machine{ID: box, Owner: alice, State: machines.StateRunning, Flavor: defaultFlavor, Image: "desktop:v1", Revision: 1})
}

func (e *stateEnv) saveBox(t *testing.T, box string) {
	t.Helper()
	op, err := e.svc.SaveState(e.ctx, &controlv1.BoxRef{BoxId: box})
	if err != nil {
		t.Fatalf("SaveState %s: %v", box, err)
	}
	if events := watch(t, e.svc, e.ctx, op.GetId(), ""); events[len(events)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("save of %s ended with %v", box, events[len(events)-1])
	}
}

func (e *stateEnv) deleteBox(t *testing.T, box string) {
	t.Helper()
	op, err := e.svc.Delete(e.ctx, &controlv1.BoxRef{BoxId: box})
	if err != nil {
		t.Fatalf("Delete %s: %v", box, err)
	}
	if events := watch(t, e.svc, e.ctx, op.GetId(), ""); events[len(events)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("delete of %s ended with %v", box, events[len(events)-1])
	}
}

func (e *stateEnv) restoreHead(t *testing.T) {
	t.Helper()
	if _, err := e.svc.state.restore(e.ctx, statePath{user: alice, box: aliceBox}, machines.Endpoint{Key: "restore"}, "", noProgress); err != nil {
		t.Errorf("restore of the head: %v", err)
	}
}

func (e *stateEnv) timelineIDs(t *testing.T, p statePath) []string {
	t.Helper()
	var tl timeline
	if err := getJSON(e.ctx, e.store, p.timeline(), &tl); err != nil {
		t.Fatalf("timeline: %v", err)
	}
	var ids []string
	for _, s := range tl.Snapshots {
		ids = append(ids, s.ID)
	}
	return ids
}

func (e *stateEnv) indexIDs(t *testing.T) []string {
	t.Helper()
	var idx chunkIndex
	if err := getJSON(e.ctx, e.store, alice+"/index.json", &idx); err != nil {
		t.Fatalf("index: %v", err)
	}
	return idx.Chunks
}

func hexIDs(names ...string) []string {
	ids := make([]string, len(names))
	for i, name := range names {
		ids[i] = hexID(name)
	}
	return ids
}

func noProgress(*agentv1.TransferProgress) {}
