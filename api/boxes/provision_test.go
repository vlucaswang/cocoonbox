package boxes

import (
	"context"
	"errors"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	"github.com/loomhand/cocoonbox/api/credential"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
	"github.com/loomhand/cocoonbox/api/users"
)

func TestEnsureProvisionsBeforeRunning(t *testing.T) {
	fake := newFakeMachines()
	accounts := &fakeAccounts{}
	svc := New(fake, testDefaults, newFakeRegistry())
	svc.EnableAccounts(accounts)
	svc.EnableProvisioning(fakeSigner{})
	ctx := auth.WithUser(t.Context(), alice)

	resp, err := svc.Ensure(ctx, &controlv1.EnsureRequest{})
	if err != nil {
		t.Fatalf("Ensure: %v", err)
	}
	if spec := fake.lastSpec(); spec.Tenant != "tenant-"+alice {
		t.Errorf("claimed with tenant %q, want the user's", spec.Tenant)
	}
	fake.push(machines.Machine{ID: aliceBox, State: machines.StateRunning, Incarnation: "pod1/vm1"})
	events := watch(t, svc, ctx, resp.GetOperation().GetId(), "")
	last := events[len(events)-1]
	if last.GetPhase() != controlv1.Phase_PHASE_DONE || last.GetBoxState() != controlv1.BoxState_BOX_STATE_RUNNING {
		t.Fatalf("last event = %v", last)
	}
	if !hasDetail(events, "provisioning") {
		t.Errorf("no provisioning event in %v", events)
	}

	execs := fake.execCalls()
	if len(execs) != 1 || execs[0].gwKey != "gw-key-"+alice {
		t.Fatalf("deliveries = %+v, want one with the gw key", execs)
	}
	file := execs[0].identity
	if file.BoxID != aliceBox || file.Name != users.FirstBoxName || file.KeyID != "k1" || file.PublicKey != "pub1" || !regexp.MustCompile(`^[0-9a-f]{32}$`).MatchString(file.WakeID) {
		t.Errorf("identity = %+v", file)
	}
	if accounts.calls() != "key "+alice {
		t.Errorf("account calls = %q", accounts.calls())
	}
	ep, wake, err := svc.Resolve(ctx, alice, aliceBox)
	if err != nil || wake != file.WakeID || ep.Key == "" {
		t.Errorf("Resolve = %q, %q, %v, want wake %s", ep.Key, wake, err, file.WakeID)
	}
}

func TestWakeReprovisionsIdentityOnly(t *testing.T) {
	fake := newFakeMachines()
	fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateRunning, Incarnation: "pod1/vm1", Meta: map[string]string{metaGW: "1", metaWake: "pod1/vm1|w-old"}})
	accounts := &fakeAccounts{}
	svc := New(fake, testDefaults, newFakeRegistry())
	svc.EnableAccounts(accounts)
	svc.EnableProvisioning(fakeSigner{})
	ctx := auth.WithUser(t.Context(), alice)

	if _, wake, err := svc.Resolve(ctx, alice, aliceBox); err != nil || wake != "w-old" {
		t.Fatalf("Resolve before wake = %q, %v", wake, err)
	}
	if _, err := svc.Rename(ctx, &controlv1.RenameBoxRequest{BoxId: aliceBox, Name: "desk_1"}); err != nil {
		t.Fatalf("Rename: %v", err)
	}
	fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateRunning, Incarnation: "pod1/vm2", Meta: map[string]string{metaGW: "1", metaWake: "pod1/vm1|w-old"}})
	if _, _, err := svc.Resolve(ctx, alice, aliceBox); status.Code(err) != codes.Unavailable || status.Convert(err).Message() != "box is BOX_STATE_STARTING" {
		t.Errorf("Resolve of an unprovisioned incarnation: %v, want Unavailable box is BOX_STATE_STARTING", err)
	}
	if box, err := svc.Get(ctx, &controlv1.GetBoxRequest{BoxId: aliceBox}); err != nil || box.GetState() != controlv1.BoxState_BOX_STATE_STARTING {
		t.Errorf("Get = %v, %v, want STARTING", box, err)
	}

	runCtx, cancel := context.WithCancel(t.Context())
	defer cancel()
	go svc.RunProvisioner(runCtx, 10*time.Millisecond)
	deadline := time.Now().Add(5 * time.Second)
	for {
		if _, wake, err := svc.Resolve(ctx, alice, aliceBox); err == nil && wake != "w-old" {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("provisioner did not provision the new incarnation")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if accounts.calls() != "" {
		t.Errorf("wake asked for a gw key again: %q", accounts.calls())
	}
	if execs := fake.execCalls(); len(execs) != 1 || execs[0].gwKey != "" || execs[0].identity.WakeID == "w-old" || execs[0].identity.Name != "desk_1" {
		t.Errorf("deliveries = %+v, want one identity named desk_1 without a gw key", execs)
	}
}

func TestProvisioningCoversEveryUser(t *testing.T) {
	fake := newFakeMachines()
	fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateRunning, Incarnation: "pod1/vm1"})
	svc := New(fake, testDefaults, newFakeRegistry())
	svc.EnableAccounts(&fakeAccounts{})
	svc.EnableProvisioning(fakeSigner{})
	if _, _, err := svc.Resolve(auth.WithUser(t.Context(), alice), alice, aliceBox); status.Code(err) != codes.Unavailable {
		t.Errorf("Resolve before provisioning: %v, want Unavailable", err)
	}
	svc.provisionStale(t.Context())
	if execs := fake.execCalls(); len(execs) != 1 || execs[0].identity.BoxID != aliceBox {
		t.Fatalf("deliveries = %+v, want one for %s", execs, aliceBox)
	}
	if _, wake, err := svc.Resolve(auth.WithUser(t.Context(), alice), alice, aliceBox); err != nil || wake == "" {
		t.Errorf("Resolve after provisioning = %q, %v", wake, err)
	}
}

func TestProvisioningFailureFailsTheOperation(t *testing.T) {
	fake := newFakeMachines()
	fake.execErr = errors.New("exec: 502")
	svc := New(fake, testDefaults, newFakeRegistry())
	svc.EnableProvisioning(fakeSigner{})
	svc.provision.backoff = time.Millisecond
	ctx := auth.WithUser(t.Context(), alice)
	resp, err := svc.Ensure(ctx, &controlv1.EnsureRequest{})
	if err != nil {
		t.Fatalf("Ensure: %v", err)
	}
	fake.push(machines.Machine{ID: aliceBox, State: machines.StateRunning, Incarnation: "pod1/vm1"})
	events := watchWithin(t, svc, ctx, resp.GetOperation().GetId(), 20*time.Second)
	if last := events[len(events)-1]; last.GetPhase() != controlv1.Phase_PHASE_FAILED || last.GetDetail() != "machine failed to start" {
		t.Errorf("last event = %v", last)
	}
}

func TestRebaseClaimsUnderTheTenant(t *testing.T) {
	env := newStateEnv(t)
	accounts := &fakeAccounts{}
	env.fake.put(machines.Machine{
		ID: aliceBox, Owner: alice, State: machines.StateRunning, Flavor: defaultFlavor, Image: "desktop:v1", Revision: 1,
		Incarnation: "pod0/vm0", Meta: map[string]string{metaGW: "1", metaWake: "pod0/vm0|w0"},
	})
	env.svc.EnableAccounts(accounts)
	env.svc.EnableProvisioning(fakeSigner{})
	env.agent.chunks = []string{hexID("c1")}
	op, err := env.svc.Rebase(env.ctx, &controlv1.RebaseRequest{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("Rebase: %v", err)
	}
	spec := env.fake.waitSpec(t, 1)
	newID := spec.ID
	if spec.Tenant != "tenant-"+alice {
		t.Errorf("rebase machine claimed with tenant %q, want the user's", spec.Tenant)
	}
	env.fake.push(machines.Machine{ID: newID, State: machines.StateRunning, Incarnation: "pod1/vm1"})
	events := watch(t, env.svc, env.ctx, op.GetId(), "")
	if last := events[len(events)-1]; last.GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("last event = %v", last)
	}
	if got := env.agent.planCredential(); got != "cred:"+aliceBox+":w0" {
		t.Errorf("save on the old machine sent credential %q", got)
	}
	restoreCred := env.agent.restoreCredential()
	if !strings.HasPrefix(restoreCred, "cred:"+aliceBox+":") || restoreCred == "cred:"+aliceBox+":w0" {
		t.Errorf("restore on the new machine sent credential %q", restoreCred)
	}
	if want := "key " + alice; accounts.calls() != want {
		t.Errorf("account calls = %q, want %q", accounts.calls(), want)
	}
}

func TestARestoredMachineGetsItsOwnIDAndTheUsersGWKey(t *testing.T) {
	env := newStateEnv(t)
	env.agent.chunks = []string{hexID("c1")}
	env.save(t)
	accounts := &fakeAccounts{}
	env.svc.EnableAccounts(accounts)
	env.svc.EnableProvisioning(fakeSigner{})
	env.fake.mu.Lock()
	delete(env.fake.machines, aliceBox)
	env.fake.mu.Unlock()

	resp, err := env.svc.Ensure(env.ctx, &controlv1.EnsureRequest{})
	if err != nil {
		t.Fatalf("Ensure: %v", err)
	}
	newID := env.fake.waitSpec(t, 1).ID
	if !strings.HasPrefix(newID, aliceBox+"-r") {
		t.Fatalf("restored machine id = %q, want %s-r<ts>", newID, aliceBox)
	}
	env.fake.push(machines.Machine{ID: newID, State: machines.StateRunning, Incarnation: "pod1/vm1"})
	if last := watch(t, env.svc, env.ctx, resp.GetOperation().GetId(), ""); last[len(last)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("restore ended with %v", last[len(last)-1])
	}
	if want := "key " + alice; accounts.calls() != want {
		t.Errorf("account calls = %q, want %q", accounts.calls(), want)
	}
}

func TestReleaseDeletesEveryMachineOfTheUser(t *testing.T) {
	fake := newFakeMachines()
	fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateRunning})
	fake.put(machines.Machine{ID: aliceBox + "-r1", Owner: alice, State: machines.StateRunning})
	fake.put(machines.Machine{ID: BoxID("bob"), Owner: "bob", State: machines.StateRunning})
	svc := New(fake, testDefaults, newFakeRegistry())
	if err := svc.Release(t.Context(), alice); err != nil {
		t.Fatalf("Release: %v", err)
	}
	if ms, _ := fake.List(t.Context(), alice); len(ms) != 0 {
		t.Errorf("alice keeps %d machines", len(ms))
	}
	if ms, _ := fake.List(t.Context(), "bob"); len(ms) != 1 {
		t.Errorf("bob has %d machines, want 1", len(ms))
	}
}

func watchWithin(t *testing.T, svc *Service, ctx context.Context, opID string, d time.Duration) []*controlv1.OperationEvent {
	t.Helper()
	ctx, cancel := context.WithTimeout(ctx, d)
	defer cancel()
	stream := &eventStream{ctx: ctx}
	if err := svc.WatchOperation(&controlv1.WatchOperationRequest{OperationId: opID}, stream); err != nil {
		t.Fatalf("WatchOperation: %v", err)
	}
	return stream.events
}

func hasDetail(events []*controlv1.OperationEvent, detail string) bool {
	for _, ev := range events {
		if ev.GetDetail() == detail {
			return true
		}
	}
	return false
}

func outgoingCredential(ctx context.Context) string {
	md, _ := metadata.FromOutgoingContext(ctx)
	if v := md.Get(credential.Header); len(v) == 1 {
		return v[0]
	}
	return ""
}

type fakeSigner struct{}

func (fakeSigner) KeyID() string                      { return "k1" }
func (fakeSigner) PublicKey() string                  { return "pub1" }
func (fakeSigner) Credential(box, wake string) string { return "cred:" + box + ":" + wake }

type fakeAccounts struct {
	mu  sync.Mutex
	log []string
}

func (a *fakeAccounts) Tenant(user string) (string, error) { return "tenant-" + user, nil }

func (a *fakeAccounts) GWKey(user string) (string, error) {
	a.record("key " + user)
	return "gw-key-" + user, nil
}

func (a *fakeAccounts) record(call string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.log = append(a.log, call)
}

func (a *fakeAccounts) calls() string {
	a.mu.Lock()
	defer a.mu.Unlock()
	return strings.Join(a.log, ";")
}
