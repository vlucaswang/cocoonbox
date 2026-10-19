package boxes

import (
	"cmp"
	"context"
	"errors"
	"fmt"

	"github.com/projecteru2/core/log"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	agentv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/agent/v1"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/machines"
	"github.com/loomhand/cocoonbox/api/users"
)

func (s *Service) Create(ctx context.Context, req *controlv1.CreateBoxRequest) (*controlv1.EnsureResponse, error) {
	user, err := auth.Caller(ctx)
	if err != nil {
		return nil, err
	}
	flavor, err := s.flavor(req.GetFlavor())
	if err != nil {
		return nil, err
	}
	image, ok := s.defaults.offered(cmp.Or(req.GetImage(), s.ownImage(user)))
	if !ok {
		return nil, status.Errorf(codes.InvalidArgument, "image %q is not offered; choose %s", req.GetImage(), s.defaults.choices())
	}
	unlock, err := s.lockStarts(ctx, user)
	if err != nil {
		return nil, err
	}
	defer unlock()
	logger := log.WithFunc("boxes.Service.Create").WithField("user", user)
	current, err := s.currentMachines(ctx, user)
	if err != nil {
		return nil, serviceError(ctx, logger, err, "list boxes")
	}
	record := AgentRecord(ctx)
	if record == nil {
		if err = s.checkRunning(user, "", current); err != nil {
			return nil, err
		}
	}
	b, err := s.registry.AddBox(ctx, users.Box{ID: newBoxID(), Owner: user, Name: req.GetName(), Agent: record != nil})
	if err != nil {
		return nil, registryError(ctx, err, "add box")
	}
	if record != nil {
		if err = record(ctx, b.ID); err != nil {
			logger.Errorf(ctx, err, "record the new box %s", b.ID)
			if rmErr := s.registry.RemoveBox(context.WithoutCancel(ctx), user, b.ID); rmErr != nil {
				logger.Errorf(ctx, rmErr, "remove the unrecorded box %s", b.ID)
			}
			return nil, status.Error(codes.Unavailable, "recording the new box failed")
		}
	}
	s.ops.watchers.wake(user)
	tenant, err := s.tenantOf(user)
	if err != nil {
		return nil, serviceError(ctx, logger, err, "read tenant")
	}
	spec := machines.Spec{ID: b.ID, Owner: user, Tenant: tenant, Flavor: flavor}
	if image != s.defaults.Image {
		spec.Image = image
	}
	m, _, err := s.machines.Ensure(ctx, spec)
	if err != nil {
		return nil, serviceError(ctx, logger, err, "create box")
	}
	logger.Infof(ctx, "created box %s (%s) from %s", b.Name, b.ID, image)
	return s.withOperation(ctx, user, b.ID, m, controlv1.Phase_PHASE_CREATING), nil
}

func (s *Service) Rename(ctx context.Context, req *controlv1.RenameBoxRequest) (*controlv1.Box, error) {
	user, err := s.ownerOf(ctx, req.GetBoxId())
	if err != nil {
		return nil, err
	}
	if _, err := s.registry.RenameBox(ctx, user, req.GetBoxId(), req.GetName()); err != nil {
		return nil, registryError(ctx, err, "rename box")
	}
	s.ops.watchers.wake(user)
	return s.Get(ctx, &controlv1.GetBoxRequest{BoxId: req.GetBoxId()})
}

// Delete saves a running box quiesced, then removes every machine of the box and the box; the GC removes its saved state after the retention of deleted boxes.
func (s *Service) Delete(ctx context.Context, ref *controlv1.BoxRef) (*controlv1.Operation, error) {
	box := ref.GetBoxId()
	user, err := s.ownerOf(ctx, box)
	if err != nil {
		return nil, err
	}
	unlock, err := s.lockStarts(ctx, user)
	if err != nil {
		return nil, err
	}
	defer unlock()
	if active := s.ops.activeFor(box); active != nil {
		return nil, status.Errorf(codes.FailedPrecondition, "operation %s is still running", active.id)
	}
	logger := log.WithFunc("boxes.Service.Delete").WithField("box", box)
	cur, err := s.current(ctx, user, box)
	if err != nil && !errors.Is(err, machines.ErrNotFound) {
		return nil, serviceError(ctx, logger, err, "get box")
	}
	save := err == nil && s.state != nil && s.ready(cur)
	op, started := s.ops.start(context.WithoutCancel(ctx), user, box, controlv1.Phase_PHASE_DELETING, cur.Revision, stateTimeout)
	if started {
		go s.runDelete(op, user, cur, save)
	}
	logger.Infof(ctx, "deleting box as operation %s, saving first: %t", op.id, save)
	return &controlv1.Operation{Id: op.id, BoxId: box}, nil
}

// runDelete saves m first when save is set, then deletes the box's machines and the box; a failed save keeps the box.
func (s *Service) runDelete(op *operation, user string, m machines.Machine, save bool) {
	logger := log.WithFunc("boxes.Service.runDelete").WithField("box", op.boxID)
	if save {
		rec, err := s.save(op, user, m, agentv1.Consistency_CONSISTENCY_QUIESCED)
		if err != nil {
			logger.Error(op.ctx, err, "save before delete")
			op.fail("save failed; the box is kept")
			return
		}
		op.note(controlv1.Phase_PHASE_SAVING, controlv1.BoxState_BOX_STATE_RUNNING, "saved snapshot "+rec.ID)
	}
	ms, err := s.machines.List(op.ctx, user)
	if err != nil {
		logger.Error(op.ctx, err, "list machines")
		op.fail("removing the machine failed")
		return
	}
	for _, mm := range ms {
		if boxOfMachine(mm.ID) != op.boxID {
			continue
		}
		op.note(controlv1.Phase_PHASE_DELETING, boxStates[mm.State], "removing machine "+mm.ID)
		if err := s.deleteMachine(op.ctx, mm.ID); err != nil && !errors.Is(err, machines.ErrNotFound) {
			logger.Error(op.ctx, err, "delete machine "+mm.ID)
			op.fail("removing the machine failed")
			return
		}
	}
	if err := s.registry.RemoveBox(op.ctx, user, op.boxID); err != nil {
		logger.Error(op.ctx, err, "remove box")
		op.fail("the machine is gone, but removing the box failed")
		return
	}
	logger.Infof(op.ctx, "deleted box of user %s", user)
	op.succeed(fmt.Sprintf("deleted; the saved state of %s is removed after the retention of deleted boxes", op.boxID))
}

// deleting reports whether box is being deleted.
func (s *Service) deleting(box string) bool {
	active := s.ops.activeFor(box)
	return active != nil && active.phase == controlv1.Phase_PHASE_DELETING
}

func registryError(ctx context.Context, err error, action string) error {
	switch {
	case errors.Is(err, users.ErrInvalid):
		return status.Error(codes.InvalidArgument, err.Error())
	case errors.Is(err, users.ErrExists):
		return status.Error(codes.AlreadyExists, err.Error())
	case errors.Is(err, users.ErrQuota):
		return status.Error(codes.FailedPrecondition, err.Error())
	case errors.Is(err, users.ErrNotFound):
		return ErrNoBox
	default:
		log.WithFunc("boxes.registryError").Error(ctx, err, action)
		return status.Errorf(codes.Unavailable, "%s failed", action)
	}
}
