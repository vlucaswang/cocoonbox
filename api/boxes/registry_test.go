package boxes

import (
	"fmt"
	"regexp"
	"slices"
	"strings"
	"testing"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
	"github.com/loomhand/cocoonbox/api/users"
)

func TestCreateAddsANamedBoxWithinTheRunningLimit(t *testing.T) {
	fake := newFakeMachines()
	fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateRunning, Flavor: defaultFlavor, Revision: 1})
	svc := New(fake, testDefaults, newFakeRegistry())
	ctx := auth.WithUser(t.Context(), alice)

	_, err := svc.Create(ctx, &controlv1.CreateBoxRequest{Name: "test"})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(status.Convert(err).Message(), "box main ("+aliceBox+") is running") {
		t.Fatalf("Create beside a running box: %v, want FailedPrecondition naming main", err)
	}
	if list, _ := svc.List(ctx, &controlv1.ListBoxesRequest{}); len(list.GetBoxes()) != 1 {
		t.Fatalf("a refused Create added a box: %v", list)
	}
	fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateHibernated, Flavor: defaultFlavor, Revision: 2})
	created, err := svc.Create(ctx, &controlv1.CreateBoxRequest{Name: "test"})
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	test := created.GetBox().GetId()
	if !regexp.MustCompile(`^cb-[0-9a-f]{12}$`).MatchString(test) || test == aliceBox || created.GetBox().GetName() != "test" || created.GetOperation().GetId() == "" {
		t.Fatalf("created = %v", created)
	}
	if spec := fake.lastSpec(); spec.ID != test || spec.Owner != alice || spec.Image != "" {
		t.Errorf("spec = %+v", spec)
	}
	list, err := svc.List(ctx, &controlv1.ListBoxesRequest{})
	if err != nil || len(list.GetBoxes()) != 2 || list.GetBoxes()[0].GetName() != "main" || list.GetBoxes()[1].GetId() != test ||
		list.GetBoxes()[1].GetState() != controlv1.BoxState_BOX_STATE_STARTING {
		t.Fatalf("List = %v, %v", list, err)
	}

	_, err = svc.Ensure(ctx, &controlv1.EnsureRequest{BoxId: aliceBox})
	if status.Code(err) != codes.FailedPrecondition || !strings.Contains(status.Convert(err).Message(), "box test ("+test+") is running") {
		t.Fatalf("waking main beside test: %v, want FailedPrecondition naming test", err)
	}
	if again, err := svc.Ensure(ctx, &controlv1.EnsureRequest{BoxId: test}); err != nil || again.GetBox().GetId() != test {
		t.Errorf("Ensure of the starting box = %v, %v", again, err)
	}
	fake.put(machines.Machine{ID: test, Owner: alice, State: machines.StateHibernated, Flavor: defaultFlavor, Revision: 3})
	woken, err := svc.Ensure(ctx, &controlv1.EnsureRequest{})
	if err != nil || woken.GetBox().GetId() != aliceBox || woken.GetOperation().GetId() == "" {
		t.Fatalf("Ensure without a box id = %v, %v, want main woken", woken, err)
	}
	if renamed, err := svc.Rename(ctx, &controlv1.RenameBoxRequest{BoxId: test, Name: "work"}); err != nil || renamed.GetName() != "work" {
		t.Errorf("Rename = %v, %v", renamed, err)
	}
}

func TestTheRunningLimitIsPerUser(t *testing.T) {
	for _, tt := range []struct {
		name    string
		own     int
		running int
	}{
		{name: "inherits the default of 1", own: 0, running: 1},
		{name: "own limit 1", own: 1, running: 1},
		{name: "own limit 3", own: 3, running: 3},
	} {
		t.Run(tt.name, func(t *testing.T) {
			fake := newFakeMachines()
			registry := newFakeRegistry()
			registry.max, registry.running[alice] = 4, tt.own
			svc := New(fake, testDefaults, registry)
			ctx := auth.WithUser(t.Context(), alice)
			if _, err := svc.Ensure(ctx, &controlv1.EnsureRequest{}); err != nil {
				t.Fatalf("Ensure main: %v", err)
			}
			for i := 1; i < tt.running; i++ {
				if _, err := svc.Create(ctx, &controlv1.CreateBoxRequest{Name: fmt.Sprintf("box%d", i)}); err != nil {
					t.Fatalf("Create box %d of %d: %v", i+1, tt.running, err)
				}
			}
			_, err := svc.Create(ctx, &controlv1.CreateBoxRequest{Name: "over"})
			want := fmt.Sprintf("and %d may run at a time", tt.running)
			if status.Code(err) != codes.FailedPrecondition || !strings.Contains(status.Convert(err).Message(), want) {
				t.Fatalf("Create past %d running = %v, want FailedPrecondition with %q", tt.running, err, want)
			}
			if tt.running > 1 && !strings.HasPrefix(status.Convert(err).Message(), "boxes ") {
				t.Errorf("refusal %q does not name every running box", status.Convert(err).Message())
			}
		})
	}
}

func TestCreateResolvesAnImageFamily(t *testing.T) {
	defaults := testDefaults
	defaults.Image, defaults.RebaseImages, defaults.MaxRunning = "cbdesk:v25", []string{"cbcore:v1", "cbdesk:v24"}, 9
	for _, tt := range []struct {
		image, want string
		code        codes.Code
	}{
		{"", "", codes.OK},
		{"cbcore", "cbcore:v1", codes.OK},
		{"cbdesk", "", codes.OK},
		{"cbdesk:v24", "cbdesk:v24", codes.OK},
		{"nosuch", "", codes.InvalidArgument},
		{"cbcore:v9", "", codes.InvalidArgument},
	} {
		fake := newFakeMachines()
		svc := New(fake, defaults, newFakeRegistry())
		_, err := svc.Create(auth.WithUser(t.Context(), alice), &controlv1.CreateBoxRequest{Name: "kind", Image: tt.image})
		if status.Code(err) != tt.code {
			t.Errorf("Create(image %q) = %v, want %s", tt.image, err, tt.code)
			continue
		}
		if err == nil && fake.lastSpec().Image != tt.want {
			t.Errorf("Create(image %q) spec image = %q, want %q", tt.image, fake.lastSpec().Image, tt.want)
		}
	}
	noCore := testDefaults
	noCore.Image, noCore.RebaseImages = "cbdesk:v25", []string{"cbdesk:v24"}
	_, err := New(newFakeMachines(), noCore, newFakeRegistry()).Create(auth.WithUser(t.Context(), alice), &controlv1.CreateBoxRequest{Name: "kind", Image: "cbcore"})
	if want := `image "cbcore" is not offered; choose cbdesk:v25, cbdesk:v24, or the family of one (the name before the tag's ':' after the last '/')`; status.Code(err) != codes.InvalidArgument || status.Convert(err).Message() != want {
		t.Errorf("Create(cbcore) with no cbcore image offered = %v, want InvalidArgument %q", err, want)
	}
}

func TestCreateRefusals(t *testing.T) {
	fake := newFakeMachines()
	svc := New(fake, testDefaults, newFakeRegistry())
	ctx := auth.WithUser(t.Context(), alice)
	for _, tt := range []struct {
		req  *controlv1.CreateBoxRequest
		want codes.Code
	}{
		{&controlv1.CreateBoxRequest{Name: "main"}, codes.AlreadyExists},
		{&controlv1.CreateBoxRequest{Name: "x", Image: "desktop:v9"}, codes.InvalidArgument},
		{&controlv1.CreateBoxRequest{Name: "x", Flavor: &controlv1.Flavor{Vcpus: 64}}, codes.InvalidArgument},
	} {
		if _, err := svc.Create(ctx, tt.req); status.Code(err) != tt.want {
			t.Errorf("Create(%v) = %v, want %s", tt.req, err, tt.want)
		}
	}
	for i, name := range []string{"two", "three"} {
		created, err := svc.Create(ctx, &controlv1.CreateBoxRequest{Name: name, Image: "desktop:v3"})
		if err != nil {
			t.Fatalf("Create %s: %v", name, err)
		}
		if fake.lastSpec().Image != "desktop:v3" {
			t.Errorf("spec image = %q", fake.lastSpec().Image)
		}
		fake.put(machines.Machine{ID: created.GetBox().GetId(), Owner: alice, State: machines.StateHibernated, Revision: int64(i + 2)})
	}
	if _, err := svc.Create(ctx, &controlv1.CreateBoxRequest{Name: "four"}); status.Code(err) != codes.FailedPrecondition {
		t.Errorf("Create past the quota = %v, want FailedPrecondition", err)
	}
	if _, err := svc.Ensure(auth.WithUser(t.Context(), "bob"), &controlv1.EnsureRequest{BoxId: aliceBox}); status.Code(err) != codes.NotFound {
		t.Errorf("Ensure of alice's box by bob = %v, want NotFound", err)
	}
	if _, err := svc.Rename(ctx, &controlv1.RenameBoxRequest{BoxId: aliceBox, Name: "two"}); status.Code(err) != codes.AlreadyExists {
		t.Errorf("Rename onto a taken name = %v, want AlreadyExists", err)
	}
}

func TestEnsureWithoutBoxesAddsOne(t *testing.T) {
	fake := newFakeMachines()
	registry := newFakeRegistry()
	registry.boxes[alice] = nil
	svc := New(fake, testDefaults, registry)
	resp, err := svc.Ensure(auth.WithUser(t.Context(), alice), &controlv1.EnsureRequest{})
	if err != nil {
		t.Fatalf("Ensure: %v", err)
	}
	if id := resp.GetBox().GetId(); id == aliceBox || !strings.HasPrefix(id, "cb-") || resp.GetBox().GetName() != users.FirstBoxName {
		t.Errorf("box = %v, want a new box named main", resp.GetBox())
	}
}

func TestDeleteSavesThenRemovesEveryMachineOfTheBox(t *testing.T) {
	env := newStateEnv(t)
	env.agent.chunks = []string{hexID("c1")}
	env.save(t)
	env.fake.put(machines.Machine{ID: aliceBox + "-rold", Owner: alice, State: machines.StateRunning})
	env.fake.put(machines.Machine{ID: "cb-000000000000", Owner: alice, State: machines.StateHibernated})
	plans := env.agent.plans()

	op, err := env.svc.Delete(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("Delete: %v", err)
	}
	events := watch(t, env.svc, env.ctx, op.GetId(), "")
	last := events[len(events)-1]
	if last.GetPhase() != controlv1.Phase_PHASE_DONE || !slices.ContainsFunc(events, func(ev *controlv1.OperationEvent) bool { return ev.GetPhase() == controlv1.Phase_PHASE_DELETING }) {
		t.Fatalf("delete events = %v", events)
	}
	if env.agent.plans() != plans+1 || env.agent.lastPlan().GetConsistency().String() != "CONSISTENCY_QUIESCED" {
		t.Errorf("delete planned %d saves, last %v, want one quiesced", env.agent.plans()-plans, env.agent.lastPlan())
	}
	if got := env.fake.deletedIDs(); !strings.Contains(got, aliceBox) || !strings.Contains(got, aliceBox+"-rold") || strings.Contains(got, "cb-000000000000") {
		t.Errorf("deleted machines %q, want both of the box and no other", got)
	}
	if !env.store.has(alice + "/boxes/" + aliceBox + "/head.json") {
		t.Error("the deleted box's saved state is gone")
	}
	if _, err := env.svc.Ensure(env.ctx, &controlv1.EnsureRequest{BoxId: aliceBox}); status.Code(err) != codes.NotFound {
		t.Errorf("Ensure of the deleted box = %v, want NotFound", err)
	}
	if list, _ := env.svc.List(env.ctx, &controlv1.ListBoxesRequest{}); len(list.GetBoxes()) != 0 {
		t.Errorf("List after delete = %v", list)
	}
}

func TestDeleteOfAHibernatedBoxSkipsTheSave(t *testing.T) {
	env := newStateEnv(t)
	env.fake.put(machines.Machine{ID: aliceBox, Owner: alice, State: machines.StateHibernated, Revision: 2})
	op, err := env.svc.Delete(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("Delete: %v", err)
	}
	if last := watch(t, env.svc, env.ctx, op.GetId(), ""); last[len(last)-1].GetPhase() != controlv1.Phase_PHASE_DONE {
		t.Fatalf("delete events = %v", last)
	}
	if env.agent.plans() != 0 || env.fake.deletedIDs() != aliceBox {
		t.Errorf("plans %d, deleted %q", env.agent.plans(), env.fake.deletedIDs())
	}
}

func TestDeleteRefusals(t *testing.T) {
	env := newStateEnv(t)
	if _, err := env.svc.Delete(auth.WithUser(t.Context(), "bob"), &controlv1.BoxRef{BoxId: aliceBox}); status.Code(err) != codes.NotFound {
		t.Errorf("Delete by bob = %v, want NotFound", err)
	}
	gate := make(chan struct{})
	env.agent.onUpload = func(machines.Endpoint) { <-gate }
	save, err := env.svc.SaveState(env.ctx, &controlv1.BoxRef{BoxId: aliceBox})
	if err != nil {
		t.Fatalf("SaveState: %v", err)
	}
	if _, err := env.svc.Delete(env.ctx, &controlv1.BoxRef{BoxId: aliceBox}); status.Code(err) != codes.FailedPrecondition || !strings.Contains(err.Error(), save.GetId()) {
		t.Errorf("Delete during a save = %v, want FailedPrecondition naming %s", err, save.GetId())
	}
	close(gate)
	watch(t, env.svc, env.ctx, save.GetId(), "")
}

func TestEachBoxResolvesToItsOwnMachine(t *testing.T) {
	fake := newFakeMachines()
	registry := newFakeRegistry()
	second := users.Box{ID: "cb-0123456789ab", Owner: alice, Name: "test"}
	if _, err := registry.AddBox(t.Context(), second); err != nil {
		t.Fatalf("AddBox: %v", err)
	}
	fake.put(machines.Machine{ID: aliceBox + "-r1", Owner: alice, State: machines.StateRunning})
	fake.put(machines.Machine{ID: second.ID, Owner: alice, State: machines.StateRunning})
	svc := New(fake, testDefaults, registry)
	for box, machine := range map[string]string{aliceBox: aliceBox + "-r1", second.ID: second.ID} {
		ep, _, err := svc.Resolve(t.Context(), alice, box)
		if err != nil || ep.Key != machine+":7443" {
			t.Errorf("Resolve(%s) = %q, %v, want machine %s", box, ep.Key, err, machine)
		}
	}
}
