// Package gwkeys issues and revokes gw keys, one per owner, and reads their usage and sets their owners' budgets through gw's admin API.
package gwkeys

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"time"
)

const (
	tenant          = "cocoonbox-boxes"
	product         = "cocoonbox"
	qps             = 40
	dailyTokens     = 50_000_000
	tokensPerMinute = 2_400_000

	keysPath   = "/admin/keys"
	usagePath  = "/admin/usage/users"
	budgetPath = "/admin/tenants/" + tenant + "/users"

	// Unlimited as a daily cap lifts the owner's own cap; the tenant pool still applies.
	Unlimited     = -1
	unlimitedWire = "unlimited"

	pastExpiry = 0

	pageSize       = 10_000
	requestTimeout = 30 * time.Second
	maxErrorBody   = 4 << 10
)

// Usage is one model's share of an owner's gw usage.
type Usage struct {
	Model            string `json:"model"`
	Requests         int64  `json:"requests"`
	PromptTokens     int64  `json:"prompt_tokens"`
	CompletionTokens int64  `json:"completion_tokens"`
	TotalTokens      int64  `json:"total_tokens"`
	CostMicros       int64  `json:"cost_micros"`
}

// Client manages keys in tenant cocoonbox-boxes, one per owner.
type Client struct {
	base  *url.URL
	token string
	http  *http.Client
}

// New talks to gw's admin API at base with a tenant admin token.
func New(base, token string) (*Client, error) {
	u, err := url.Parse(base)
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return nil, fmt.Errorf("gw admin url %q must be http(s)://host[:port]", base)
	}
	return &Client{base: u, token: token, http: &http.Client{Timeout: requestTimeout}}, nil
}

// Add replaces owner's gw keys with one that gw generates and returns it with its ak_id.
func (c *Client) Add(ctx context.Context, owner string) (string, string, error) {
	if err := c.Revoke(ctx, owner); err != nil {
		return "", "", err
	}
	body := map[string]any{
		"product": product, "tenant": tenant, "owner": owner,
		"qps": qps, "daily_token_quota": dailyTokens, "tokens_per_minute": tokensPerMinute,
	}
	var created struct {
		AK   string `json:"ak"`
		AKID string `json:"ak_id"`
	}
	if _, err := c.request(ctx, http.MethodPost, keysPath, nil, body, &created); err != nil {
		return "", "", err
	}
	if created.AK == "" || created.AKID == "" {
		return "", "", errors.New("POST /admin/keys: no generated key in the response")
	}
	return created.AK, created.AKID, nil
}

// Revoke removes owner's gw keys; revoking an absent key succeeds.
func (c *Client) Revoke(ctx context.Context, owner string) error {
	ids, err := c.keysOf(ctx, owner)
	if err != nil {
		return err
	}
	for _, id := range ids {
		if err := c.delete(ctx, id); err != nil {
			return err
		}
	}
	return nil
}

func (c *Client) SetEnabled(ctx context.Context, owner string, enabled bool) error {
	var expires any = pastExpiry
	if enabled {
		expires = nil
	}
	ids, err := c.keysOf(ctx, owner)
	if err != nil {
		return err
	}
	for _, id := range ids {
		if _, err := c.request(ctx, http.MethodPatch, keysPath+"/"+id, nil, map[string]any{"expires_at_epoch_secs": expires}, nil); err != nil {
			return err
		}
	}
	return nil
}

// Usage returns owner's gw usage by model since since.
func (c *Client) Usage(ctx context.Context, owner string, since time.Time) ([]Usage, error) {
	var resp struct {
		Usage []Usage `json:"usage"`
	}
	query := url.Values{"user": {owner}, "since": {strconv.FormatInt(since.Unix(), 10)}}
	if _, err := c.request(ctx, http.MethodGet, usagePath, query, nil, &resp); err != nil {
		return nil, err
	}
	return resp.Usage, nil
}

// DailyCap returns owner's own daily cost cap in micros or Unlimited; nil means the tenant default.
func (c *Client) DailyCap(ctx context.Context, owner string) (*int64, error) {
	var b budget
	if _, err := c.request(ctx, http.MethodGet, budgetPath+"/"+url.PathEscape(owner)+"/budget", nil, nil, &b); err != nil {
		return nil, err
	}
	return b.dailyCap()
}

// DailyCaps returns the daily cost cap of every owner that has its own.
func (c *Client) DailyCaps(ctx context.Context) (map[string]int64, error) {
	caps := map[string]int64{}
	query := url.Values{"limit": {strconv.Itoa(pageSize)}}
	for {
		var page struct {
			Users []budget `json:"users"`
			Next  string   `json:"next"`
		}
		if _, err := c.request(ctx, http.MethodGet, budgetPath, query, nil, &page); err != nil {
			return nil, err
		}
		for _, b := range page.Users {
			daily, err := b.dailyCap()
			if err != nil {
				return nil, err
			}
			if daily != nil {
				caps[b.User] = *daily
			}
		}
		if page.Next == "" {
			return caps, nil
		}
		query.Set("after", page.Next)
	}
}

// SetDailyCap gives owner its own daily cost cap in micros or Unlimited; nil puts owner back on the tenant default.
func (c *Client) SetDailyCap(ctx context.Context, owner string, micros *int64) error {
	var daily any
	switch {
	case micros == nil:
	case *micros == Unlimited:
		daily = unlimitedWire
	default:
		daily = *micros
	}
	_, err := c.request(ctx, http.MethodPut, budgetPath+"/"+url.PathEscape(owner)+"/budget", nil, map[string]any{"daily_cost_quota_micros": daily}, nil)
	return err
}

func (c *Client) keysOf(ctx context.Context, owner string) ([]string, error) {
	var page struct {
		Keys []struct {
			AKID   string `json:"ak_id"`
			Tenant string `json:"tenant"`
			Owner  string `json:"owner"`
		} `json:"keys"`
	}
	query := url.Values{"tenant": {tenant}, "owner": {owner}, "limit": {strconv.Itoa(pageSize)}}
	if _, err := c.request(ctx, http.MethodGet, keysPath, query, nil, &page); err != nil {
		return nil, err
	}
	var ids []string
	for _, k := range page.Keys {
		if k.Tenant == tenant && k.Owner == owner && k.AKID != "" {
			ids = append(ids, k.AKID)
		}
	}
	return ids, nil
}

func (c *Client) delete(ctx context.Context, id string) error {
	status, err := c.request(ctx, http.MethodDelete, keysPath+"/"+id, nil, nil, nil)
	if status == http.StatusNotFound {
		return nil
	}
	return err
}

// request calls gw's admin API at path.
func (c *Client) request(ctx context.Context, method, path string, query url.Values, body, out any) (int, error) {
	op := method + " " + path
	var payload io.Reader
	if body != nil {
		raw, err := json.Marshal(body)
		if err != nil {
			return 0, fmt.Errorf("encode %s: %w", op, err)
		}
		payload = bytes.NewReader(raw)
	}
	u := c.base.JoinPath(path)
	u.RawQuery = query.Encode()
	req, err := http.NewRequestWithContext(ctx, method, u.String(), payload)
	if err != nil {
		return 0, fmt.Errorf("build %s: %w", op, err)
	}
	req.Header.Set("Authorization", "Bearer "+c.token)
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := c.http.Do(req)
	if err != nil {
		if urlErr, ok := errors.AsType[*url.Error](err); ok {
			err = urlErr.Err
		}
		return 0, fmt.Errorf("%s: %w", op, err)
	}
	defer func() { _ = resp.Body.Close() }()
	if resp.StatusCode/100 != 2 {
		var e struct {
			Error struct {
				Message string `json:"message"`
			} `json:"error"`
		}
		raw, _ := io.ReadAll(io.LimitReader(resp.Body, maxErrorBody))
		if json.Unmarshal(raw, &e) != nil || e.Error.Message == "" {
			e.Error.Message = http.StatusText(resp.StatusCode)
		}
		return resp.StatusCode, fmt.Errorf("%s: %d %s", op, resp.StatusCode, e.Error.Message)
	}
	if out == nil {
		return resp.StatusCode, nil
	}
	if err := json.NewDecoder(resp.Body).Decode(out); err != nil {
		return resp.StatusCode, fmt.Errorf("decode %s: %w", op, err)
	}
	return resp.StatusCode, nil
}

type budget struct {
	User  string          `json:"user"`
	Daily json.RawMessage `json:"daily_cost_quota_micros"`
}

func (b budget) dailyCap() (*int64, error) {
	if len(b.Daily) == 0 || string(b.Daily) == "null" {
		return nil, nil
	}
	var micros int64
	if json.Unmarshal(b.Daily, &micros) == nil {
		return &micros, nil
	}
	var word string
	if json.Unmarshal(b.Daily, &word) == nil && word == unlimitedWire {
		return new(int64(Unlimited)), nil
	}
	return nil, fmt.Errorf("daily cap of %s: unexpected %s", b.User, b.Daily)
}
