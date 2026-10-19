package boxes

import (
	"context"
	"encoding/json/v2"
	"errors"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	"github.com/loomhand/cocoonbox/api/cluster"
	"github.com/loomhand/cocoonbox/api/cluster/clustertest"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
)

func TestTwoProcessesCommitHeadsInTurnAndRefuseAStaleOne(t *testing.T) {
	a, b, _ := newSharedPair(t, t.Context())
	a.agent.chunks = hexIDs("c1")
	a.save(t)
	p := statePath{user: alice, box: aliceBox}
	clustertest.WaitFor(t, "b sees a's head and no operation", func() bool {
		h, err := b.svc.state.head(t.Context(), p)
		return err == nil && h.Latest == snapshotID(1) && b.svc.ops.activeFor(aliceBox) == nil
	})
	b.agent.chunks = hexIDs("c1", "c2")
	b.save(t)
	clustertest.WaitFor(t, "a sees b's head", func() bool {
		h, err := a.svc.state.head(t.Context(), p)
		return err == nil && h.Latest == snapshotID(2)
	})
	var op *controlv1.Operation
	clustertest.WaitFor(t, "a takes b's save as ended", func() bool {
		var err error
		op, err = a.svc.SaveState(a.ctx, &controlv1.BoxRef{BoxId: aliceBox})
		return err == nil
	})
	if events := watch(t, a.svc, a.ctx, op.GetId(), ""); events[len(events)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("a's save ended with %v", events[len(events)-1])
	}
	if parent := a.agent.lastPlan().GetParent(); parent != snapshotID(2) {
		t.Errorf("a planned on %q, want b's snapshot", parent)
	}
	if _, err := a.svc.state.swapHead(t.Context(), p, head{Latest: "stale", version: 1}); err == nil || !strings.Contains(err.Error(), "another api process") {
		t.Errorf("swapHead on an old version = %v, want a refusal", err)
	}
}

func TestAHeadsNotifyFillsTheCacheWithoutDroppingANewerHead(t *testing.T) {
	a, b, db := newSharedPair(t, t.Context())
	p, mark := statePath{user: alice, box: aliceBox}, statePath{user: alice, box: "cb-mark"}
	settle := func(latest string) {
		t.Helper()
		puts := 0
		clustertest.WaitFor(t, "both take a notify of "+latest, func() bool {
			if puts%10 == 0 {
				if err := db.PutHead(t.Context(), alice, mark.box, headBytes(head{Latest: latest, SavedAtMs: int64(puts)})); err != nil {
					t.Fatalf("PutHead: %v", err)
				}
			}
			puts++
			return cachedHead(a, mark).Latest == latest && cachedHead(b, mark).Latest == latest
		})
	}
	settle("listening")
	a.agent.chunks = hexIDs("c1")
	a.save(t)
	settle("after")
	if h := cachedHead(a, p); h.Latest != snapshotID(1) || h.version != cachedHead(b, p).version {
		t.Fatalf("a caches %+v after its own notify, b %+v; want the same head", h, cachedHead(b, p))
	}
	if err := b.svc.state.learnHead(cluster.Head{User: alice, Box: aliceBox, Head: headBytes(head{Latest: "old"}), Version: 0}); err != nil {
		t.Fatalf("learnHead: %v", err)
	}
	if h := cachedHead(b, p); h.Latest != snapshotID(1) {
		t.Errorf("an older head replaced the cached one: %+v", h)
	}
}

func TestAWatchResumedInAnotherProcessGetsTheEventsAfterItsOffset(t *testing.T) {
	a, b, _ := newSharedPair(t, t.Context())
	op, _ := a.svc.ops.start(a.ctx, alice, aliceBox, controlv1.Phase_PHASE_SAVING, 1, time.Minute)
	for i := range 10 {
		op.progress(controlv1.Phase_PHASE_SAVING, "upload", uint64(i+1), 10)
	}
	events, _, _ := op.since(0)
	from := events[len(events)-1].GetOffsetKey()
	op.succeed("saved")
	clustertest.WaitFor(t, "b reads a's finished operation", func() bool {
		remote, err := b.svc.ops.get(t.Context(), op.id)
		if err != nil || remote == nil {
			return false
		}
		_, done := remote.finished()
		return done
	})
	events = watch(t, b.svc, b.ctx, op.id, from)
	if len(events) != 1 || events[0].GetPhase() != controlv1.Phase_PHASE_DONE || events[0].GetOffsetKey() <= from {
		t.Errorf("a watch on b resumed after %s got %v, want the one event after it", from, events)
	}
}

func TestAParkClaimAndASupersedeFromAnotherProcessExcludeEachOther(t *testing.T) {
	a, b, db := newSharedPair(t, t.Context())
	park := func() *operation {
		op, _ := a.svc.ops.begin(a.ctx, &operation{user: alice, boxID: aliceBox, phase: controlv1.Phase_PHASE_SAVING, park: true, gen: 1}, time.Minute)
		clustertest.WaitFor(t, "b sees a's park", func() bool {
			active := b.svc.ops.activeFor(aliceBox)
			return active != nil && active.id == op.id
		})
		return op
	}
	superseded := park()
	if !b.svc.ops.end(t.Context(), superseded.id, "superseded by operation of b") {
		t.Fatal("b could not end a's unclaimed park")
	}
	if a.svc.ops.claim(t.Context(), superseded) {
		t.Error("a claimed a park b had superseded in the table")
	}
	clustertest.WaitFor(t, "a takes b's end of its park", func() bool {
		_, done := superseded.finished()
		return done
	})
	claimed := park()
	if !a.svc.ops.claim(t.Context(), claimed) {
		t.Fatal("a could not claim a park nobody superseded")
	}
	if op, started := b.svc.ops.start(b.ctx, alice, aliceBox, controlv1.Phase_PHASE_SAVING, 1, time.Minute); started || op.id != claimed.id {
		t.Errorf("b's operation over a claimed park = %s started %t; want a's park kept", op.id, started)
	}
	if rec, err := db.Operation(t.Context(), claimed.id); err != nil || !rec.FinishedAt.IsZero() || !rec.Releasing {
		t.Errorf("claimed park row = %+v, %v; want it unfinished and releasing", rec, err)
	}
}

func TestASharedParkWhoseMachineRemovalFailsEndsFailed(t *testing.T) {
	e := newParkEnv(t)
	db := clustertest.Open(t)
	e.svc.EnableLedger(t.Context(), db)
	e.svc.EnableShared(t.Context(), db)
	e.fake.deleteErr = errors.New("sandboxd refused the release")
	op := e.park(t, e.clock.Add(parkAfter))
	if op == nil {
		t.Fatal("no park")
	}
	if last := lastEvent(op); last.GetPhase() != controlv1.Phase_PHASE_FAILED || !strings.Contains(last.GetDetail(), "hibernated again") {
		t.Errorf("park whose removal failed ended with %v", last)
	}
	clustertest.WaitFor(t, "the table ends the park failed", func() bool {
		rec, err := db.Operation(t.Context(), op.id)
		events := fromEventRecords(rec.ID, rec.Events)
		return err == nil && !rec.FinishedAt.IsZero() && len(events) > 0 && events[len(events)-1].GetPhase() == controlv1.Phase_PHASE_FAILED
	})
}

func TestAnEnsureRacingAParkInAnotherProcessRestoresTheParkSnapshot(t *testing.T) {
	db := clustertest.Open(t)
	bCtx, deaf := context.WithCancel(t.Context())
	a := newStateEnv(t)
	b := &stateEnv{ctx: auth.WithUser(t.Context(), alice), fake: a.fake, agent: a.agent, store: a.store, keys: a.keys}
	b.svc = New(a.fake, testDefaults, a.svc.registry)
	b.svc.EnableState(a.agent, a.store, a.keys)
	a.svc.EnableLedger(t.Context(), db)
	a.svc.EnableShared(t.Context(), db)
	b.svc.EnableLedger(t.Context(), db)
	b.svc.EnableShared(bCtx, db)
	e := &parkEnv{stateEnv: a}
	e.agent.chunks = []string{hexID("c1")}
	e.save(t)
	p := statePath{user: alice, box: aliceBox}
	if h, err := b.svc.state.head(t.Context(), p); err != nil || h.Latest != snapshotID(1) {
		t.Fatalf("b's head before the park = %+v, %v", h, err)
	}
	e.clock = time.Now()
	e.svc.EnableParking(parkAfter)
	e.svc.park.now = func() time.Time { return e.clock }
	if _, err := e.svc.Hibernate(e.ctx, &controlv1.BoxRef{BoxId: aliceBox}); err != nil {
		t.Fatalf("Hibernate: %v", err)
	}
	e.fake.push(machines.Machine{ID: aliceBox, State: machines.StateHibernated})
	e.idle(t)
	e.agent.onUpload = func(machines.Endpoint) {
		e.agent.onUpload = nil
		clustertest.WaitFor(t, "b sees a's park running", func() bool {
			active := b.svc.ops.activeFor(aliceBox)
			return active != nil && active.park
		})
		deaf()
	}
	if op := e.park(t, e.clock.Add(parkAfter)); op == nil || lastEvent(op).GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("a's park = %v", op)
	}
	if _, err := b.svc.Ensure(b.ctx, &controlv1.EnsureRequest{BoxId: aliceBox}); err != nil {
		t.Fatalf("Ensure on b after a's park: %v", err)
	}
	spec := e.fake.waitSpec(t, 2)
	if !strings.HasPrefix(spec.ID, aliceBox+"-r") {
		t.Fatalf("b's Ensure made machine %q, want a restore of the parked box", spec.ID)
	}
	clustertest.WaitFor(t, "b's restore ends", func() bool {
		e.fake.push(machines.Machine{ID: spec.ID, State: machines.StateRunning})
		return b.svc.ops.activeFor(aliceBox) == nil
	})
	if restore := e.agent.lastRestore(); len(restore.reqs) == 0 || restore.reqs[0].GetManifest().GetChunkId() != snapshotID(2) {
		t.Errorf("b restored %v, want the park's snapshot", restore.reqs)
	}
}

func TestASnapshotOneProcessPinsIsKeptByTheGCOfAnother(t *testing.T) {
	a, b, _ := newSharedPair(t, t.Context())
	for _, chunks := range [][]string{{"c1"}, {"c2"}} {
		a.agent.chunks = hexIDs(chunks...)
		a.save(t)
	}
	p := statePath{user: alice, box: aliceBox}
	unpin, err := a.svc.state.pin(t.Context(), p, snapshotID(1))
	if err != nil {
		t.Fatalf("pin: %v", err)
	}
	later := time.Now().Add(40 * 24 * time.Hour)
	if _, err := b.svc.state.collect(t.Context(), alice, GCOptions{DeleteBatch: 1000}, later, everyBoxLive); err != nil {
		t.Fatalf("collect while pinned: %v", err)
	}
	if !a.store.has(p.record(snapshotID(1))) {
		t.Fatal("the other process's gc removed a snapshot this one pinned")
	}
	unpin()
	if _, err := b.svc.state.collect(t.Context(), alice, GCOptions{DeleteBatch: 1000}, later, everyBoxLive); err != nil {
		t.Fatalf("collect after the unpin: %v", err)
	}
	if a.store.has(p.record(snapshotID(1))) {
		t.Error("the other process's gc kept a snapshot no pin holds")
	}
}

func TestTwoProcessesProvisioningOneMachineProvisionItOnce(t *testing.T) {
	a, b, _ := newSharedPair(t, t.Context())
	a.svc.EnableProvisioning(fakeSigner{})
	b.svc.EnableProvisioning(fakeSigner{})
	a.fake.onDeliver = func() { time.Sleep(100 * time.Millisecond) }
	a.fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateRunning, Flavor: defaultFlavor, Incarnation: "vm1", Revision: 2})
	m, err := a.fake.Get(t.Context(), aliceBox)
	if err != nil {
		t.Fatalf("Get: %v", err)
	}
	before := len(a.fake.execCalls())
	var wg sync.WaitGroup
	for _, env := range []*stateEnv{a, b} {
		wg.Go(func() {
			if err := env.svc.provisionMachine(t.Context(), m); err != nil {
				t.Errorf("provisionMachine: %v", err)
			}
		})
	}
	wg.Wait()
	if n := len(a.fake.execCalls()) - before; n != 1 {
		t.Errorf("deliveries = %d, want one for two processes provisioning one machine", n)
	}
}

func TestAFileRestoreInOneProcessWaitsOutARebaseElsewhere(t *testing.T) {
	db := clustertest.Open(t)
	bCtx, deaf := context.WithCancel(t.Context())
	a := newStateEnv(t)
	b := &stateEnv{ctx: auth.WithUser(t.Context(), alice), fake: a.fake, agent: a.agent, store: a.store, keys: a.keys}
	b.svc = New(a.fake, testDefaults, a.svc.registry)
	b.svc.EnableState(a.agent, a.store, a.keys)
	a.svc.EnableLedger(t.Context(), db)
	a.svc.EnableShared(t.Context(), db)
	b.svc.EnableLedger(t.Context(), db)
	b.svc.EnableShared(bCtx, db)
	a.agent.files = sampleFiles()
	a.save(t)
	first := a.headID(t)
	deaf()
	gate, saving := make(chan struct{}), make(chan struct{})
	a.agent.onUpload = func(machines.Endpoint) {
		a.agent.onUpload = nil
		close(saving)
		<-gate
	}
	if _, err := a.svc.Rebase(a.ctx, &controlv1.RebaseRequest{BoxId: aliceBox, Image: "desktop:v3"}); err != nil {
		t.Fatalf("Rebase: %v", err)
	}
	<-saving
	b.svc.ops.mu.Lock()
	if op := b.svc.ops.active[aliceBox]; op != nil && op.remote {
		b.svc.ops.forgetLocked(op)
	}
	b.svc.ops.mu.Unlock()
	_, err := b.svc.RestorePaths(b.ctx, &controlv1.RestorePathsRequest{BoxId: aliceBox, SnapshotId: first, Paths: []string{"/workspace/app/main.go"}})
	close(gate)
	if status.Code(err) != codes.FailedPrecondition {
		t.Errorf("RestorePaths on b during a's rebase = %v, want FailedPrecondition, since the rebase removes the machine it would write into", err)
	}
}

func TestAStaleRowNeverReopensOrRewindsAnotherProcesssOperation(t *testing.T) {
	_, b, _ := newSharedPair(t, t.Context())
	events := func(n int) []byte {
		recs := make([]eventRecord, n)
		for i := range recs {
			recs[i] = eventRecord{Phase: int32(controlv1.Phase_PHASE_SAVING), OffsetKey: strconv.Itoa(i + 1)}
		}
		raw, _ := json.Marshal(recs)
		return raw
	}
	row := cluster.Operation{
		ID: "op-x", User: alice, Box: aliceBox, Phase: int32(controlv1.Phase_PHASE_SAVING), Events: events(3),
		ExpiresAt: time.Now().Add(time.Minute), CreatedAt: time.Now(),
	}
	r := b.svc.ops
	r.mu.Lock()
	defer r.mu.Unlock()
	op := r.takeLocked(row)
	older := row
	older.Events = events(2)
	r.takeLocked(older)
	if got, _, _ := op.since(0); len(got) != 3 {
		t.Errorf("an older row rewound the copy to %d events, want 3", len(got))
	}
	finished := row
	finished.Events, finished.FinishedAt = events(4), time.Now()
	r.takeLocked(finished)
	r.takeLocked(row)
	if _, done := op.finished(); !done || r.activeLocked(aliceBox) != nil {
		t.Error("a row read before the operation ended reopened it")
	}
}

func TestAnOperationOfOneProcessRunsAndEndsInTheOther(t *testing.T) {
	a, b, _ := newSharedPair(t, t.Context())
	gate := make(chan struct{})
	a.agent.onUpload = func(machines.Endpoint) { <-gate }
	a.agent.chunks = hexIDs("c1")
	op, err := a.svc.SaveState(a.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("SaveState: %v", err)
	}
	clustertest.WaitFor(t, "b sees a's save running", func() bool {
		active := b.svc.ops.activeFor(aliceBox)
		return active != nil && active.id == op.GetId() && active.remote
	})
	if _, err := b.svc.SaveState(b.ctx, &controlv1.BoxRef{BoxId: aliceBox}); err == nil {
		t.Error("b started a save while a's ran")
	}
	close(gate)
	events := watch(t, b.svc, b.ctx, op.GetId(), "")
	if last := events[len(events)-1]; last.GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("b watched a's save end with %v", last)
	}
	clustertest.WaitFor(t, "b forgets a's finished save", func() bool { return b.svc.ops.activeFor(aliceBox) == nil })
}

func TestAnOperationAnotherProcessSupersedesFailsWhereItRuns(t *testing.T) {
	a, b, _ := newSharedPair(t, t.Context())
	gate := make(chan struct{})
	defer close(gate)
	a.agent.onUpload = func(machines.Endpoint) { <-gate }
	a.agent.chunks = hexIDs("c1")
	op, err := a.svc.SaveState(a.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("SaveState: %v", err)
	}
	clustertest.WaitFor(t, "b sees a's save running", func() bool { return b.svc.ops.activeFor(aliceBox) != nil })
	next, started := b.svc.ops.start(b.ctx, alice, aliceBox, controlv1.Phase_PHASE_DELETING, 1, time.Minute)
	if !started {
		t.Fatal("b did not start its operation over a's")
	}
	defer next.succeed("done")
	clustertest.WaitFor(t, "a's save fails as superseded", func() bool {
		local := a.svc.ops.local(op.GetId())
		at, done := local.finished()
		return done && !at.IsZero() && strings.Contains(local.last.detail+lastEventDetail(local), "superseded by operation "+next.id)
	})
	if d, err := a.svc.Verify(t.Context()); err != nil || len(d.Operations) != 0 {
		t.Errorf("Verify on a after the supersede = %+v, %v, want the table's events kept", d, err)
	}
}

func TestTheOperationsOfAStoppedProcessEndAndFreeTheBox(t *testing.T) {
	aCtx, stopA := context.WithCancel(t.Context())
	a, b, _ := newSharedPair(t, aCtx)
	gate := make(chan struct{})
	defer close(gate)
	a.agent.onUpload = func(machines.Endpoint) { <-gate }
	a.agent.chunks = hexIDs("c1")
	op, err := a.svc.SaveState(a.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("SaveState: %v", err)
	}
	clustertest.WaitFor(t, "b sees a's save running", func() bool { return b.svc.ops.activeFor(aliceBox) != nil })
	stopA()
	clustertest.WaitFor(t, "b sees the box free once a stopped", func() bool { return b.svc.ops.activeFor(aliceBox) == nil })
	events := watch(t, b.svc, b.ctx, op.GetId(), "")
	if last := events[len(events)-1]; last.GetPhase() != controlv1.Phase_PHASE_FAILED || !strings.Contains(last.GetDetail(), "stopped") {
		t.Errorf("a's save ended with %v, want a failure saying its process stopped", last)
	}
}

func TestAClaimedParkOfAGoneProcessFreesTheBoxAtItsLease(t *testing.T) {
	_, b, db := newSharedPair(t, t.Context())
	if err := db.PutOperation(t.Context(), cluster.Operation{
		ID: "op-claimed", User: alice, Box: aliceBox, Phase: int32(controlv1.Phase_PHASE_SAVING), Park: true, Events: []byte(`[]`),
		ExpiresAt: time.Now().Add(200 * time.Millisecond), CreatedAt: time.Now(),
	}); err != nil {
		t.Fatalf("PutOperation: %v", err)
	}
	if claimed, err := db.ClaimOperation(t.Context(), "op-claimed"); err != nil || !claimed {
		t.Fatalf("ClaimOperation = %t, %v", claimed, err)
	}
	clustertest.WaitFor(t, "b sees the claimed park", func() bool { return b.svc.ops.activeFor(aliceBox) != nil })
	clustertest.WaitFor(t, "b takes the claimed park as gone past its lease", func() bool { return b.svc.ops.activeFor(aliceBox) == nil })
	if op, started := b.svc.ops.start(b.ctx, alice, aliceBox, controlv1.Phase_PHASE_SAVING, 1, time.Minute); !started || op.id == "op-claimed" {
		t.Errorf("b's operation after the lease = %s started %t; want a new one", op.id, started)
	}
	if err := db.PurgeOperations(t.Context(), time.Now().Add(time.Second)); err != nil {
		t.Fatalf("PurgeOperations: %v", err)
	}
	if _, err := db.Operation(t.Context(), "op-claimed"); !errors.Is(err, cluster.ErrNotFound) {
		t.Errorf("the claimed row after the purge = %v, want it gone", err)
	}
}

func TestAnOperationPastItsLeaseCountsAsGone(t *testing.T) {
	_, b, db := newSharedPair(t, t.Context())
	if err := db.PutOperation(t.Context(), cluster.Operation{
		ID: "op-dead", User: alice, Box: aliceBox, Phase: int32(controlv1.Phase_PHASE_SAVING), Events: []byte(`[]`),
		ExpiresAt: time.Now().Add(200 * time.Millisecond), CreatedAt: time.Now(),
	}); err != nil {
		t.Fatalf("PutOperation: %v", err)
	}
	clustertest.WaitFor(t, "b sees the operation", func() bool { return b.svc.ops.activeFor(aliceBox) != nil })
	clustertest.WaitFor(t, "b takes the operation as gone past its lease", func() bool { return b.svc.ops.activeFor(aliceBox) == nil })
	events := watch(t, b.svc, b.ctx, "op-dead", "")
	if last := events[len(events)-1]; last.GetPhase() != controlv1.Phase_PHASE_FAILED {
		t.Errorf("watching an operation past its lease ended with %v", last)
	}
}

func TestAClientOfOneProcessKeepsTheBoxUpForTheOthersIdleSweep(t *testing.T) {
	db := clustertest.Open(t)
	a, clock := newUserIdleEnv(t)
	b := &stateEnv{ctx: auth.WithUser(t.Context(), alice), fake: a.fake, agent: a.agent, store: a.store, keys: a.keys}
	b.svc = New(a.fake, testDefaults, a.svc.registry)
	b.svc.EnableState(a.agent, a.store, a.keys)
	b.svc.EnableIdle(0, userIdleAfter)
	b.svc.idle.now = a.svc.idle.now
	for _, env := range []*stateEnv{a, b} {
		env.svc.EnableLedger(t.Context(), db)
		env.svc.EnableShared(t.Context(), db)
	}
	a.svc.hibernateIdle(t.Context())
	done := b.svc.Reached(aliceBox)
	defer done()
	*clock = clock.Add(userIdleAfter)
	b.svc.reaches.flush(t.Context(), b.svc.openBoxes())
	a.svc.hibernateIdle(t.Context())
	if op := newestOperation(a.svc, aliceBox); op != nil || a.fake.state(aliceBox) != machines.StateRunning {
		t.Errorf("a's sweep started an operation (%t) on a box a client of b holds, state %v", op != nil, a.fake.state(aliceBox))
	}
}

func TestTheFirstReachAfterAQuietSpellReachesTheTableAtOnce(t *testing.T) {
	_, b, db := newSharedPair(t, t.Context())
	b.svc.Reached(aliceBox)()
	clustertest.WaitFor(t, "the reach is in the table before the next flush", func() bool {
		seen, err := db.Activity(t.Context(), time.Now().Add(-time.Minute))
		return err == nil && !seen[aliceBox].IsZero()
	})
}

// newSharedPair returns two services on one machine backend, store and shared database, as two api processes see them; a's process runs until aCtx ends.
func newSharedPair(t *testing.T, aCtx context.Context) (*stateEnv, *stateEnv, *cluster.Store) {
	t.Helper()
	db := clustertest.Open(t)
	a := newStateEnv(t)
	b := &stateEnv{ctx: auth.WithUser(t.Context(), alice), fake: a.fake, agent: a.agent, store: a.store, keys: a.keys}
	b.svc = New(a.fake, testDefaults, a.svc.registry)
	b.svc.EnableState(a.agent, a.store, a.keys)
	a.svc.EnableLedger(aCtx, db)
	a.svc.EnableShared(aCtx, db)
	b.svc.EnableLedger(t.Context(), db)
	b.svc.EnableShared(t.Context(), db)
	return a, b, db
}

func lastEventDetail(op *operation) string {
	op.mu.Lock()
	defer op.mu.Unlock()
	if len(op.events) == 0 {
		return ""
	}
	return op.events[len(op.events)-1].GetDetail()
}

func cachedHead(env *stateEnv, p statePath) head {
	env.svc.state.mu.Lock()
	defer env.svc.state.mu.Unlock()
	return env.svc.state.heads[p]
}
