package boxes

import (
	"context"
	"errors"
	"testing"
	"time"

	"github.com/loomhand/cocoonbox/api/auth"
	agentv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/agent/v1"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
	"github.com/loomhand/cocoonbox/api/users"
)

const userIdleAfter = 2 * time.Hour

func TestAnIdleUserBoxIsSavedLiveThenHibernated(t *testing.T) {
	env, clock := newUserIdleEnv(t)
	env.svc.hibernateIdle(t.Context())
	*clock = clock.Add(userIdleAfter - time.Second)
	env.svc.hibernateIdle(t.Context())
	if got := env.fake.state(aliceBox); got != machines.StateRunning {
		t.Fatalf("state = %v under the threshold", got)
	}
	*clock = clock.Add(time.Second)
	env.svc.hibernateIdle(t.Context())
	waitState(t, env.fake, aliceBox, machines.StateHibernating)
	if req := env.agent.lastPlan(); req.GetConsistency() != agentv1.Consistency_CONSISTENCY_LIVE {
		t.Errorf("the save before the hibernation was %v, want a live one", req.GetConsistency())
	}
}

func TestAnIdleUserBoxStaysUpWhileSomethingKeepsIt(t *testing.T) {
	tests := []struct {
		name  string
		setup func(env *stateEnv, activity *fakeActivity, clock *time.Time)
	}{
		{"an agent turn or offloaded session works", func(_ *stateEnv, a *fakeActivity, _ *time.Time) { a.busy = true }},
		{"its agent cannot answer", func(_ *stateEnv, a *fakeActivity, _ *time.Time) { a.err = errors.New("unavailable") }},
		{"a client call is open", func(e *stateEnv, _ *fakeActivity, _ *time.Time) { e.svc.Reached(aliceBox) }},
		{"its scheduled wake is on", func(e *stateEnv, _ *fakeActivity, clock *time.Time) {
			store := &fakeWakeStore{schedules: map[string]users.WakeSchedule{aliceBox: {Owner: alice, Box: aliceBox, SleepAt: clock.Add(3 * userIdleAfter)}}}
			e.svc.EnableWake(store, testWakeLimits)
		}},
		{"user boxes never idle out", func(e *stateEnv, _ *fakeActivity, _ *time.Time) { e.svc.idle.user = 0 }},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			env, clock := newUserIdleEnv(t)
			env.svc.hibernateIdle(t.Context())
			tt.setup(env, env.svc.activity.(*fakeActivity), clock)
			*clock = clock.Add(2 * userIdleAfter)
			env.svc.hibernateIdle(t.Context())
			if got := env.fake.state(aliceBox); got != machines.StateRunning || env.svc.ops.activeFor(aliceBox) != nil {
				t.Errorf("state = %v with operation %v, want it running with none", got, env.svc.ops.activeFor(aliceBox))
			}
		})
	}
}

func TestAWedgedAgentDoesNotStallTheSweepForTheNextBox(t *testing.T) {
	env, clock := newUserIdleEnv(t)
	bobBox := BoxID(bob)
	env.fake.put(machines.Machine{ID: bobBox, Owner: bob, State: machines.StateRunning, Flavor: defaultFlavor, Image: "desktop:v1", Revision: 1})
	hung, next := min(aliceBox, bobBox), max(aliceBox, bobBox)
	env.svc.activity.(*fakeActivity).hang = hung
	env.svc.idle.probe = 50 * time.Millisecond
	env.svc.hibernateIdle(t.Context())
	*clock = clock.Add(userIdleAfter)
	env.svc.hibernateIdle(t.Context())
	waitState(t, env.fake, next, machines.StateHibernating)
	if got := env.fake.state(hung); got != machines.StateRunning || env.svc.ops.activeFor(hung) != nil {
		t.Errorf("the box whose agent never answered: state %v, operation %v; want it running with none", got, env.svc.ops.activeFor(hung))
	}
}

func TestAClientComingBackDuringTheSaveKeepsTheBoxUp(t *testing.T) {
	env, clock := newUserIdleEnv(t)
	env.svc.hibernateIdle(t.Context())
	*clock = clock.Add(userIdleAfter)
	env.agent.onUpload = func(machines.Endpoint) { env.svc.Reached(aliceBox)() }
	env.svc.hibernateIdle(t.Context())
	op := newestOperation(env.svc, aliceBox)
	if op == nil {
		t.Fatal("no save started for the idle box")
	}
	events := watch(t, env.svc, env.ctx, op.id, "")
	if last := events[len(events)-1]; last.GetPhase() != controlv1.Phase_PHASE_DONE || env.fake.state(aliceBox) != machines.StateRunning {
		t.Errorf("after a call during the save: %v, state %v", last, env.fake.state(aliceBox))
	}
}

func TestAnIdleBoxWhoseSaveFailedStillHibernates(t *testing.T) {
	env, clock := newUserIdleEnv(t)
	env.svc.state.retryDelay = time.Millisecond
	env.store.failNext(statePath{user: alice, box: aliceBox}.head(), 10)
	env.svc.hibernateIdle(t.Context())
	*clock = clock.Add(userIdleAfter)
	env.svc.hibernateIdle(t.Context())
	op := newestOperation(env.svc, aliceBox)
	if op == nil {
		t.Fatal("no save started for the idle box")
	}
	events := watch(t, env.svc, env.ctx, op.id, "")
	if last := events[len(events)-1]; last.GetPhase() != controlv1.Phase_PHASE_DONE || last.GetDetail() != "save failed; hibernating it" {
		t.Errorf("the operation ended with %v", last)
	}
	waitState(t, env.fake, aliceBox, machines.StateHibernating)
}

func TestAnIdleHibernatedUserBoxWakesOnEnsure(t *testing.T) {
	env, clock := newUserIdleEnv(t)
	env.svc.hibernateIdle(t.Context())
	*clock = clock.Add(userIdleAfter)
	env.svc.hibernateIdle(t.Context())
	waitState(t, env.fake, aliceBox, machines.StateHibernating)
	env.fake.setState(aliceBox, machines.StateHibernated)
	if _, err := env.svc.Ensure(env.ctx, &controlv1.EnsureRequest{BoxId: aliceBox}); err != nil {
		t.Fatalf("Ensure: %v", err)
	}
	if got := env.fake.state(aliceBox); got != machines.StateStarting {
		t.Errorf("state after Ensure = %v, want it waking", got)
	}
}

func newUserIdleEnv(t *testing.T) (*stateEnv, *time.Time) {
	t.Helper()
	env := newStateEnv(t)
	env.svc.EnableIdle(0, userIdleAfter)
	env.svc.EnableActivity(&fakeActivity{})
	clock := time.Unix(1_000_000, 0)
	env.svc.idle.now = func() time.Time { return clock }
	env.ctx = auth.WithUser(t.Context(), alice)
	return env, &clock
}

// newestOperation returns the last operation begun on box, finished or not, since a fast one ends before a test looks.
func newestOperation(svc *Service, box string) *operation {
	svc.ops.mu.Lock()
	defer svc.ops.mu.Unlock()
	return svc.ops.active[box]
}

func waitState(t *testing.T, fake *fakeMachines, id string, want machines.State) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for fake.state(id) != want {
		if time.Now().After(deadline) {
			t.Fatalf("state = %v, want %v", fake.state(id), want)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

type fakeActivity struct {
	busy bool
	err  error
	hang string
}

func (a *fakeActivity) Busy(ctx context.Context, box string, _ machines.Endpoint) (bool, error) {
	if box == a.hang {
		<-ctx.Done()
		return false, ctx.Err()
	}
	return a.busy, a.err
}
