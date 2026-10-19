package controlmcp

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"

	"github.com/projecteru2/core/log"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/boxes"
	controlv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/control/v1"
	"github.com/loomhand/cocoonbox/api/mcpwire"
	"github.com/loomhand/cocoonbox/api/oauth"
)

const (
	stateAbsent       = "absent"
	boxProperty       = `{"type":"string","description":"The box's name or id; without it, your first box."}`
	usagePeriodSchema = `{"type":"object","properties":{"since_ms":{"type":"integer"},"requests":{"type":"integer"},"prompt_tokens":{"type":"integer"},"completion_tokens":{"type":"integer"},"total_tokens":{"type":"integer"},"cost_micros":{"type":"integer"},"models":{"type":"array","items":{"type":"object","properties":{"model":{"type":"string"},"requests":{"type":"integer"},"prompt_tokens":{"type":"integer"},"completion_tokens":{"type":"integer"},"total_tokens":{"type":"integer"},"cost_micros":{"type":"integer"}},"required":["model","requests","prompt_tokens","completion_tokens","total_tokens","cost_micros"]}}},"required":["since_ms","requests","prompt_tokens","completion_tokens","total_tokens","cost_micros","models"]}`

	errUpgradeUnconfirmed toolError = "Upgrading restarts running programs; files, logins and agent history are kept. Call again with confirm: true."
	errUpgradeCurrent     toolError = "Already on the latest image."
	errDeleteUnconfirmed  toolError = "Deleting removes the box and stops its running programs; its last saved state is kept for 7 days, then removed. Call again with confirm: true."
	errDeleteUnnamed      toolError = "Name the box to delete in box."
	errNoGrantBox         toolError = "This client has created no box yet; box_create creates one."
)

var toolSpecs = []toolSpec{
	{
		Name:         "box_list",
		Description:  "List your cocoonbox boxes with their names, states and images.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{},"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"boxes":{"type":"array","items":{"type":"object","properties":{"box_id":{"type":"string"},"name":{"type":"string"},"state":{"type":"string"},"image":{"type":"string"},"last_saved_at_ms":{"type":"integer"}},"required":["box_id","name","state","image","last_saved_at_ms"]}}},"required":["boxes"]}`),
	},
	{
		Name:         "box_status",
		Description:  "Show the state of your cocoonbox box: running or hibernated, its image, whether an upgrade is available, and when your files were last saved.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{"box":` + boxProperty + `},"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"box_id":{"type":"string"},"name":{"type":"string"},"state":{"type":"string"},"image":{"type":"string"},"latest_image":{"type":"string"},"upgrade_available":{"type":"boolean"},"last_saved_at_ms":{"type":"integer"}},"required":["box_id","name","state","image","latest_image","upgrade_available","last_saved_at_ms"]}`),
	},
	{
		Name:         "box_ensure",
		Description:  "Start or wake your cocoonbox box and wait until it is running. Waking takes a few seconds; creating a new box takes longer.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{"box":` + boxProperty + `,"wait":{"type":"boolean","default":true}},"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"box_id":{"type":"string"},"state":{"type":"string"},"operation_id":{"type":"string"}},"required":["box_id","state","operation_id"]}`),
	},
	{
		Name:         "box_create",
		Description:  "Create another cocoonbox box with this name and wait until it is running. Your other boxes stay as they are; only a limited number may run at once.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{"name":{"type":"string"},"image":{"type":"string","description":"An image family, such as cbdroid for an Android phone, or an image; without it, your usual desktop."},"wait":{"type":"boolean","default":true}},"required":["name"],"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"box_id":{"type":"string"},"name":{"type":"string"},"state":{"type":"string"},"operation_id":{"type":"string"}},"required":["box_id","name","state","operation_id"]}`),
	},
	{
		Name:         "box_hibernate",
		Description:  "Hibernate your cocoonbox box. Running programs and memory are kept and come back on the next wake.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{"box":` + boxProperty + `},"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"box_id":{"type":"string"},"state":{"type":"string"},"operation_id":{"type":"string"}},"required":["box_id","state","operation_id"]}`),
	},
	{
		Name:         "box_save",
		Description:  "Save your files, logins and agent history from the box to storage now. On a desktop box the browser closes during the save.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{"box":` + boxProperty + `},"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"box_id":{"type":"string"},"last_saved_at_ms":{"type":"integer"},"operation_id":{"type":"string"},"bytes":{"type":"integer"}},"required":["box_id","last_saved_at_ms","operation_id","bytes"]}`),
	},
	{
		Name:         "box_upgrade",
		Description:  "Move your box to the latest image of its kind. Running programs restart; files, logins and agent history are kept. Requires confirm: true.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{"box":` + boxProperty + `,"confirm":{"type":"boolean"}},"required":["confirm"],"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"box_id":{"type":"string"},"state":{"type":"string"},"image":{"type":"string"},"operation_id":{"type":"string"}},"required":["box_id","state","image","operation_id"]}`),
	},
	{
		Name:         "box_delete",
		Description:  "Delete one of your cocoonbox boxes. A running box is saved first; its saved state is kept for 7 days, then removed. Requires the box and confirm: true.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{"box":` + boxProperty + `,"confirm":{"type":"boolean"}},"required":["box","confirm"],"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"box_id":{"type":"string"},"operation_id":{"type":"string"}},"required":["box_id","operation_id"]}`),
	},
	{
		Name:         "usage",
		Description:  "Show the model usage gw metered for your account, all your boxes together: requests, tokens and cost in micro-dollars today and this month (UTC), by model, with your daily cost cap (-1 unlimited) and what is left of it today. Your own model client's tokens are not in it.",
		InputSchema:  json.RawMessage(`{"type":"object","properties":{},"additionalProperties":false}`),
		OutputSchema: json.RawMessage(`{"type":"object","properties":{"today":` + usagePeriodSchema + `,"month":` + usagePeriodSchema + `,"daily_cap_micros":{"type":"integer"},"default_cap":{"type":"boolean"},"remaining_today_micros":{"type":"integer"}},"required":["today","month","daily_cap_micros","default_cap","remaining_today_micros"]}`),
	},
}

type (
	progressFunc func(message string)
	toolFunc     func() (any, error)
)

// toolError is a refusal the model reads as an isError result.
type toolError string

func (e toolError) Error() string { return string(e) }

type toolSpec struct {
	Name         string          `json:"name"`
	Description  string          `json:"description"`
	InputSchema  json.RawMessage `json:"inputSchema"`
	OutputSchema json.RawMessage `json:"outputSchema"`
}

type toolResult struct {
	Content           []textContent `json:"content"`
	StructuredContent any           `json:"structuredContent,omitempty"`
	IsError           bool          `json:"isError"`
}

type textContent struct {
	Type string `json:"type"`
	Text string `json:"text"`
}

type statusResult struct {
	BoxID            string `json:"box_id"`
	Name             string `json:"name"`
	State            string `json:"state"`
	Image            string `json:"image"`
	LatestImage      string `json:"latest_image"`
	UpgradeAvailable bool   `json:"upgrade_available"`
	LastSavedAtMs    int64  `json:"last_saved_at_ms"`
}

type boxArgs struct {
	Box string `json:"box"`
}

type confirmArgs struct {
	Box     string `json:"box"`
	Confirm bool   `json:"confirm"`
}

type listResult struct {
	Boxes []listEntry `json:"boxes"`
}

type listEntry struct {
	BoxID         string `json:"box_id"`
	Name          string `json:"name"`
	State         string `json:"state"`
	Image         string `json:"image"`
	LastSavedAtMs int64  `json:"last_saved_at_ms"`
}

type createResult struct {
	BoxID       string `json:"box_id"`
	Name        string `json:"name"`
	State       string `json:"state"`
	OperationID string `json:"operation_id"`
}

type deleteResult struct {
	BoxID       string `json:"box_id"`
	OperationID string `json:"operation_id"`
}

type operationResult struct {
	BoxID       string `json:"box_id"`
	State       string `json:"state"`
	OperationID string `json:"operation_id"`
}

type saveResult struct {
	BoxID         string `json:"box_id"`
	LastSavedAtMs int64  `json:"last_saved_at_ms"`
	OperationID   string `json:"operation_id"`
	Bytes         uint64 `json:"bytes"`
}

type usageResult struct {
	Today                usagePeriod `json:"today"`
	Month                usagePeriod `json:"month"`
	DailyCapMicros       int64       `json:"daily_cap_micros"`
	DefaultCap           bool        `json:"default_cap"`
	RemainingTodayMicros int64       `json:"remaining_today_micros"`
}

type usagePeriod struct {
	SinceMs          int64        `json:"since_ms"`
	Requests         int64        `json:"requests"`
	PromptTokens     int64        `json:"prompt_tokens"`
	CompletionTokens int64        `json:"completion_tokens"`
	TotalTokens      int64        `json:"total_tokens"`
	CostMicros       int64        `json:"cost_micros"`
	Models           []modelUsage `json:"models"`
}

type modelUsage struct {
	Model            string `json:"model"`
	Requests         int64  `json:"requests"`
	PromptTokens     int64  `json:"prompt_tokens"`
	CompletionTokens int64  `json:"completion_tokens"`
	TotalTokens      int64  `json:"total_tokens"`
	CostMicros       int64  `json:"cost_micros"`
}

type upgradeResult struct {
	BoxID       string `json:"box_id"`
	State       string `json:"state"`
	Image       string `json:"image"`
	OperationID string `json:"operation_id"`
}

// call runs one tool as the user in ctx; progress receives each event of the operation it waits on.
func (s *Server) call(ctx context.Context, call *callParams, progress progressFunc) (*toolResult, *mcpwire.Error) {
	switch call.Name {
	case "box_list":
		return withArguments(call.Arguments, &struct{}{}, func() (any, error) { return s.list(ctx) })
	case "box_status":
		var args boxArgs
		return withArguments(call.Arguments, &args, func() (any, error) { return s.status(ctx, args.Box) })
	case "box_ensure":
		var args struct {
			Box  string `json:"box"`
			Wait *bool  `json:"wait"`
		}
		return withArguments(call.Arguments, &args, func() (any, error) {
			return s.ensure(ctx, args.Box, args.Wait == nil || *args.Wait, progress)
		})
	case "box_create":
		var args struct {
			Name  string `json:"name"`
			Image string `json:"image"`
			Wait  *bool  `json:"wait"`
		}
		return withArguments(call.Arguments, &args, func() (any, error) {
			return s.create(ctx, &controlv1.CreateBoxRequest{Name: args.Name, Image: args.Image}, args.Wait == nil || *args.Wait, progress)
		})
	case "box_hibernate":
		var args boxArgs
		return withArguments(call.Arguments, &args, func() (any, error) { return s.hibernate(ctx, args.Box, progress) })
	case "box_save":
		var args boxArgs
		return withArguments(call.Arguments, &args, func() (any, error) { return s.save(ctx, args.Box, progress) })
	case "box_upgrade":
		var args confirmArgs
		return withArguments(call.Arguments, &args, func() (any, error) {
			if !args.Confirm {
				return nil, errUpgradeUnconfirmed
			}
			return s.upgrade(ctx, args.Box, progress)
		})
	case "box_delete":
		var args confirmArgs
		return withArguments(call.Arguments, &args, func() (any, error) {
			if !args.Confirm {
				return nil, errDeleteUnconfirmed
			}
			return s.delete(ctx, args.Box, progress)
		})
	case "usage":
		return withArguments(call.Arguments, &struct{}{}, func() (any, error) { return s.usage(ctx) })
	default:
		return nil, &mcpwire.Error{Code: mcpwire.CodeInvalidParams, Message: "unknown tool: " + call.Name}
	}
}

func (s *Server) list(ctx context.Context) (*listResult, error) {
	list, err := s.visible(ctx)
	if err != nil {
		return nil, err
	}
	out := &listResult{Boxes: []listEntry{}}
	for _, b := range list {
		out.Boxes = append(out.Boxes, listEntry{BoxID: b.GetId(), Name: b.GetName(), State: stateName(b.GetState()), Image: b.GetImage(), LastSavedAtMs: b.GetLastSavedAtMs()})
	}
	return out, nil
}

func (s *Server) status(ctx context.Context, ref string) (*statusResult, error) {
	box, err := s.box(ctx, ref)
	if err != nil || box == nil {
		return &statusResult{State: stateAbsent}, err
	}
	return &statusResult{
		BoxID:            box.GetId(),
		Name:             box.GetName(),
		State:            stateName(box.GetState()),
		Image:            box.GetImage(),
		LatestImage:      box.GetLatestImage(),
		UpgradeAvailable: box.GetImage() != "" && box.GetLatestImage() != "" && box.GetImage() != box.GetLatestImage(),
		LastSavedAtMs:    box.GetLastSavedAtMs(),
	}, nil
}

func (s *Server) ensure(ctx context.Context, ref string, wait bool, progress progressFunc) (*operationResult, error) {
	resp, err := s.startEnsure(ctx, ref)
	if err != nil {
		return nil, err
	}
	res := &operationResult{BoxID: resp.GetBox().GetId(), State: stateName(resp.GetBox().GetState()), OperationID: resp.GetOperation().GetId()}
	if res.OperationID == "" || !wait {
		return res, nil
	}
	if _, err = s.finish(ctx, "Ensure", res.OperationID, progress); err != nil {
		return nil, err
	}
	box, err := s.box(ctx, res.BoxID)
	res.State = stateName(box.GetState())
	return res, err
}

func (s *Server) create(ctx context.Context, req *controlv1.CreateBoxRequest, wait bool, progress progressFunc) (*createResult, error) {
	resp, err := s.startCreate(ctx, req)
	if err != nil {
		return nil, err
	}
	res := &createResult{BoxID: resp.GetBox().GetId(), Name: resp.GetBox().GetName(), State: stateName(resp.GetBox().GetState()), OperationID: resp.GetOperation().GetId()}
	if res.OperationID == "" || !wait {
		return res, nil
	}
	if _, err = s.finish(ctx, "Create", res.OperationID, progress); err != nil {
		return nil, err
	}
	box, err := s.box(ctx, res.BoxID)
	res.State = stateName(box.GetState())
	return res, err
}

func (s *Server) hibernate(ctx context.Context, ref string, progress progressFunc) (*operationResult, error) {
	box, err := s.runningBox(ctx, ref)
	if err != nil {
		return nil, err
	}
	op, err := s.boxes.Hibernate(ctx, &controlv1.BoxRef{BoxId: box.GetId()})
	if err != nil {
		return nil, err
	}
	if _, err = s.finish(ctx, "Hibernate", op.GetId(), progress); err != nil {
		return nil, err
	}
	box, err = s.box(ctx, op.GetBoxId())
	return &operationResult{BoxID: op.GetBoxId(), State: stateName(box.GetState()), OperationID: op.GetId()}, err
}

func (s *Server) save(ctx context.Context, ref string, progress progressFunc) (*saveResult, error) {
	box, err := s.runningBox(ctx, ref)
	if err != nil {
		return nil, err
	}
	op, err := s.boxes.SaveState(ctx, &controlv1.BoxRef{BoxId: box.GetId()})
	if err != nil {
		return nil, err
	}
	w, err := s.finish(ctx, "Save", op.GetId(), progress)
	if err != nil {
		return nil, err
	}
	box, err = s.box(ctx, op.GetBoxId())
	return &saveResult{BoxID: op.GetBoxId(), LastSavedAtMs: box.GetLastSavedAtMs(), OperationID: op.GetId(), Bytes: w.savedBytes}, err
}

func (s *Server) upgrade(ctx context.Context, ref string, progress progressFunc) (*upgradeResult, error) {
	box, err := s.runningBox(ctx, ref)
	if err != nil {
		return nil, err
	}
	if box.GetImage() == box.GetLatestImage() {
		return nil, errUpgradeCurrent
	}
	op, err := s.boxes.Rebase(ctx, &controlv1.RebaseRequest{BoxId: box.GetId()})
	if err != nil {
		return nil, err
	}
	if _, err = s.finish(ctx, "Upgrade", op.GetId(), progress); err != nil {
		return nil, err
	}
	box, err = s.box(ctx, op.GetBoxId())
	return &upgradeResult{BoxID: op.GetBoxId(), State: stateName(box.GetState()), Image: box.GetImage(), OperationID: op.GetId()}, err
}

func (s *Server) delete(ctx context.Context, ref string, progress progressFunc) (*deleteResult, error) {
	if ref == "" {
		return nil, errDeleteUnnamed
	}
	box, err := s.box(ctx, ref)
	if err != nil {
		return nil, err
	}
	op, err := s.boxes.Delete(ctx, &controlv1.BoxRef{BoxId: box.GetId()})
	if err != nil {
		return nil, err
	}
	if _, err = s.finish(ctx, "Delete", op.GetId(), progress); err != nil {
		return nil, err
	}
	return &deleteResult{BoxID: op.GetBoxId(), OperationID: op.GetId()}, nil
}

func (s *Server) usage(ctx context.Context) (*usageResult, error) {
	resp, err := s.boxes.Usage(ctx, &controlv1.UsageRequest{})
	if err != nil {
		return nil, err
	}
	return &usageResult{
		Today: toUsagePeriod(resp.GetToday()), Month: toUsagePeriod(resp.GetMonth()),
		DailyCapMicros: resp.GetDailyCapMicros(), DefaultCap: resp.GetDefaultCap(), RemainingTodayMicros: resp.GetRemainingTodayMicros(),
	}, nil
}

// finish follows operation id to its end; a failed operation becomes "<verb> failed: <detail>".
func (s *Server) finish(ctx context.Context, verb, id string, progress progressFunc) (*watchStream, error) {
	w := &watchStream{ctx: ctx, progress: progress}
	if err := s.boxes.WatchOperation(&controlv1.WatchOperationRequest{OperationId: id}, w); err != nil {
		return nil, err
	}
	if w.last.GetPhase() == controlv1.Phase_PHASE_FAILED {
		return nil, toolError(verb + " failed: " + w.last.GetDetail())
	}
	return w, nil
}

func (s *Server) runningBox(ctx context.Context, ref string) (*controlv1.Box, error) {
	box, err := s.box(ctx, ref)
	switch {
	case err != nil:
		return nil, err
	case box == nil:
		return nil, toolError("The box is " + stateAbsent + ".")
	case box.GetState() != controlv1.BoxState_BOX_STATE_RUNNING:
		return nil, toolError("The box is " + stateName(box.GetState()) + ".")
	}
	return box, nil
}

// box returns the caller's box named or numbered ref, or the first one when ref is empty, nil when the caller has none.
func (s *Server) box(ctx context.Context, ref string) (*controlv1.Box, error) {
	list, err := s.visible(ctx)
	if err != nil {
		return nil, err
	}
	if ref == "" {
		if len(list) == 0 {
			return nil, nil
		}
		return list[0], nil
	}
	for _, b := range list {
		if b.GetId() == ref || b.GetName() == ref {
			return b, nil
		}
	}
	return nil, toolError(fmt.Sprintf("You have no box %q; box_list lists your boxes.", ref))
}

// startEnsure starts or wakes the box ref names; a grant's call names one of its boxes and stays within its running cap.
func (s *Server) startEnsure(ctx context.Context, ref string) (*controlv1.EnsureResponse, error) {
	g := grantOf(ctx)
	if g == nil && ref == "" {
		return s.boxes.Ensure(ctx, &controlv1.EnsureRequest{})
	}
	if g != nil {
		defer s.lock(g.ID)()
	}
	box, err := s.box(ctx, ref)
	switch {
	case err != nil:
		return nil, err
	case box == nil:
		return nil, errNoGrantBox
	}
	if g != nil && !holdsMemory(box) {
		if err = s.checkRunning(ctx, g); err != nil {
			return nil, err
		}
	}
	return s.boxes.Ensure(ctx, &controlv1.EnsureRequest{BoxId: box.GetId()})
}

// startCreate adds a box; a grant's call stays within its caps and records the box as the grant's before its machine starts.
func (s *Server) startCreate(ctx context.Context, req *controlv1.CreateBoxRequest) (*controlv1.EnsureResponse, error) {
	g := grantOf(ctx)
	if g == nil {
		return s.boxes.Create(ctx, req)
	}
	defer s.lock(g.ID)()
	list, err := s.visible(ctx)
	if err != nil {
		return nil, err
	}
	if len(list) >= g.MaxBoxes {
		return nil, toolError(fmt.Sprintf("This client may keep %s at most; delete one of its boxes first.", countBoxes(g.MaxBoxes)))
	}
	if err = s.checkRunning(ctx, g); err != nil {
		return nil, err
	}
	record := func(ctx context.Context, box string) error { return s.grants.AddBox(ctx, g.ID, box) }
	return s.boxes.Create(boxes.WithAgent(ctx, record), req)
}

// checkRunning refuses to start another box of grant g while its running cap is held.
func (s *Server) checkRunning(ctx context.Context, g *oauth.Grant) error {
	list, err := s.visible(ctx)
	if err != nil {
		return err
	}
	if running := len(slices.DeleteFunc(list, func(b *controlv1.Box) bool { return !holdsMemory(b) })); running >= g.MaxRunning {
		return toolError(fmt.Sprintf("This client may run %s at once; hibernate one of its boxes first.", countBoxes(g.MaxRunning)))
	}
	return nil
}

// visible returns the caller's boxes, oldest first: for a grant, only those it created.
func (s *Server) visible(ctx context.Context) ([]*controlv1.Box, error) {
	resp, err := s.boxes.List(ctx, &controlv1.ListBoxesRequest{})
	if err != nil {
		return nil, err
	}
	g := grantOf(ctx)
	if g == nil {
		return resp.GetBoxes(), nil
	}
	ids, err := s.grants.Boxes(ctx, g.ID)
	if err != nil {
		log.WithFunc("controlmcp.Server.visible").Errorf(ctx, err, "list the boxes of grant %s", g.ID)
		return nil, status.Error(codes.Unavailable, "reading the client's boxes failed")
	}
	return slices.DeleteFunc(resp.GetBoxes(), func(b *controlv1.Box) bool { return !slices.Contains(ids, b.GetId()) }), nil
}

// watchStream receives WatchOperation events in-process as the caller in ctx.
type watchStream struct {
	grpc.ServerStream
	ctx      context.Context
	progress progressFunc

	last       *controlv1.OperationEvent
	savedBytes uint64
}

func (w *watchStream) Context() context.Context { return w.ctx }

func (w *watchStream) Send(ev *controlv1.OperationEvent) error {
	w.last = ev
	message := strings.ToLower(strings.TrimPrefix(ev.GetPhase().String(), "PHASE_")) + ": " + ev.GetDetail()
	if ev.GetTotalBytes() != 0 {
		message += fmt.Sprintf(" (%d/%d bytes)", ev.GetDoneBytes(), ev.GetTotalBytes())
		if ev.GetPhase() == controlv1.Phase_PHASE_SAVING {
			w.savedBytes = ev.GetTotalBytes()
		}
	}
	w.progress(message)
	return nil
}

// withArguments decodes raw into dst, refusing names outside the input schema, and runs fn.
func withArguments(raw json.RawMessage, dst any, fn toolFunc) (*toolResult, *mcpwire.Error) {
	if len(raw) != 0 && string(raw) != "null" {
		dec := json.NewDecoder(bytes.NewReader(raw))
		dec.DisallowUnknownFields()
		if err := dec.Decode(dst); err != nil {
			return nil, &mcpwire.Error{Code: mcpwire.CodeInvalidParams, Message: "invalid arguments: " + err.Error()}
		}
	}
	out, err := fn()
	var refusal toolError
	switch {
	case errors.As(err, &refusal):
		return errorText(refusal.Error()), nil
	case internal(err):
		return nil, &mcpwire.Error{Code: mcpwire.CodeInternal, Message: status.Convert(err).Message()}
	case err != nil:
		return errorText(status.Convert(err).Message()), nil
	}
	data, err := marshal(out)
	if err != nil {
		return nil, &mcpwire.Error{Code: mcpwire.CodeInternal, Message: "encode result"}
	}
	return &toolResult{Content: []textContent{{Type: "text", Text: string(bytes.TrimSpace(data))}}, StructuredContent: out}, nil
}

func errorText(text string) *toolResult {
	return &toolResult{Content: []textContent{{Type: "text", Text: text}}, IsError: true}
}

// internal reports a Boxes failure the model cannot act on.
func internal(err error) bool {
	switch status.Code(err) {
	case codes.Unavailable, codes.Internal, codes.Unknown, codes.DataLoss:
		return true
	default:
		return false
	}
}

func toUsagePeriod(p *controlv1.UsagePeriod) usagePeriod {
	out := usagePeriod{
		SinceMs: p.GetSinceMs(), Requests: p.GetRequests(), PromptTokens: p.GetPromptTokens(), CompletionTokens: p.GetCompletionTokens(),
		TotalTokens: p.GetTotalTokens(), CostMicros: p.GetCostMicros(), Models: []modelUsage{},
	}
	for _, m := range p.GetModels() {
		out.Models = append(out.Models, modelUsage{
			Model: m.GetModel(), Requests: m.GetRequests(), PromptTokens: m.GetPromptTokens(), CompletionTokens: m.GetCompletionTokens(),
			TotalTokens: m.GetTotalTokens(), CostMicros: m.GetCostMicros(),
		})
	}
	return out
}

func grantOf(ctx context.Context) *oauth.Grant {
	g, _ := ctx.Value(grantKey{}).(*oauth.Grant)
	return g
}

// holdsMemory reports whether a box counts against a running cap: starting, running or hibernating.
func holdsMemory(b *controlv1.Box) bool {
	switch b.GetState() {
	case controlv1.BoxState_BOX_STATE_STARTING, controlv1.BoxState_BOX_STATE_RUNNING, controlv1.BoxState_BOX_STATE_HIBERNATING:
		return true
	default:
		return false
	}
}

func countBoxes(n int) string {
	if n == 1 {
		return "1 box"
	}
	return fmt.Sprintf("%d boxes", n)
}

func stateName(state controlv1.BoxState) string {
	if state == controlv1.BoxState_BOX_STATE_UNSPECIFIED {
		return stateAbsent
	}
	return strings.ToLower(strings.TrimPrefix(state.String(), "BOX_STATE_"))
}
