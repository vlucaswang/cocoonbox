package edge

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net"

	"github.com/projecteru2/core/log"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	"github.com/loomhand/cocoonbox/api/credential"
	agentv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/agent/v1"
	"github.com/loomhand/cocoonbox/api/machines"
)

const (
	viewerReadChunk = 32 << 10
	screenReadChunk = 64 << 10
)

// Screen bridges a share's viewer to the box's RFB server until either side closes or the share ends;
// the viewer's input never reaches the box, and the box's clipboard never reaches the viewer.
func (p *Proxy) Screen(ctx context.Context, g auth.Grant, viewer io.ReadWriter) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	go func() {
		select {
		case <-g.Cut:
		case <-ctx.Done():
		}
		cancel()
	}()
	ep, _, err := p.resolver.Resolve(ctx, g.Owner, g.Box)
	if err != nil {
		return err
	}
	defer p.resolver.Reached(g.Box)()
	release, err := p.holdScreen(g.Box)
	if err != nil {
		return err
	}
	defer release()
	vnc, err := dialScreen(ctx, g.Box, ep)
	if err != nil {
		return err
	}
	defer func() { _ = vnc.Close() }()
	session := &rfbSession{}
	go func() {
		forwardViewer(vnc, viewer, &viewOnly{session: session})
		cancel()
	}()
	screen := &screenOnly{session: session}
	buf := make([]byte, screenReadChunk)
	for {
		n, err := vnc.Read(buf)
		if n > 0 {
			out, ferr := screen.feed(buf[:n])
			if ferr != nil {
				return ferr
			}
			if len(out) > 0 {
				if _, werr := viewer.Write(out); werr != nil {
					return werr
				}
			}
		}
		if err != nil {
			return screenEnd(ctx, err)
		}
	}
}

// EndShare asks the box agent to close every MCP session of share id.
func (p *Proxy) EndShare(ctx context.Context, owner, box, id string) error {
	ep, wake, err := p.resolver.Resolve(ctx, owner, box)
	if err != nil {
		return err
	}
	conn, err := p.conn(ctx, box, ep)
	if err != nil {
		return err
	}
	if wake != "" {
		ctx = metadata.AppendToOutgoingContext(ctx, credential.Header, p.creds.Credential(box, wake))
	}
	_, err = agentv1.NewAgentsClient(conn).End(ctx, &agentv1.EndRequest{Target: &agentv1.EndRequest_ShareId{ShareId: id}})
	return err
}

// relayScreen serves the owner's Session.Screen from the box's RFB server.
func (p *Proxy) relayScreen(ctx context.Context, down grpc.ServerStream, box string, ep machines.Endpoint) error {
	open := &agentv1.ScreenClientFrame{}
	if err := down.RecvMsg(open); err != nil {
		return err
	}
	if open.GetOpen() == nil {
		return status.Error(codes.InvalidArgument, "the first Screen frame must be open")
	}
	release, err := p.holdScreen(box)
	if err != nil {
		return err
	}
	defer release()
	vnc, err := dialScreen(ctx, box, ep)
	if err != nil {
		return err
	}
	defer func() { _ = vnc.Close() }()
	go forwardScreenInput(down, vnc)
	buf := make([]byte, screenReadChunk)
	for {
		n, err := vnc.Read(buf)
		if n > 0 {
			if serr := down.SendMsg(&agentv1.ScreenServerFrame{Data: bytes.Clone(buf[:n])}); serr != nil {
				return serr
			}
		}
		if err != nil {
			return screenEnd(ctx, err)
		}
	}
}

// holdScreen takes one of the box's screen relay slots and returns its release.
func (p *Proxy) holdScreen(box string) (func(), error) {
	p.screensMu.Lock()
	defer p.screensMu.Unlock()
	if p.screens[box] >= maxScreensPerBox {
		return nil, status.Errorf(codes.ResourceExhausted, "box %s has %d screens open", box, maxScreensPerBox)
	}
	p.screens[box]++
	return func() {
		p.screensMu.Lock()
		defer p.screensMu.Unlock()
		if p.screens[box]--; p.screens[box] == 0 {
			delete(p.screens, box)
		}
	}, nil
}

// dialScreen connects to the box's RFB server; the connection closes when ctx ends.
func dialScreen(ctx context.Context, box string, ep machines.Endpoint) (net.Conn, error) {
	conn, err := ep.Screen(ctx)
	if err != nil {
		log.WithFunc("edge.dialScreen").Errorf(ctx, err, "dial the screen of box %s", box)
		return nil, status.Error(codes.Unavailable, "connect to the box screen")
	}
	context.AfterFunc(ctx, func() { _ = conn.Close() })
	return conn, nil
}

// screenEnd maps the end of a box screen read: EOF is OK, and a gone caller is its context error.
func screenEnd(ctx context.Context, err error) error {
	switch {
	case errors.Is(err, io.EOF):
		return nil
	case ctx.Err() != nil:
		return status.FromContextError(ctx.Err()).Err()
	default:
		return status.Error(codes.Unavailable, "read the box screen")
	}
}

// forwardScreenInput writes the client's data frames to the RFB server; the client's half-close half-closes it.
func forwardScreenInput(down grpc.ServerStream, vnc net.Conn) {
	for {
		f := &agentv1.ScreenClientFrame{}
		if err := down.RecvMsg(f); err != nil {
			if errors.Is(err, io.EOF) {
				closeWrite(vnc)
			} else {
				_ = vnc.Close()
			}
			return
		}
		if data := f.GetData(); len(data) > 0 {
			if _, err := vnc.Write(data); err != nil {
				return
			}
		}
	}
}

// forwardViewer sends the viewer's handshake and framebuffer requests to the box until the viewer closes or sends something else.
func forwardViewer(vnc net.Conn, viewer io.Reader, filter *viewOnly) {
	buf := make([]byte, viewerReadChunk)
	for {
		n, err := viewer.Read(buf)
		if n > 0 {
			out, ferr := filter.feed(buf[:n])
			if ferr != nil {
				return
			}
			if len(out) > 0 {
				if _, werr := vnc.Write(out); werr != nil {
					return
				}
			}
		}
		if err != nil {
			closeWrite(vnc)
			return
		}
	}
}

func closeWrite(conn net.Conn) {
	if hc, ok := conn.(interface{ CloseWrite() error }); ok {
		_ = hc.CloseWrite()
		return
	}
	_ = conn.Close()
}
