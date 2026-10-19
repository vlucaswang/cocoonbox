package edge

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"io"
	"net"
	"net/http"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"

	"github.com/loomhand/cocoonbox/api/auth"
	"github.com/loomhand/cocoonbox/api/boxes"
	"github.com/loomhand/cocoonbox/api/dialer"
	agentv1 "github.com/loomhand/cocoonbox/api/gen/cocoonbox/agent/v1"
)

const (
	bobToken     = "bob-secret"
	carolToken   = "carol-secret"
	openSecret   = auth.SecretPrefix + "0123456789abcdef0123456789abcdef"
	namedSecret  = auth.SecretPrefix + "fedcba9876543210fedcba9876543210"
	staleSecret  = auth.SecretPrefix + "00000000000000000000000000000000"
	toolsListRPC = `{"jsonrpc":"2.0","id":1,"method":"tools/list"}`
	// shareLikeToken is a user token that happens to carry the share prefix.
	shareLikeToken = auth.SecretPrefix + "user-token-of-dave"
)

func TestMCPAuthorizesOwnerOrShare(t *testing.T) {
	env := newMCPEnv(t)
	env.resolver.wake = "w1"
	box := boxes.BoxID("alice")
	env.shares.put(openSecret, auth.Share{ID: "sh-open", Owner: "alice", Box: box, Write: true})
	env.shares.put(namedSecret, auth.Share{ID: "sh-named", Owner: "alice", Box: box, Users: []string{"bob"}})
	env.shares.put(staleSecret, auth.Share{ID: "sh-stale", Owner: "alice", Box: box, ExpiresAt: time.Now().Add(-time.Second)})
	tests := []struct {
		name, authorization, guest, box string
		want                            int
		credential                      string
	}{
		{name: "owner", authorization: "Bearer " + aliceToken, box: box, want: http.StatusOK, credential: "cred:" + box + ":w1"},
		{name: "open share", authorization: "Bearer " + openSecret, box: box, want: http.StatusOK, credential: "cred:" + box + ":w1:share:sh-open:write"},
		{name: "open share ignores a guest token", authorization: "Bearer " + openSecret, guest: carolToken, box: box, want: http.StatusOK, credential: "cred:" + box + ":w1:share:sh-open:write"},
		{name: "named share, named user", authorization: "Bearer " + namedSecret, guest: bobToken, box: box, want: http.StatusOK, credential: "cred:" + box + ":w1:share:sh-named:read:bob"},
		{name: "named share without a token", authorization: "Bearer " + namedSecret, box: box, want: http.StatusUnauthorized},
		{name: "named share, another user", authorization: "Bearer " + namedSecret, guest: carolToken, box: box, want: http.StatusForbidden},
		{name: "named share, bad token", authorization: "Bearer " + namedSecret, guest: "nope", box: box, want: http.StatusUnauthorized},
		{name: "expired share", authorization: "Bearer " + staleSecret, box: box, want: http.StatusForbidden},
		{name: "revoked or regenerated share", authorization: "Bearer " + auth.SecretPrefix + "gone", box: box, want: http.StatusForbidden},
		{name: "share of another box", authorization: "Bearer " + openSecret, box: boxes.BoxID("bob"), want: http.StatusForbidden},
		{name: "the share owner's guest token is no owner token", authorization: "Bearer " + namedSecret, guest: aliceToken, box: box, want: http.StatusForbidden},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			req := env.request(t, http.MethodPost, tt.box, toolsListRPC)
			req.Header.Set("Authorization", tt.authorization)
			if tt.guest != "" {
				req.Header.Set(auth.HeaderGuestToken, tt.guest)
			}
			resp := env.do(t, req)
			defer resp.Body.Close()
			if resp.StatusCode != tt.want {
				body, _ := io.ReadAll(resp.Body)
				t.Fatalf("status %d %s, want %d", resp.StatusCode, body, tt.want)
			}
			if tt.want != http.StatusOK {
				return
			}
			var got map[string]string
			if err := json.NewDecoder(resp.Body).Decode(&got); err != nil {
				t.Fatalf("decode: %v", err)
			}
			if got["credential"] != tt.credential || got["authorization"] != "" || got["guest"] != "" {
				t.Errorf("upstream saw credential %q, authorization %q, guest %q; want %q and neither header", got["credential"], got["authorization"], got["guest"], tt.credential)
			}
		})
	}
}

func TestUserTokenWinsOverTheSharePrefix(t *testing.T) {
	env := newMCPEnv(t)
	env.resolver.wake = "w1"
	sum := sha256.Sum256([]byte(shareLikeToken))
	env.users[hex.EncodeToString(sum[:])] = "dave"
	env.shares.put(shareLikeToken, auth.Share{ID: "sh-twin", Owner: "alice", Box: boxes.BoxID("alice")})
	req := env.request(t, http.MethodPost, boxes.BoxID("dave"), toolsListRPC)
	req.Header.Set("Authorization", "Bearer "+shareLikeToken)
	resp := env.do(t, req)
	defer resp.Body.Close()
	var got map[string]string
	if err := json.NewDecoder(resp.Body).Decode(&got); err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("status %d, %v", resp.StatusCode, err)
	}
	if want := "cred:" + boxes.BoxID("dave") + ":w1"; got["credential"] != want {
		t.Errorf("a user token starting with %s went to %q, want dave's owner credential %q", auth.SecretPrefix, got["credential"], want)
	}
	req = env.request(t, http.MethodPost, boxes.BoxID("alice"), toolsListRPC)
	req.Header.Set("Authorization", "Bearer "+shareLikeToken)
	resp = env.do(t, req)
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusNotFound {
		t.Errorf("dave's token on alice's box = %d, want 404 as dave, not admitted as the share", resp.StatusCode)
	}
}

func TestShareStreamsEndWithTheShare(t *testing.T) {
	env := newMCPEnv(t)
	box := boxes.BoxID("alice")
	for _, end := range []string{"write switched", "share ended"} {
		cut, moded := make(chan struct{}), make(chan struct{})
		env.shares.put(openSecret, auth.Share{ID: "sh-open", Owner: "alice", Box: box, Cut: cut, Moded: moded})
		env.upstream.closed = make(chan struct{})
		req := env.request(t, http.MethodGet, box, "")
		req.Header.Set("Authorization", "Bearer "+openSecret)
		resp := env.do(t, req)
		if line, err := bufio.NewReader(resp.Body).ReadString('\n'); err != nil || line != "data: 0\n" {
			t.Fatalf("%s: first event %q, %v", end, line, err)
		}
		if end == "share ended" {
			close(cut)
		} else {
			close(moded)
		}
		for _, done := range []<-chan struct{}{env.upstream.closed, served(env)} {
			select {
			case <-done:
			case <-time.After(time.Second):
				t.Fatalf("%s: the stream is still open after 1 s", end)
			}
		}
		_ = resp.Body.Close()
	}
}

func TestWriteSwitchCutsOnlyServerStreams(t *testing.T) {
	cut, moded := make(chan struct{}), make(chan struct{})
	g := &auth.Grant{Share: auth.Share{Cut: cut, Moded: moded}}
	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()
	done := make(chan struct{})
	go func() { cutOnEnd(ctx, cancel, g, false); close(done) }()
	close(moded)
	select {
	case <-done:
		t.Fatal("a write switch cut a POST exchange, which may carry a running tool call")
	case <-time.After(100 * time.Millisecond):
	}
	close(cut)
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("the end of the share did not cut the POST exchange")
	}
}

func TestShareSecretIsNoGRPCToken(t *testing.T) {
	env := newProxyEnv(t)
	ctx := metadata.AppendToOutgoingContext(t.Context(), auth.HeaderAuthorization, "Bearer "+openSecret, HeaderBox, boxes.BoxID("alice"))
	if _, err := env.client.Hello(ctx, &agentv1.HelloRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Errorf("Hello with a share secret: %v, want Unauthenticated", err)
	}
	stream, err := env.client.Exec(ctx)
	if err != nil {
		t.Fatalf("Exec: %v", err)
	}
	if _, err := stream.Recv(); status.Code(err) != codes.Unauthenticated {
		t.Errorf("Exec with a share secret: %v, want Unauthenticated", err)
	}
	if env.agent.calls.Load() != 0 {
		t.Errorf("agent received %d share-secret calls", env.agent.calls.Load())
	}
}

func TestScreenFiltersBothDirections(t *testing.T) {
	stream, want := serverStream()
	box := serveScriptedScreen(t, stream)
	resolver := &fakeResolver{screen: box.addr}
	proxy := NewProxy(resolver, fakeCreds{}, dialer.NewPool())
	t.Cleanup(proxy.pool.Close)
	cut := make(chan struct{})
	grant := auth.Grant{Share: auth.Share{ID: "sh-open", Owner: "alice", Box: boxes.BoxID("alice"), Cut: cut}}
	viewer, client := net.Pipe()
	served := make(chan error, 1)
	go func() { served <- proxy.Screen(t.Context(), grant, viewer) }()

	sent := [][]byte{
		[]byte("RFB 003.008\n"),
		{1},
		{0},
		append([]byte{0, 0, 0, 0, 32}, make([]byte, 15)...),
		{2, 0, 0, 3, 0, 0, 0, 16, 0xc0, 0xa1, 0xe5, 0xce, 0, 0, 0, 7},
		{4, 1, 0, 0, 0, 0, 0, 0x61},
		{5, 1, 0, 10, 0, 20},
		append([]byte{6, 0, 0, 0, 0, 0, 0, 5}, "hello"...),
		{3, 1, 0, 0, 0, 0, 5, 0, 3, 32},
	}
	toBox := bytes.Join([][]byte{sent[0], {1}, {1}, sent[3], {2, 0, 0, 1, 0, 0, 0, 16}, sent[8]}, nil)
	var got []byte
	read := make(chan error, 1)
	go func() {
		buf := make([]byte, 256)
		for len(got) < len(want) {
			n, err := client.Read(buf)
			if err != nil {
				read <- err
				return
			}
			got = append(got, buf[:n]...)
		}
		read <- nil
	}()
	for _, b := range sent {
		if _, err := client.Write(b); err != nil {
			t.Fatalf("viewer write: %v", err)
		}
	}
	select {
	case err := <-read:
		if err != nil || !bytes.Equal(got, want) {
			t.Fatalf("the viewer saw %v (%v), want %v", got, err, want)
		}
	case <-time.After(5 * time.Second):
		t.Fatalf("the viewer saw %d of %d bytes", len(got), len(want))
	}
	var atBox []byte
	for len(atBox) < len(toBox) {
		select {
		case b := <-box.got:
			atBox = append(atBox, b...)
		case <-time.After(5 * time.Second):
			t.Fatalf("the box saw %v, want %v", atBox, toBox)
		}
	}
	if !bytes.Equal(atBox, toBox) {
		t.Errorf("the box saw %v, want %v", atBox, toBox)
	}
	start := time.Now()
	close(cut)
	select {
	case <-served:
		if took := time.Since(start); took > time.Second {
			t.Errorf("the bridge ended %s after the share", took)
		}
	case <-time.After(time.Second):
		t.Fatal("the bridge outlived its share by 1 s")
	}
}

// scriptedScreen plays stream as the box's RFB server, 7 bytes a write, sending the security part only once the
// viewer's version has arrived, and reports every byte the bridge forwards.
type scriptedScreen struct {
	addr string
	got  chan []byte
}

func serveScriptedScreen(t *testing.T, stream []byte) *scriptedScreen {
	t.Helper()
	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	t.Cleanup(func() { _ = lis.Close() })
	s := &scriptedScreen{addr: lis.Addr().String(), got: make(chan []byte, 64)}
	go func() {
		conn, err := lis.Accept()
		if err != nil {
			return
		}
		defer func() { _ = conn.Close() }()
		send := func(b []byte) error {
			for len(b) > 0 {
				n := min(7, len(b))
				if _, err := conn.Write(b[:n]); err != nil {
					return err
				}
				b = b[n:]
			}
			return nil
		}
		if send(stream[:12]) != nil {
			return
		}
		rest := stream[12:]
		buf := make([]byte, 256)
		for {
			n, err := conn.Read(buf)
			if err != nil {
				return
			}
			s.got <- bytes.Clone(buf[:n])
			if rest != nil {
				if send(rest) != nil {
					return
				}
				rest = nil
			}
		}
	}()
	return s
}

// fakeShares serves shares by the sha256 of their secret; a zero ExpiresAt means an hour from now.
type fakeShares struct {
	mu    sync.Mutex
	bySum map[string]auth.Share
}

func (f *fakeShares) ShareBySecretSHA256(sum string) (auth.Share, bool) {
	f.mu.Lock()
	defer f.mu.Unlock()
	sh, ok := f.bySum[sum]
	return sh, ok
}

func (f *fakeShares) put(secret string, sh auth.Share) {
	if sh.ExpiresAt.IsZero() {
		sh.ExpiresAt = time.Now().Add(time.Hour)
	}
	sum := sha256.Sum256([]byte(secret))
	f.mu.Lock()
	defer f.mu.Unlock()
	f.bySum[hex.EncodeToString(sum[:])] = sh
}

// served closes when the edge handler of the next exchange returns.
func served(env *mcpEnv) <-chan struct{} {
	done := make(chan struct{})
	go func() { <-env.served; close(done) }()
	return done
}
