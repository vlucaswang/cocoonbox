package boxes

import (
	"cmp"
	"context"
	"fmt"
	"slices"
	"time"

	"github.com/projecteru2/core/log"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	agentv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/agent/v1"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
)

func (s *Service) Rebase(ctx context.Context, req *controlv1.RebaseRequest) (*controlv1.Operation, error) {
	box := req.GetBoxId()
	user, err := s.ownerOf(ctx, box)
	if err != nil {
		return nil, err
	}
	unlock, err := s.lockShared(ctx, user)
	if err != nil {
		return nil, err
	}
	defer unlock()
	m, err := s.runningMachine(ctx, box, user)
	if err != nil {
		return nil, err
	}
	image := cmp.Or(req.GetImage(), s.defaults.upgrade(s.ownImage(user), m.Image))
	if image != s.defaults.Image && !slices.Contains(s.defaults.RebaseImages, image) {
		return nil, status.Errorf(codes.InvalidArgument, "image %q is not a rebase target; choose %s", image, s.defaults.choices())
	}
	if familyOf(image) != familyOf(m.Image) && !auth.Operator(ctx) {
		return nil, status.Errorf(codes.FailedPrecondition, "image %q is not of the box's family %s; only an operator moves a box to another family", image, familyOf(m.Image))
	}
	op, started := s.ops.start(context.WithoutCancel(ctx), user, box, controlv1.Phase_PHASE_SAVING, m.Revision, stateTimeout)
	if started {
		go s.runRebase(op, user, m, image)
	}
	log.WithFunc("boxes.Service.Rebase").WithField("box", box).Infof(ctx, "rebasing from %s (%s) to %s as operation %s", m.ID, m.Image, image, op.id)
	return &controlv1.Operation{Id: op.id, BoxId: box}, nil
}

// runRebase saves quiesced, starts a machine from image, restores into it, then deletes the old machine.
func (s *Service) runRebase(op *operation, user string, old machines.Machine, image string) {
	logger := log.WithFunc("boxes.Service.runRebase").WithField("box", op.boxID)
	start := time.Now()
	rec, err := s.save(op, user, old, agentv1.Consistency_CONSISTENCY_QUIESCED)
	if err != nil {
		logger.Error(op.ctx, err, "save before rebase")
		op.fail("save failed")
		return
	}
	unpin, err := s.state.pin(op.ctx, statePath{user: user, box: op.boxID}, rec.ID)
	defer unpin()
	if err != nil {
		logger.Error(op.ctx, err, "pin the saved snapshot")
		op.fail("pin the saved snapshot failed")
		return
	}
	saved := time.Since(start)
	op.note(controlv1.Phase_PHASE_SAVING, controlv1.BoxState_BOX_STATE_RUNNING,
		fmt.Sprintf("saved snapshot %s in %s", rec.ID, saved.Round(time.Second)))

	tenant, err := s.tenantOf(user)
	if err != nil {
		logger.Error(op.ctx, err, "read tenant for rebase machine")
		op.fail("create machine failed")
		return
	}
	spec := machines.Spec{ID: newMachineID(op.boxID), Owner: user, Tenant: tenant, Flavor: old.Flavor, Image: image}
	created, _, err := s.machines.Ensure(op.ctx, spec)
	if err != nil {
		logger.Error(op.ctx, err, "create rebase machine")
		op.fail("create machine failed")
		return
	}
	op.note(controlv1.Phase_PHASE_CREATING, controlv1.BoxState_BOX_STATE_STARTING, fmt.Sprintf("machine %s from %s", created.ID, image))
	running, err := s.waitRunning(op, created)
	if err == nil && s.provision != nil {
		if err = s.provisionMachine(op.ctx, running); err == nil {
			running, err = s.machines.Get(op.ctx, created.ID)
		}
	}
	if err != nil {
		logger.Error(op.ctx, err, "start rebase machine")
		s.discard(op, created.ID)
		op.fail(startFailure(err))
		return
	}
	booted := time.Since(start)

	ep, err := s.machines.Address(op.ctx, created.ID)
	if err == nil {
		_, err = s.state.restore(s.agentContext(op.ctx, running), statePath{user: user, box: op.boxID}, ep, rec.ID, progressNotes(op, controlv1.Phase_PHASE_RESTORING, "restoring"))
	}
	if err != nil {
		logger.Error(op.ctx, err, "restore into rebase machine")
		s.discard(op, created.ID)
		op.fail("restore failed")
		return
	}
	restored := time.Since(start)

	unlock, err := s.lockShared(op.ctx, user)
	if err != nil {
		logger.Error(op.ctx, err, "lock the starts before removing the old machine")
		s.discard(op, created.ID)
		op.fail("rebase failed; the old machine stays")
		return
	}
	defer unlock()
	if s.ops.activeFor(op.boxID) != op || !s.ops.claim(op.ctx, op) {
		logger.Infof(op.ctx, "another operation took %s over; the old machine stays", op.boxID)
		s.discard(op, created.ID)
		op.fail("superseded; the old machine stays")
		return
	}
	if err := s.deleteMachine(op.ctx, old.ID); err != nil {
		logger.Error(op.ctx, err, "delete old machine")
		op.fail("state restored into the new machine, but removing the old machine failed")
		return
	}
	logger.Infof(op.ctx, "rebased %s to %s (%s): saved %s, booted %s, restored %s", old.ID, created.ID, image,
		saved.Round(time.Second), booted.Round(time.Second), restored.Round(time.Second))
	op.succeed(fmt.Sprintf("now on %s (%s); snapshot %s, %.1f MiB logical; save %s, boot %s, restore %s",
		created.ID, image, rec.ID, float64(rec.LogicalBytes)/mib, saved.Round(time.Second),
		(booted - saved).Round(time.Second), (restored - booted).Round(time.Second)))
}

// waitRunning follows m until it runs, fails, or op ends, and returns it running.
func (s *Service) waitRunning(op *operation, m machines.Machine) (machines.Machine, error) {
	for cur := range s.machines.Watch(op.ctx, m.ID, m.Revision) {
		switch cur.State {
		case machines.StateRunning:
			return cur, nil
		case machines.StateFailed:
			if atCapacity(cur) {
				return machines.Machine{}, fmt.Errorf("machine %s: %w", m.ID, machines.ErrCapacity)
			}
			return machines.Machine{}, fmt.Errorf("machine %s failed: %s", m.ID, cur.Reason)
		case machines.StateGone:
			return machines.Machine{}, fmt.Errorf("machine %s was deleted", m.ID)
		}
		op.note(controlv1.Phase_PHASE_CREATING, boxStates[cur.State], stateWords[cur.State])
	}
	return machines.Machine{}, fmt.Errorf("machine %s did not start: %w", m.ID, op.ctx.Err())
}

// discard deletes a rebase machine that will not serve; the old machine keeps serving.
func (s *Service) discard(op *operation, id string) {
	if err := s.deleteMachine(context.WithoutCancel(op.ctx), id); err != nil {
		log.WithFunc("boxes.Service.discard").WithField("box", op.boxID).Error(op.ctx, err, "delete "+id)
	}
}
