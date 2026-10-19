package sandboxd

import (
	"bufio"
	"bytes"
	"cmp"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"time"

	"github.com/loomhand/cocoonbox/api/machines"
)

const (
	requestTimeout = 30 * time.Second
	dialTimeout    = 5 * time.Second
	maxErrorBody   = 4 << 10
)

// apiError is a sandboxd error answer; 400 is machines.ErrRefused, 404 machines.ErrNotFound, 409 machines.ErrConflict and 429 machines.ErrCapacity.
type apiError struct {
	status  int
	message string
}

func (e *apiError) Error() string { return fmt.Sprintf("sandboxd %d: %s", e.status, e.message) }

func (e *apiError) Unwrap() error {
	switch e.status {
	case http.StatusBadRequest:
		return machines.ErrRefused
	case http.StatusNotFound:
		return machines.ErrNotFound
	case http.StatusConflict:
		return machines.ErrConflict
	case http.StatusTooManyRequests:
		return machines.ErrCapacity
	default:
		return nil
	}
}

type poolKey struct {
	Template string `json:"template"`
	Net      string `json:"net"`
	Size     string `json:"size"`
}

// row is one claim of the sandboxd index; Token is set only on the root by-id read.
type row struct {
	ID         string            `json:"id"`
	Key        poolKey           `json:"key"`
	Token      string            `json:"token,omitempty"`
	Tenant     string            `json:"tenant,omitempty"`
	Metadata   map[string]string `json:"metadata,omitempty"`
	Hibernated bool              `json:"hibernated"`
	Archived   bool              `json:"archived,omitzero"`
	Restarts   int               `json:"restarts,omitzero"`
	Failed     string            `json:"failed,omitempty"`
	ClaimedAt  time.Time         `json:"claimed_at,omitzero"`
}

type envVar struct {
	Value  string  `json:"value"`
	Guest  *bool   `json:"guest,omitempty"`
	Inject *inject `json:"inject,omitempty"`
}

type inject struct {
	Hosts       []string `json:"hosts"`
	Header      string   `json:"header,omitempty"`
	Query       string   `json:"query,omitempty"`
	Placeholder string   `json:"placeholder,omitempty"`
}

type claimRequest struct {
	poolKey
	TTLSeconds int               `json:"ttl_seconds"`
	OnExpire   string            `json:"on_expire"`
	Metadata   map[string]string `json:"metadata"`
}

type renewRequest struct {
	TTLSeconds int    `json:"ttl_seconds"`
	OnExpire   string `json:"on_expire"`
}

type execRequest struct {
	Argv           []string          `json:"argv"`
	Env            map[string]string `json:"env,omitempty"`
	TimeoutSeconds int               `json:"timeout_seconds"`
}

type execResponse struct {
	ExitCode int32  `json:"exit_code"`
	Stdout   string `json:"stdout"`
	Stderr   string `json:"stderr"`
}

type tenantRequest struct {
	Token       string `json:"token,omitempty"`
	MaxClaims   int    `json:"max_claims"`
	EgressClass string `json:"egress_class,omitempty"`
}

// client speaks the sandboxd HTTP API with the node's root token.
type client struct {
	base  *url.URL
	token string
	http  *http.Client
}

func newClient(rawURL, token string) (*client, error) {
	base, err := url.Parse(rawURL)
	if err != nil || base.Host == "" {
		return nil, fmt.Errorf("sandboxd url %q must be an absolute URL", rawURL)
	}
	return &client{base: base, token: token, http: &http.Client{}}, nil
}

// claim claims with token, a tenant's or the root one.
func (c *client) claim(ctx context.Context, token string, req claimRequest) (string, error) {
	var resp struct {
		ID string `json:"id"`
	}
	err := c.request(ctx, cmp.Or(token, c.token), http.MethodPost, "/v1/claim", nil, req, &resp)
	return resp.ID, err
}

func (c *client) list(ctx context.Context) ([]row, error) {
	var resp struct {
		Sandboxes []row `json:"sandboxes"`
	}
	err := c.do(ctx, http.MethodGet, "/v1/sandboxes", nil, nil, &resp)
	return resp.Sandboxes, err
}

func (c *client) find(ctx context.Context, key, value string) ([]row, error) {
	var resp struct {
		Sandboxes []row `json:"sandboxes"`
	}
	err := c.do(ctx, http.MethodGet, "/v1/sandboxes", url.Values{"metadata": {key + "=" + value}}, nil, &resp)
	return resp.Sandboxes, err
}

func (c *client) get(ctx context.Context, id string) (row, error) {
	var r row
	err := c.do(ctx, http.MethodGet, "/v1/sandboxes/"+url.PathEscape(id), nil, nil, &r)
	return r, err
}

// verb posts one lifecycle verb (wake, hibernate, release, renew) with the root token.
func (c *client) verb(ctx context.Context, id, verb string, body any) error {
	return c.do(ctx, http.MethodPost, "/v1/sandboxes/"+url.PathEscape(id)+"/"+verb, nil, body, nil)
}

func (c *client) env(ctx context.Context, id string) (map[string]envVar, error) {
	var resp struct {
		Env map[string]envVar `json:"env"`
	}
	err := c.do(ctx, http.MethodGet, "/v1/sandboxes/"+url.PathEscape(id)+"/env", nil, nil, &resp)
	return resp.Env, err
}

// patchEnv merges entries into the claim's env; a nil entry removes the name.
func (c *client) patchEnv(ctx context.Context, id string, entries map[string]*envVar) error {
	return c.do(ctx, http.MethodPatch, "/v1/sandboxes/"+url.PathEscape(id)+"/env", nil, map[string]any{"env": entries}, nil)
}

// exec runs a buffered command as the guest's root with the sandbox's own token.
func (c *client) exec(ctx context.Context, id, token string, req execRequest) (execResponse, error) {
	var resp execResponse
	err := c.request(ctx, token, http.MethodPost, "/v1/sandboxes/"+url.PathEscape(id)+"/exec", nil, req, &resp)
	return resp, err
}

// dialPort opens the passive port relay to guest port on sandbox id.
func (c *client) dialPort(ctx context.Context, id string, port int) (net.Conn, error) {
	conn, err := (&net.Dialer{Timeout: dialTimeout}).DialContext(ctx, "tcp", c.base.Host)
	if err != nil {
		return nil, fmt.Errorf("dial sandboxd: %w", err)
	}
	if deadline, ok := ctx.Deadline(); ok {
		_ = conn.SetDeadline(deadline)
	} else {
		_ = conn.SetDeadline(time.Now().Add(dialTimeout))
	}
	path := "/v1/sandboxes/" + url.PathEscape(id) + "/ports/" + strconv.Itoa(port)
	req := fmt.Sprintf("GET %s HTTP/1.1\r\nHost: %s\r\nAuthorization: Bearer %s\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n", path, c.base.Host, c.token)
	if _, err = io.WriteString(conn, req); err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("send relay upgrade: %w", err)
	}
	br := bufio.NewReader(conn)
	resp, err := http.ReadResponse(br, nil)
	if err != nil {
		_ = conn.Close()
		return nil, fmt.Errorf("read relay upgrade: %w", err)
	}
	defer func() { _ = resp.Body.Close() }()
	if resp.StatusCode != http.StatusSwitchingProtocols {
		defer func() { _ = conn.Close() }()
		return nil, responseError(resp)
	}
	_ = conn.SetDeadline(time.Time{})
	return &bufferedConn{Conn: conn, r: br}, nil
}

func (c *client) do(ctx context.Context, method, path string, query url.Values, body, out any) error {
	return c.request(ctx, c.token, method, path, query, body, out)
}

func (c *client) request(ctx context.Context, token, method, path string, query url.Values, body, out any) error {
	var payload io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return fmt.Errorf("encode %s %s: %w", method, path, err)
		}
		payload = bytes.NewReader(raw)
	}
	u := c.base.JoinPath(path)
	u.RawQuery = query.Encode()
	req, err := http.NewRequestWithContext(ctx, method, u.String(), payload)
	if err != nil {
		return fmt.Errorf("build %s %s: %w", method, path, err)
	}
	req.Header.Set("Authorization", "Bearer "+token)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.http.Do(req)
	if err != nil {
		return fmt.Errorf("%s %s: %w", method, path, err)
	}
	defer func() { _ = resp.Body.Close() }()
	if resp.StatusCode/100 != 2 {
		return fmt.Errorf("%s %s: %w", method, path, responseError(resp))
	}
	if out == nil || resp.StatusCode == http.StatusNoContent {
		return nil
	}
	if err := json.NewDecoder(resp.Body).Decode(out); err != nil {
		return fmt.Errorf("decode %s %s: %w", method, path, err)
	}
	return nil
}

// bufferedConn keeps the bytes the upgrade reader buffered past the 101.
type bufferedConn struct {
	net.Conn
	r *bufio.Reader
}

func (c *bufferedConn) Read(p []byte) (int, error) { return c.r.Read(p) }

// CloseWrite half-closes the relay, which sandboxd carries to the guest port.
func (c *bufferedConn) CloseWrite() error {
	if tc, ok := c.Conn.(*net.TCPConn); ok {
		return tc.CloseWrite()
	}
	return c.Close()
}

func responseError(resp *http.Response) error {
	var body struct {
		Error string `json:"error"`
	}
	raw, _ := io.ReadAll(io.LimitReader(resp.Body, maxErrorBody))
	if json.Unmarshal(raw, &body) != nil || body.Error == "" {
		body.Error = http.StatusText(resp.StatusCode)
	}
	return &apiError{status: resp.StatusCode, message: body.Error}
}

func isStatus(err error, status int) bool {
	var apiErr *apiError
	return errors.As(err, &apiErr) && apiErr.status == status
}
