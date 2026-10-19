package edge

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"io"
	"net"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	"github.com/loomhand/cocoonbox/api/boxes"
	"github.com/loomhand/cocoonbox/api/credential"
	"github.com/loomhand/cocoonbox/api/dialer"
	agentv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/agent/v1"
	"github.com/loomhand/cocoonbox/api/machines"
)

const aliceToken = "alice-secret"

func TestUnaryRewritesHeaders(t *testing.T) {
	env := newProxyEnv(t)
	var header, trailer metadata.MD
	ctx := metadata.AppendToOutgoingContext(env.aliceCtx(t), HeaderEdgeToken, "forged")
	resp, err := env.client.Hello(ctx, &agentv1.HelloRequest{ClientVersion: "test"}, grpc.Header(&header), grpc.Trailer(&trailer))
	if err != nil {
		t.Fatalf("Hello: %v", err)
	}
	if resp.GetAgentVersion() != "" {
		t.Errorf("agent saw edge token %q, want none", resp.GetAgentVersion())
	}
	if len(resp.GetModels()) != 0 {
		t.Errorf("agent saw authorization %v, want none", resp.GetModels())
	}
	if resp.GetDefaultMainModel() != boxes.BoxID("alice") {
		t.Errorf("agent saw box %q", resp.GetDefaultMainModel())
	}
	if got := header.Get("x-agent-header"); len(got) != 1 || got[0] != "h" {
		t.Errorf("header x-agent-header = %v", got)
	}
	if got := trailer.Get("x-agent-trailer"); len(got) != 1 || got[0] != "t" {
		t.Errorf("trailer x-agent-trailer = %v", got)
	}
}

func TestAClientCallReachesTheBoxAndEndShareDoesNot(t *testing.T) {
	env := newProxyEnv(t)
	_ = env.proxy.EndShare(t.Context(), "alice", boxes.BoxID("alice"), "sh-1")
	if got := env.resolver.reaches(); len(got) != 0 {
		t.Fatalf("EndShare reached %v, want nothing", got)
	}
	if _, err := env.client.Hello(env.aliceCtx(t), &agentv1.HelloRequest{}); err != nil {
		t.Fatalf("Hello: %v", err)
	}
	if got := env.resolver.reaches(); !slices.Equal(got, []string{boxes.BoxID("alice")}) {
		t.Errorf("reached %v after a Hello, want alice's box", got)
	}
}

func TestSessionEventsDoNotKeepTheBoxUp(t *testing.T) {
	env := newProxyEnv(t)
	stream, err := env.client.Events(env.aliceCtx(t), &agentv1.EventsRequest{})
	if err == nil {
		_, _ = stream.Recv()
	}
	if got := env.resolver.reaches(); len(got) != 0 {
		t.Errorf("Session.Events reached %v; it only watches the box", got)
	}
	if _, err := env.client.Hello(env.aliceCtx(t), &agentv1.HelloRequest{}); err != nil {
		t.Fatalf("Hello: %v", err)
	}
	if got := env.resolver.reaches(); !slices.Equal(got, []string{boxes.BoxID("alice")}) {
		t.Errorf("reached %v after a Hello, want alice's box", got)
	}
}

func TestDialsTheEndpointNotItsKey(t *testing.T) {
	env := newProxyEnv(t)
	env.resolver.opaque = true
	if _, err := env.client.Hello(env.aliceCtx(t), &agentv1.HelloRequest{ClientVersion: "test"}); err != nil {
		t.Fatalf("Hello through an endpoint whose key resolves nowhere: %v", err)
	}
}

func TestUnarySendsBoxCredential(t *testing.T) {
	env := newProxyEnv(t)
	env.resolver.wake = "w1"
	ctx := metadata.AppendToOutgoingContext(env.aliceCtx(t), credential.Header, "forged")
	resp, err := env.client.Hello(ctx, &agentv1.HelloRequest{})
	if err != nil {
		t.Fatalf("Hello: %v", err)
	}
	box := boxes.BoxID("alice")
	if want := box + " cred:" + box + ":w1"; resp.GetDefaultMainModel() != want {
		t.Errorf("agent saw %q, want %q", resp.GetDefaultMainModel(), want)
	}
}

func TestStreamPassesFramesThrough(t *testing.T) {
	env := newProxyEnv(t)
	stream, err := env.client.Exec(env.aliceCtx(t))
	if err != nil {
		t.Fatalf("Exec: %v", err)
	}
	if err := stream.Send(execStart("echo")); err != nil {
		t.Fatalf("send start: %v", err)
	}
	for i := range 20 {
		want := "frame-" + strconv.Itoa(i)
		if err := stream.Send(execStdin([]byte(want))); err != nil {
			t.Fatalf("send %d: %v", i, err)
		}
		got, err := stream.Recv()
		if err != nil {
			t.Fatalf("recv %d: %v", i, err)
		}
		if string(got.GetStdout()) != want {
			t.Fatalf("recv %d = %q, want %q", i, got.GetStdout(), want)
		}
	}
	big := make([]byte, 8<<20)
	if err := stream.Send(execStdin(big)); err != nil {
		t.Fatalf("send 8 MiB: %v", err)
	}
	if got, err := stream.Recv(); err != nil || len(got.GetStdout()) != len(big) {
		t.Fatalf("recv 8 MiB: %d bytes, %v", len(got.GetStdout()), err)
	}
	if err := stream.CloseSend(); err != nil {
		t.Fatalf("CloseSend: %v", err)
	}
	if _, err := stream.Recv(); !errors.Is(err, io.EOF) {
		t.Errorf("recv after half-close: %v, want io.EOF", err)
	}
}

func TestScreenRelaysTheBoxRFBServerWithoutTheAgent(t *testing.T) {
	env := newProxyEnv(t)
	stream, err := env.client.Screen(env.aliceCtx(t))
	if err != nil {
		t.Fatalf("Screen: %v", err)
	}
	if err := stream.Send(&agentv1.ScreenClientFrame{Body: &agentv1.ScreenClientFrame_Open{Open: &agentv1.ScreenOpen{}}}); err != nil {
		t.Fatalf("send open: %v", err)
	}
	for i := range 20 {
		want := "frame-" + strconv.Itoa(i)
		if err := stream.Send(screenData([]byte(want))); err != nil {
			t.Fatalf("send %d: %v", i, err)
		}
		if got := recvScreen(t, stream, len(want)); string(got) != want {
			t.Fatalf("recv %d = %q, want %q", i, got, want)
		}
	}
	big := bytes.Repeat([]byte("rfb"), 3<<20)
	if err := stream.Send(screenData(big)); err != nil {
		t.Fatalf("send 9 MiB: %v", err)
	}
	if got := recvScreen(t, stream, len(big)); !bytes.Equal(got, big) {
		t.Fatalf("recv 9 MiB: %d bytes differ", len(got))
	}
	if err := stream.CloseSend(); err != nil {
		t.Fatalf("CloseSend: %v", err)
	}
	if _, err := stream.Recv(); !errors.Is(err, io.EOF) {
		t.Errorf("recv after half-close: %v, want io.EOF", err)
	}
	if n := env.agent.calls.Load(); n != 0 {
		t.Errorf("the agent got %d calls, want none", n)
	}
	if got := env.resolver.reaches(); !slices.Equal(got, []string{boxes.BoxID("alice")}) {
		t.Errorf("reached %v after a Screen, want alice's box", got)
	}
}

func TestScreenNeedsOpenFirst(t *testing.T) {
	env := newProxyEnv(t)
	stream, err := env.client.Screen(env.aliceCtx(t))
	if err != nil {
		t.Fatalf("Screen: %v", err)
	}
	if err := stream.Send(screenData([]byte("x"))); err != nil {
		t.Fatalf("send: %v", err)
	}
	if _, err := stream.Recv(); status.Code(err) != codes.InvalidArgument {
		t.Errorf("recv: %v, want InvalidArgument", err)
	}
	if n := env.screen.accepted.Load(); n != 0 {
		t.Errorf("the screen accepted %d connections, want none", n)
	}
}

func TestScreenCancelClosesTheBoxConnection(t *testing.T) {
	env := newProxyEnv(t)
	ctx, cancel := context.WithCancel(env.aliceCtx(t))
	stream, err := env.client.Screen(ctx)
	if err != nil {
		t.Fatalf("Screen: %v", err)
	}
	if err := stream.Send(&agentv1.ScreenClientFrame{Body: &agentv1.ScreenClientFrame_Open{Open: &agentv1.ScreenOpen{}}}); err != nil {
		t.Fatalf("send open: %v", err)
	}
	if err := stream.Send(screenData([]byte("x"))); err != nil {
		t.Fatalf("send: %v", err)
	}
	recvScreen(t, stream, 1)
	cancel()
	select {
	case <-env.screen.closed:
	case <-time.After(5 * time.Second):
		t.Fatal("the box screen connection outlived the cancelled stream")
	}
}

func TestExecAnswersAfterTheClientHalfCloses(t *testing.T) {
	env := newProxyEnv(t)
	stream, err := env.client.Exec(env.aliceCtx(t))
	if err != nil {
		t.Fatalf("Exec: %v", err)
	}
	frames := []*agentv1.ExecClientFrame{
		{Body: &agentv1.ExecClientFrame_Start{Start: &agentv1.ExecStart{Command: "cat"}}},
		{Body: &agentv1.ExecClientFrame_Stdin{Stdin: []byte("in-")}},
		{Body: &agentv1.ExecClientFrame_Stdin{Stdin: []byte("put")}},
	}
	for _, f := range frames {
		if err := stream.Send(f); err != nil {
			t.Fatalf("send: %v", err)
		}
	}
	if err := stream.CloseSend(); err != nil {
		t.Fatalf("CloseSend: %v", err)
	}
	out, err := stream.Recv()
	if err != nil || string(out.GetStdout()) != "in-put" {
		t.Fatalf("recv stdout = %q, %v; want in-put", out.GetStdout(), err)
	}
	exit, err := stream.Recv()
	if err != nil || exit.GetBody() == nil || exit.GetExitCode() != 7 {
		t.Fatalf("recv exit = %v, %v; want exit_code 7", exit, err)
	}
	if _, err := stream.Recv(); !errors.Is(err, io.EOF) {
		t.Errorf("recv after exit: %v, want io.EOF", err)
	}
}

func TestScreenRelaysPerBoxAreCapped(t *testing.T) {
	env := newProxyEnv(t)
	open := func(ctx context.Context) (agentv1.Session_ScreenClient, error) {
		stream, err := env.client.Screen(ctx)
		if err != nil {
			return nil, err
		}
		if err := stream.Send(&agentv1.ScreenClientFrame{Body: &agentv1.ScreenClientFrame_Open{Open: &agentv1.ScreenOpen{}}}); err != nil {
			return nil, err
		}
		if err := stream.Send(screenData([]byte("x"))); err != nil {
			return nil, err
		}
		_, err = stream.Recv()
		return stream, err
	}
	cancels := make([]context.CancelFunc, 0, maxScreensPerBox)
	for i := range maxScreensPerBox {
		ctx, cancel := context.WithCancel(env.aliceCtx(t))
		t.Cleanup(cancel)
		cancels = append(cancels, cancel)
		if _, err := open(ctx); err != nil {
			t.Fatalf("screen %d: %v", i, err)
		}
	}
	if _, err := open(env.aliceCtx(t)); status.Code(err) != codes.ResourceExhausted {
		t.Fatalf("screen %d: %v, want ResourceExhausted", maxScreensPerBox+1, err)
	}
	grant := auth.Grant{Share: auth.Share{ID: "sh-1", Owner: "alice", Box: boxes.BoxID("alice"), Cut: make(chan struct{})}}
	viewer, _ := net.Pipe()
	if err := env.proxy.Screen(t.Context(), grant, viewer); status.Code(err) != codes.ResourceExhausted {
		t.Errorf("share viewer at the cap: %v, want ResourceExhausted", err)
	}
	if n := env.screen.accepted.Load(); n != maxScreensPerBox {
		t.Errorf("the screen accepted %d connections, want %d", n, maxScreensPerBox)
	}
	cancels[0]()
	deadline := time.Now().Add(2 * time.Second)
	for {
		_, err := open(env.aliceCtx(t))
		if err == nil {
			break
		}
		if status.Code(err) != codes.ResourceExhausted || time.Now().After(deadline) {
			t.Fatalf("screen after one closed: %v", err)
		}
		time.Sleep(20 * time.Millisecond)
	}
}

func TestStreamPropagatesStatus(t *testing.T) {
	env := newProxyEnv(t)
	stream, err := env.client.Exec(env.aliceCtx(t))
	if err != nil {
		t.Fatalf("Exec: %v", err)
	}
	if err := stream.Send(execStart("fail")); err != nil {
		t.Fatalf("send: %v", err)
	}
	_, err = stream.Recv()
	if status.Code(err) != codes.FailedPrecondition || status.Convert(err).Message() != "exec refused" {
		t.Errorf("recv: %v, want FailedPrecondition exec refused", err)
	}
	if got := stream.Trailer().Get("x-agent-trailer"); len(got) != 1 || got[0] != "t" {
		t.Errorf("trailer = %v", stream.Trailer())
	}
}

func TestCancelReachesAgent(t *testing.T) {
	env := newProxyEnv(t)
	ctx, cancel := context.WithCancel(env.aliceCtx(t))
	stream, err := env.client.Exec(ctx)
	if err != nil {
		t.Fatalf("Exec: %v", err)
	}
	if err := stream.Send(execStart("echo")); err != nil {
		t.Fatalf("send start: %v", err)
	}
	if err := stream.Send(execStdin([]byte("x"))); err != nil {
		t.Fatalf("send: %v", err)
	}
	if _, err := stream.Recv(); err != nil {
		t.Fatalf("recv: %v", err)
	}
	cancel()
	select {
	case err := <-env.agent.execDone:
		if status.Code(err) != codes.Canceled {
			t.Errorf("agent stream ended with %v, want Canceled", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("agent stream was not cancelled")
	}
}

func TestRejectsCallers(t *testing.T) {
	env := newProxyEnv(t)
	tests := []struct {
		name string
		md   []string
		want codes.Code
	}{
		{name: "no token", md: []string{HeaderBox, boxes.BoxID("alice")}, want: codes.Unauthenticated},
		{name: "bad token", md: []string{auth.HeaderAuthorization, "Bearer wrong", HeaderBox, boxes.BoxID("alice")}, want: codes.Unauthenticated},
		{name: "someone else's box", md: []string{auth.HeaderAuthorization, "Bearer " + aliceToken, HeaderBox, boxes.BoxID("bob")}, want: codes.NotFound},
		{name: "no box", md: []string{auth.HeaderAuthorization, "Bearer " + aliceToken}, want: codes.InvalidArgument},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			ctx := metadata.AppendToOutgoingContext(t.Context(), tt.md...)
			if _, err := env.client.Hello(ctx, &agentv1.HelloRequest{}); status.Code(err) != tt.want {
				t.Errorf("Hello: %v, want %s", err, tt.want)
			}
			stream, err := env.client.Screen(ctx)
			if err != nil {
				t.Fatalf("Screen: %v", err)
			}
			if _, err := stream.Recv(); status.Code(err) != tt.want {
				t.Errorf("Screen recv: %v, want %s", err, tt.want)
			}
		})
	}
	if env.agent.calls.Load() != 0 {
		t.Errorf("agent received %d rejected calls", env.agent.calls.Load())
	}
	if n := env.screen.accepted.Load(); n != 0 {
		t.Errorf("the screen accepted %d rejected calls", n)
	}
}

func TestRejectsStateMethods(t *testing.T) {
	env := newProxyEnv(t)
	_, err := agentv1.NewStateClient(env.conn).Plan(env.aliceCtx(t), &agentv1.PlanRequest{})
	if status.Code(err) != codes.PermissionDenied {
		t.Errorf("State/Plan: %v, want PermissionDenied", err)
	}
	stream, err := agentv1.NewStateClient(env.conn).Restore(env.aliceCtx(t))
	if err != nil {
		t.Fatalf("Restore: %v", err)
	}
	if _, err := stream.Recv(); status.Code(err) != codes.PermissionDenied {
		t.Errorf("State/Restore: %v, want PermissionDenied", err)
	}
	if env.agent.calls.Load() != 0 {
		t.Errorf("agent received %d State calls", env.agent.calls.Load())
	}
}

func TestRejectsNonAgentMethods(t *testing.T) {
	env := newProxyEnv(t)
	err := env.conn.Invoke(env.aliceCtx(t), "/grpc.health.v1.Health/Check", &agentv1.HelloRequest{}, &agentv1.HelloResponse{})
	if status.Code(err) != codes.Unimplemented {
		t.Errorf("Invoke: %v, want Unimplemented", err)
	}
}

func TestReachesAgentStartedAfterRefusedDial(t *testing.T) {
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	addr := lis.Addr().String()
	if err := lis.Close(); err != nil {
		t.Fatalf("close: %v", err)
	}
	env := newProxyEnvAt(t, addr)
	if _, err := env.client.Hello(env.aliceCtx(t), &agentv1.HelloRequest{}); status.Code(err) != codes.Unavailable {
		t.Fatalf("Hello before the agent listens: %v, want Unavailable", err)
	}
	time.Sleep(3 * time.Second)

	agentSrv := grpc.NewServer()
	agentv1.RegisterSessionServer(agentSrv, env.agent)
	lis, err = net.Listen("tcp", addr)
	if err != nil {
		t.Fatalf("relisten %s: %v", addr, err)
	}
	go func() { _ = agentSrv.Serve(lis) }()
	t.Cleanup(agentSrv.Stop)

	deadline := time.Now().Add(2 * time.Second)
	for {
		_, err := env.client.Hello(env.aliceCtx(t), &agentv1.HelloRequest{})
		if err == nil {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("Hello after the agent started: %v", err)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

type proxyEnv struct {
	agent    *fakeAgent
	screen   *echoScreen
	resolver *fakeResolver
	proxy    *Proxy
	conn     *grpc.ClientConn
	client   agentv1.SessionClient
}

func newProxyEnv(t *testing.T) *proxyEnv {
	t.Helper()
	agentSrv := grpc.NewServer(grpc.MaxRecvMsgSize(MaxMessageSize))
	env := newProxyEnvAt(t, "")
	agentv1.RegisterSessionServer(agentSrv, env.agent)
	env.resolver.addr = serve(t, agentSrv)
	env.screen = serveEchoScreen(t)
	env.resolver.screen = env.screen.addr
	return env
}

func newProxyEnvAt(t *testing.T, agentAddr string) *proxyEnv {
	t.Helper()
	agent := &fakeAgent{execDone: make(chan error, 1)}
	resolver := &fakeResolver{addr: agentAddr}

	sum := sha256.Sum256([]byte(aliceToken))
	authn := auth.New(staticUsers{hex.EncodeToString(sum[:]): "alice"}, &fakeShares{})
	proxy := NewProxy(resolver, fakeCreds{}, dialer.NewPool())
	t.Cleanup(proxy.pool.Close)
	edgeSrv := grpc.NewServer(
		grpc.ForceServerCodecV2(NewCodec()),
		grpc.UnknownServiceHandler(proxy.Handle),
		grpc.ChainUnaryInterceptor(authn.Unary),
		grpc.ChainStreamInterceptor(authn.Stream),
		grpc.MaxRecvMsgSize(MaxMessageSize),
	)
	conn, err := grpc.NewClient(serve(t, edgeSrv), grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(MaxMessageSize)))
	if err != nil {
		t.Fatalf("dial edge: %v", err)
	}
	t.Cleanup(func() { _ = conn.Close() })
	return &proxyEnv{agent: agent, resolver: resolver, proxy: proxy, conn: conn, client: agentv1.NewSessionClient(conn)}
}

func (e *proxyEnv) aliceCtx(t *testing.T) context.Context {
	return metadata.AppendToOutgoingContext(t.Context(),
		auth.HeaderAuthorization, "Bearer "+aliceToken, HeaderBox, boxes.BoxID("alice"))
}

func serve(t *testing.T, srv *grpc.Server) string {
	t.Helper()
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	go func() { _ = srv.Serve(lis) }()
	t.Cleanup(srv.Stop)
	return lis.Addr().String()
}

// fakeResolver applies the Boxes ownership rule and routes every owned box to addr; opaque hides addr behind a key nothing resolves.
type fakeResolver struct {
	addr   string
	screen string
	wake   string
	err    error
	opaque bool

	mu      sync.Mutex
	reached []string
}

func (r *fakeResolver) Resolve(_ context.Context, user, boxID string) (machines.Endpoint, string, error) {
	if boxID != boxes.BoxID(user) {
		return machines.Endpoint{}, "", boxes.ErrNoBox
	}
	ep := machines.TCPEndpoint(r.addr)
	ep.Screen = machines.TCPEndpoint(r.screen).Dial
	if r.opaque {
		ep.Key = "box.invalid:1"
	}
	return ep, r.wake, r.err
}

func (r *fakeResolver) Reached(boxID string) func() {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.reached = append(r.reached, boxID)
	return func() {}
}

func (r *fakeResolver) reaches() []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	return slices.Clone(r.reached)
}

// fakeCreds issues "cred:<box>:<wake>" and "cred:<box>:<wake>:<principal>".
type fakeCreds struct{}

func (fakeCreds) Credential(box, wake string) string { return "cred:" + box + ":" + wake }

func (fakeCreds) ShareCredential(box, wake, principal string) string {
	return "cred:" + box + ":" + wake + ":" + principal
}

// fakeAgent echoes what the edge delivered: Hello returns any edge token header as agent_version, authorization values as models and the box header as default_main_model.
type fakeAgent struct {
	agentv1.UnimplementedSessionServer
	calls    atomic.Int64
	execDone chan error
}

func (a *fakeAgent) Hello(ctx context.Context, _ *agentv1.HelloRequest) (*agentv1.HelloResponse, error) {
	a.calls.Add(1)
	md, _ := metadata.FromIncomingContext(ctx)
	if err := grpc.SetHeader(ctx, metadata.Pairs("x-agent-header", "h")); err != nil {
		return nil, err
	}
	if err := grpc.SetTrailer(ctx, metadata.Pairs("x-agent-trailer", "t")); err != nil {
		return nil, err
	}
	return &agentv1.HelloResponse{
		AgentVersion:     first(md.Get(HeaderEdgeToken)),
		Models:           md.Get(auth.HeaderAuthorization),
		DefaultMainModel: strings.TrimSpace(first(md.Get(HeaderBox)) + " " + first(md.Get(credential.Header))),
	}, nil
}

// Exec runs its start command: fail refuses, echo echoes each stdin frame, any other echoes all stdin at half-close and exits 7.
func (a *fakeAgent) Exec(stream grpc.BidiStreamingServer[agentv1.ExecClientFrame, agentv1.ExecServerFrame]) error {
	a.calls.Add(1)
	start, err := stream.Recv()
	if err != nil {
		return err
	}
	switch start.GetStart().GetCommand() {
	case "fail":
		stream.SetTrailer(metadata.Pairs("x-agent-trailer", "t"))
		return status.Error(codes.FailedPrecondition, "exec refused")
	case "echo":
		err := a.echo(stream)
		a.execDone <- err
		return err
	}
	var in []byte
	for {
		f, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return err
		}
		in = append(in, f.GetStdin()...)
	}
	if err := stream.Send(&agentv1.ExecServerFrame{Body: &agentv1.ExecServerFrame_Stdout{Stdout: in}}); err != nil {
		return err
	}
	return stream.Send(&agentv1.ExecServerFrame{Body: &agentv1.ExecServerFrame_ExitCode{ExitCode: 7}})
}

func (a *fakeAgent) echo(stream grpc.BidiStreamingServer[agentv1.ExecClientFrame, agentv1.ExecServerFrame]) error {
	for {
		in, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return status.FromContextError(stream.Context().Err()).Err()
		}
		if err := stream.Send(&agentv1.ExecServerFrame{Body: &agentv1.ExecServerFrame_Stdout{Stdout: in.GetStdin()}}); err != nil {
			return err
		}
	}
}

// echoScreen stands in for a box's RFB server: it echoes each connection until the client half-closes.
type echoScreen struct {
	addr     string
	accepted atomic.Int64
	closed   chan struct{}
}

func serveEchoScreen(t *testing.T) *echoScreen {
	t.Helper()
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	t.Cleanup(func() { _ = lis.Close() })
	s := &echoScreen{addr: lis.Addr().String(), closed: make(chan struct{}, 16)}
	go func() {
		for {
			conn, err := lis.Accept()
			if err != nil {
				return
			}
			s.accepted.Add(1)
			go func() {
				_, _ = io.Copy(conn, conn)
				_ = conn.Close()
				s.closed <- struct{}{}
			}()
		}
	}()
	return s
}

func execStart(command string) *agentv1.ExecClientFrame {
	return &agentv1.ExecClientFrame{Body: &agentv1.ExecClientFrame_Start{Start: &agentv1.ExecStart{Command: command}}}
}

func execStdin(data []byte) *agentv1.ExecClientFrame {
	return &agentv1.ExecClientFrame{Body: &agentv1.ExecClientFrame_Stdin{Stdin: data}}
}

func screenData(data []byte) *agentv1.ScreenClientFrame {
	return &agentv1.ScreenClientFrame{Body: &agentv1.ScreenClientFrame_Data{Data: data}}
}

func recvScreen(t *testing.T, stream agentv1.Session_ScreenClient, n int) []byte {
	t.Helper()
	var got []byte
	for len(got) < n {
		f, err := stream.Recv()
		if err != nil {
			t.Fatalf("recv after %d of %d bytes: %v", len(got), n, err)
		}
		got = append(got, f.GetData()...)
	}
	return got
}

func first(values []string) string {
	if len(values) == 0 {
		return ""
	}
	return values[0]
}

type staticUsers map[string]string

func (u staticUsers) ByTokenSHA256(_ context.Context, sum string) (string, bool) {
	id, ok := u[sum]
	return id, ok
}

func (staticUsers) Lifetime(string, string) (context.Context, func()) {
	return context.Background(), func() {}
}
