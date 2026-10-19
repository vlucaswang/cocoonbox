// Package sandboxd runs machines as claims of a cocoonbox sandboxd instance.
package sandboxd

import (
	"cmp"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"net"
	"net/http"
	"reflect"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/projecteru2/core/log"
	"golang.org/x/sync/errgroup"

	"github.com/loomhand/cocoonbox/api/cluster"
	"github.com/loomhand/cocoonbox/api/machines"
)

const (
	netNone                = "none"
	leaseSeconds           = 3600
	onExpire               = "archive"
	identityTimeoutSeconds = 30
	screenPort             = 5900

	refreshInterval = 2 * time.Second
	renewInterval   = 5 * time.Minute
	watchInterval   = 500 * time.Millisecond
	opTimeout       = 10 * time.Minute
	probeTimeout    = 2 * time.Second
	probeFanout     = 8
	sharedTimeout   = 10 * time.Second
)

var (
	metaKey = regexp.MustCompile(`^[A-Za-z0-9_]{1,64}$`)

	writeIdentity = []string{"sh", "-c", `set -e; umask 022; mkdir -p /run/cocoonbox-identity; chmod 0755 /run/cocoonbox-identity
t=$(mktemp /run/cocoonbox-identity/.identity.XXXXXX); printf '%s' "$CB_STDIN" > "$t"; chmod 0644 "$t"; mv -f "$t" /run/cocoonbox-identity/identity`}
)

// Options names the sandboxd instance and the claims the backend makes on it.
type Options struct {
	URL string
	// Token is the instance's root api token.
	Token string
	// Image is the default template.
	Image string
	// Flavors lists every flavor Ensure may request; each must be a sandboxd tier.
	Flavors []machines.Flavor
	// DiskGiB is the disk the node gives each claim.
	DiskGiB           uint32
	AgentPort         int
	TenantEgressClass string
	Ledger            *cluster.Store
	// Shared, when set, holds the machine records: every change lands on a row's version and the backend caches the table.
	Shared *cluster.Store
}

// Check reports whether New refuses opts before it reads the instance.
func (o Options) Check() error {
	_, err := o.client()
	return err
}

func (o Options) client() (*client, error) {
	for _, f := range o.Flavors {
		if _, ok := sizeOf(f); !ok {
			return nil, fmt.Errorf("flavor vcpus:%d memory_mib:%d is not a sandboxd tier", f.VCPUs, f.MemoryMiB)
		}
	}
	return newClient(o.URL, o.Token)
}

var (
	_ machines.Machines = (*Backend)(nil)
	_ machines.Tenants  = (*Backend)(nil)
)

// Backend implements machines.Machines and machines.Tenants on one sandboxd instance that holds only cocoonbox claims.
type Backend struct {
	*Tenants

	opts                 Options
	api                  *client
	ctx                  context.Context
	mirror               *cluster.Mirror
	leaseFor, renewEvery time.Duration

	mu      sync.Mutex
	entries map[string]*entry
	owners  map[string]chan struct{}
	views   map[string]view
}

// New checks opts, reads the instance's claims and keeps them indexed until ctx ends.
func New(ctx context.Context, opts Options) (*Backend, error) {
	api, err := opts.client()
	if err != nil {
		return nil, err
	}
	b := &Backend{
		Tenants: &Tenants{api: api, egressClass: opts.TenantEgressClass},
		opts:    opts,
		api:     api,
		ctx:     ctx,
		entries: map[string]*entry{},
		owners:  map[string]chan struct{}{},
		views:   map[string]view{},

		leaseFor: cluster.Lease, renewEvery: cluster.LeaseRenew,
	}
	switch {
	case opts.Shared != nil:
		if err := b.reload(ctx, ""); err != nil {
			return nil, fmt.Errorf("read the shared machines: %w", err)
		}
		go opts.Shared.Listen(ctx, cluster.ChannelMachines, func() {
			if err := b.reload(ctx, ""); err != nil {
				log.WithFunc("sandboxd.New").Warnf(ctx, "reload the shared machines: %v", err)
			}
		}, func(payload string) { b.changed(ctx, payload) })
	case opts.Ledger != nil:
		b.mirror = cluster.NewMirror()
		go b.mirror.Run(ctx)
	}
	if err := b.refresh(ctx); err != nil {
		return nil, fmt.Errorf("read sandboxd claims: %w", err)
	}
	if opts.Shared == nil {
		go b.Run(ctx)
	}
	return b, nil
}

// Run re-reads the claims and renews their leases until ctx ends; New runs it unless a shared table lets one api do it for all.
func (b *Backend) Run(ctx context.Context) {
	var wg sync.WaitGroup
	wg.Go(func() { b.loop(ctx, refreshInterval, b.refresh) })
	wg.Go(func() { b.loop(ctx, renewInterval, b.renew) })
	wg.Wait()
}

func (b *Backend) Ensure(ctx context.Context, spec machines.Spec) (machines.Machine, machines.Action, error) {
	size, ok := sizeOf(spec.Flavor)
	if !ok {
		return machines.Machine{}, machines.ActionNone, fmt.Errorf("flavor vcpus:%d memory_mib:%d is not a sandboxd tier", spec.Flavor.VCPUs, spec.Flavor.MemoryMiB)
	}
	if _, err := b.lookup(ctx, spec.ID); err != nil && !errors.Is(err, machines.ErrNotFound) {
		return machines.Machine{}, machines.ActionNone, err
	}
	template := cmp.Or(spec.Image, b.opts.Image)
	action, sandbox := machines.ActionNone, ""
	m, err := b.mutate(ctx, spec.ID, func(e *entry) (*entry, bool) {
		now := time.Now()
		action = machines.ActionNone
		switch {
		case e == nil || (e.row == nil && !e.busy(now)):
			e = &entry{id: spec.ID, owner: spec.Owner, image: template, created: now, meta: map[string]string{}}
			e.begin(opClaim, now, b.lease())
			action = machines.ActionCreated
			return e, true
		case e.busy(now):
			return e, false
		}
		if state, _ := e.state(); state == machines.StateHibernated || state == machines.StateFailed {
			sandbox = e.row.ID
			e.begin(opWake, now, b.lease())
			action = machines.ActionWoken
			return e, true
		}
		return e, false
	})
	if err != nil {
		return machines.Machine{}, machines.ActionNone, err
	}
	switch action {
	case machines.ActionCreated:
		b.run(spec.ID, opClaim, m.Revision, func(ctx context.Context) error { return b.claim(ctx, spec.ID, spec.Owner, spec.Tenant, template, size) })
	case machines.ActionWoken:
		b.run(spec.ID, opWake, m.Revision, func(ctx context.Context) error { return b.wake(ctx, spec.ID, sandbox) })
	}
	return m, action, nil
}

func (b *Backend) Get(ctx context.Context, id string) (machines.Machine, error) {
	return b.lookup(ctx, id)
}

func (b *Backend) List(_ context.Context, owner string) ([]machines.Machine, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	var out []machines.Machine
	for _, e := range b.entries {
		if owner == "" || e.owner == owner {
			out = append(out, e.machine(b.opts.DiskGiB))
		}
	}
	slices.SortFunc(out, func(x, y machines.Machine) int { return cmp.Compare(x.ID, y.ID) })
	return out, nil
}

func (b *Backend) Hibernate(ctx context.Context, id string) (machines.Machine, error) {
	if _, err := b.lookup(ctx, id); err != nil {
		return machines.Machine{}, err
	}
	started, missing, sandbox := false, false, ""
	m, err := b.mutate(ctx, id, func(e *entry) (*entry, bool) {
		started, missing = false, e == nil || e.row == nil
		if missing {
			return e, false
		}
		now := time.Now()
		if state, _ := e.state(); state == machines.StateRunning || state == machines.StateStarting && !e.busy(now) {
			sandbox, started = e.row.ID, true
			e.begin(opHibernate, now, b.lease())
			return e, true
		}
		return e, false
	})
	switch {
	case err != nil:
		return machines.Machine{}, err
	case missing:
		return machines.Machine{}, machines.ErrNotFound
	}
	if started {
		b.run(id, opHibernate, m.Revision, func(ctx context.Context) error {
			if err := b.api.verb(ctx, sandbox, "hibernate", nil); err != nil {
				return err
			}
			return b.reread(ctx, id, sandbox)
		})
	}
	return m, nil
}

func (b *Backend) Watch(ctx context.Context, id string, revision int64) <-chan machines.Machine {
	out := make(chan machines.Machine)
	go func() {
		defer close(out)
		ticker := time.NewTicker(watchInterval)
		defer ticker.Stop()
		seen, last := false, ""
		for {
			changed := b.changes(id)
			m, err := b.lookup(ctx, id)
			switch {
			case errors.Is(err, machines.ErrNotFound) && seen:
				send(ctx, out, machines.Machine{ID: id, State: machines.StateGone})
				return
			case err == nil:
				seen = true
				b.probe(ctx, id)
				if m, err = b.cached(id); err == nil && m.Revision >= revision {
					if key := fmt.Sprintf("%d|%s|%s|%d|%v", m.State, m.Reason, m.Incarnation, m.Revision, m.Meta); key != last {
						if !send(ctx, out, m) {
							return
						}
						last = key
					}
				}
			}
			select {
			case <-ticker.C:
			case <-changed:
			case <-ctx.Done():
				return
			}
		}
	}()
	return out
}

func (b *Backend) Changes(owner string) <-chan struct{} {
	b.mu.Lock()
	defer b.mu.Unlock()
	ch := b.owners[owner]
	if ch == nil {
		ch = make(chan struct{})
		b.owners[owner] = ch
	}
	return ch
}

func (b *Backend) Address(_ context.Context, id string) (machines.Endpoint, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	e := b.entries[id]
	if e == nil {
		return machines.Endpoint{}, machines.ErrNotFound
	}
	if state, _ := e.state(); state != machines.StateRunning {
		return machines.Endpoint{}, &machines.NotRunningError{State: state}
	}
	sandbox := e.row.ID
	key := net.JoinHostPort(sandbox+".sandboxd", strconv.Itoa(b.opts.AgentPort))
	return machines.Endpoint{
		Key: key,
		Dial: func(ctx context.Context) (net.Conn, error) {
			return b.api.dialPort(ctx, sandbox, b.opts.AgentPort)
		},
		Screen: func(ctx context.Context) (net.Conn, error) {
			return b.api.dialPort(ctx, sandbox, screenPort)
		},
	}, nil
}

func (b *Backend) Delete(ctx context.Context, id string) error {
	now := time.Now()
	unclaimed, err := b.drop(ctx, id, func(e *entry) bool { return e.row == nil && !e.busy(now) })
	if err != nil {
		return err
	}
	sandbox, err := b.sandboxOf(ctx, id)
	if unclaimed && errors.Is(err, machines.ErrNotFound) {
		return nil
	}
	if err != nil {
		return err
	}
	if err = b.api.verb(ctx, sandbox, "release", nil); err != nil {
		return err
	}
	_, err = b.drop(ctx, id, func(*entry) bool { return true })
	return err
}

func (b *Backend) SetMeta(ctx context.Context, id, key, value string) error {
	if !metaKey.MatchString(key) {
		return fmt.Errorf("meta key %q must match %s", key, metaKey)
	}
	sandbox, err := b.sandboxOf(ctx, id)
	if err != nil {
		return err
	}
	if err = b.api.patchEnv(ctx, sandbox, map[string]*envVar{envMetaPrefix + key: {Value: value}}); err != nil {
		return fmt.Errorf("set meta %s: %w", key, err)
	}
	if _, err = b.mutate(ctx, id, func(e *entry) (*entry, bool) {
		if e == nil || e.meta[key] == value {
			return e, false
		}
		e.meta[key] = value
		return e, true
	}); err != nil {
		return err
	}
	b.notify(id)
	return nil
}

func (b *Backend) Deliver(ctx context.Context, id string, identity machines.Identity, keys map[string]*machines.Secret) error {
	sandbox, err := b.sandboxOf(ctx, id)
	if err != nil {
		return err
	}
	m, err := b.cached(id)
	if err != nil {
		return err
	}
	if m.State != machines.StateRunning {
		return &machines.NotRunningError{State: m.State}
	}
	if len(keys) > 0 {
		vars := make(map[string]*envVar, len(keys))
		for name, sec := range keys {
			vars[name] = secretVar(sec)
		}
		if err = b.api.patchEnv(ctx, sandbox, vars); err != nil {
			return fmt.Errorf("install host keys: %w", err)
		}
	}
	file, err := json.Marshal(identity)
	if err != nil {
		return fmt.Errorf("encode identity: %w", err)
	}
	r, err := b.api.get(ctx, sandbox)
	if err != nil {
		return fmt.Errorf("read sandbox token: %w", err)
	}
	resp, err := b.api.exec(ctx, sandbox, r.Token, execRequest{Argv: writeIdentity, Env: map[string]string{"CB_STDIN": string(file)}, TimeoutSeconds: identityTimeoutSeconds})
	switch {
	case err != nil:
		return fmt.Errorf("write identity: %w", err)
	case resp.ExitCode != 0:
		return fmt.Errorf("write identity: exit %d: %s", resp.ExitCode, resp.Stderr)
	}
	return nil
}

func (b *Backend) SetEgress(ctx context.Context, id, upstream string) error {
	sandbox, err := b.sandboxOf(ctx, id)
	if err != nil {
		return err
	}
	var v *envVar
	if upstream != "" {
		v = &envVar{Value: upstream, Guest: new(false)}
	}
	if err = b.api.patchEnv(ctx, sandbox, map[string]*envVar{envUpstream: v}); err != nil {
		return fmt.Errorf("set egress upstream: %w", err)
	}
	_, err = b.mutate(ctx, id, func(e *entry) (*entry, bool) {
		if e == nil || e.routed == (upstream != "") {
			return e, false
		}
		e.routed = upstream != ""
		return e, true
	})
	return err
}

func (b *Backend) SetCredentials(ctx context.Context, id string, c machines.Credentials) error {
	sandbox, err := b.sandboxOf(ctx, id)
	if err != nil {
		return err
	}
	b.mu.Lock()
	e := b.entries[id]
	if e == nil {
		b.mu.Unlock()
		return machines.ErrNotFound
	}
	changes, sums, had := map[string]*envVar{}, map[string]string{}, map[string]string{}
	add := func(name string, v *envVar) {
		sum := fingerprint(v)
		have, ok := e.env[name]
		if (v == nil && !ok) || (v != nil && ok && have == sum) {
			return
		}
		changes[name], sums[name], had[name] = v, sum, have
	}
	for name, sec := range c.Secrets {
		add(name, secretVar(sec))
	}
	if c.Prune != "" {
		for name := range e.env {
			if _, ok := c.Secrets[name]; !ok && strings.HasPrefix(name, c.Prune) {
				add(name, nil)
			}
		}
	}
	if state, _ := e.state(); state == machines.StateRunning {
		for name, value := range c.Flags {
			var v *envVar
			if value != "" {
				v = &envVar{Value: value}
			}
			add(name, v)
		}
	}
	b.mu.Unlock()
	if len(changes) == 0 {
		return nil
	}
	err = b.api.patchEnv(ctx, sandbox, changes)
	if err != nil && !errors.Is(err, machines.ErrRefused) {
		return fmt.Errorf("set credentials: %w", err)
	}
	// A refused write counts as written, so a pool that cannot inject is retried only when the values change.
	if _, merr := b.mutate(ctx, id, func(cur *entry) (*entry, bool) {
		if cur == nil {
			return cur, false
		}
		if cur.env == nil {
			cur.env = map[string]string{}
		}
		for name, sum := range sums {
			switch {
			case cur.env[name] != had[name]:
				// Another process wrote the name since this one read it, so the value there is unknown.
				cur.env[name] = ""
			case changes[name] == nil:
				delete(cur.env, name)
			default:
				cur.env[name] = sum
			}
		}
		return cur, true
	}); merr != nil {
		return fmt.Errorf("record credentials: %w", merr)
	}
	if err != nil {
		return fmt.Errorf("set credentials: %w", err)
	}
	return nil
}

// Verify returns the machines whose record differs from the table once queued writes landed, repairing the row or, shared, the cache.
func (b *Backend) Verify(ctx context.Context) ([]string, error) {
	if b.opts.Shared != nil {
		differ, err := b.differences(ctx)
		for _, id := range differ {
			b.notified(ctx, id)
		}
		return differ, err
	}
	differ, err := b.mirror.Verify(ctx, b.differences)
	if err != nil {
		return nil, err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	for _, id := range differ {
		if e := b.entries[id]; e != nil {
			e.mirrored = nil
			b.touchLocked(e)
		} else {
			b.dropLocked(id)
		}
	}
	return differ, nil
}

// SyncOwner reads owner's machines from the shared table into the cache, for a start that must not act on a stale one.
func (b *Backend) SyncOwner(ctx context.Context, owner string) error {
	return b.reload(ctx, owner)
}

func (b *Backend) mutate(ctx context.Context, id string, fn func(e *entry) (*entry, bool)) (machines.Machine, error) {
	if b.opts.Shared == nil {
		b.mu.Lock()
		defer b.mu.Unlock()
		sandbox := b.entries[id].sandbox()
		next, changed := fn(b.entries[id])
		if next == nil {
			return machines.Machine{}, nil
		}
		if changed {
			if next.sandbox() != sandbox {
				next.rowAt = time.Now()
			}
			b.entries[id] = next
			b.touchLocked(next)
		}
		return next.machine(b.opts.DiskGiB), nil
	}
	// A write that committed must reach the cache even when the caller left.
	ctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), sharedTimeout)
	defer cancel()
	var kept *entry
	rec, err := b.opts.Shared.UpdateMachine(ctx, id, func(m *cluster.Machine, found bool) (bool, error) {
		var cur *entry
		if found {
			cur = &entry{id: id}
			cur.load(*m)
		}
		next, changed := fn(cur)
		kept = next
		if next == nil || !changed {
			return false, nil
		}
		rec := next.record()
		if rec.Equal(*m) {
			return false, nil
		}
		rec.Version = m.Version
		*m = rec
		return true, nil
	})
	if err != nil {
		return machines.Machine{}, err
	}
	if kept == nil {
		return machines.Machine{}, nil
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	e := b.loadLocked(rec)
	b.publishLocked()
	return e.machine(b.opts.DiskGiB), nil
}

// drop removes machine id while cond holds for its entry and reports whether it did.
func (b *Backend) drop(ctx context.Context, id string, cond func(e *entry) bool) (bool, error) {
	if b.opts.Shared == nil {
		b.mu.Lock()
		defer b.mu.Unlock()
		if e := b.entries[id]; e != nil && cond(e) {
			b.dropLocked(id)
			return true, nil
		}
		return false, nil
	}
	ctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), sharedTimeout)
	defer cancel()
	removed, err := b.opts.Shared.DeleteMachineIf(ctx, id, func(m cluster.Machine) bool {
		e := &entry{id: id}
		e.load(m)
		return cond(e)
	})
	if (err != nil && !errors.Is(err, cluster.ErrNotFound)) || (err == nil && !removed) {
		return false, err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	_, cached := b.entries[id]
	delete(b.entries, id)
	b.publishLocked()
	return removed || cached, nil
}

// run runs fn as op of machine id, begun at revision, off the caller's request, and ends the op when fn returns.
func (b *Backend) run(id, op string, revision int64, fn func(context.Context) error) {
	go func() {
		ctx, cancel := context.WithTimeout(b.ctx, opTimeout)
		defer cancel()
		logger := log.WithFunc("sandboxd.Backend.run").WithField("machine", id)
		if b.opts.Shared != nil {
			go b.renewOp(ctx, cancel, id, op, revision)
		}
		err := fn(ctx)
		if err != nil {
			logger.Error(ctx, err, op)
		}
		opErr := ""
		switch {
		case b.ctx.Err() != nil:
			// The process stops; the claim row tells the next reader how far the op came.
		case errors.Is(err, machines.ErrCapacity):
			opErr = machines.ErrCapacity.Error()
		case err != nil:
			opErr = op + " failed"
		}
		if _, err := b.mutate(ctx, id, func(e *entry) (*entry, bool) {
			if e == nil || e.op != op || e.revision != revision {
				return e, false
			}
			e.op, e.opErr, e.opUntil = "", opErr, time.Time{}
			return e, true
		}); err != nil {
			logger.Errorf(ctx, err, "end %s", op)
		}
		b.notify(id)
	}()
}

// renewOp extends the lease of op on machine id every renewEvery until ctx ends or the op does, and stops the op through stop before a lease it failed to renew runs out.
func (b *Backend) renewOp(ctx context.Context, stop context.CancelFunc, id, op string, revision int64) {
	logger := log.WithFunc("sandboxd.Backend.renewOp").WithField("machine", id)
	until := time.Now().Add(b.leaseFor)
	ticker := time.NewTicker(b.renewEvery)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
		running, next := false, time.Now().Add(b.leaseFor)
		_, err := b.mutate(ctx, id, func(e *entry) (*entry, bool) {
			if running = e != nil && e.op == op && e.revision == revision; !running {
				return e, false
			}
			e.opUntil = next
			return e, true
		})
		switch {
		case err == nil && !running:
			return
		case err == nil:
			until = next
		case !time.Now().Add(b.renewEvery).Before(until):
			logger.Errorf(ctx, err, "stop %s, whose lease runs out unrenewed", op)
			stop()
			return
		default:
			logger.Warnf(ctx, "renew the lease of %s: %v", op, err)
		}
	}
}

// lease is how long a new op holds its machine before its process must renew it; zero, for ever, in memory.
func (b *Backend) lease() time.Duration {
	if b.opts.Shared == nil {
		return 0
	}
	return b.leaseFor
}

func (b *Backend) claim(ctx context.Context, id, owner, tenant, template, size string) error {
	sandbox, err := b.api.claim(ctx, tenant, claimRequest{
		Template:   template,
		Net:        netNone,
		Size:       size,
		TTLSeconds: leaseSeconds,
		OnExpire:   onExpire,
		Metadata:   map[string]string{metaMachine: id, metaOwner: owner},
	})
	if err != nil {
		return err
	}
	return b.reread(ctx, id, sandbox)
}

func (b *Backend) wake(ctx context.Context, id, sandbox string) error {
	if err := b.api.verb(ctx, sandbox, "wake", nil); err != nil {
		return err
	}
	b.mu.Lock()
	wakes := 1
	if e := b.entries[id]; e != nil {
		wakes = e.wakes + 1
	}
	b.mu.Unlock()
	if err := b.api.patchEnv(ctx, sandbox, map[string]*envVar{envWakes: {Value: strconv.Itoa(wakes)}}); err != nil {
		return fmt.Errorf("record wake: %w", err)
	}
	if _, err := b.mutate(ctx, id, func(e *entry) (*entry, bool) {
		if e == nil {
			return e, false
		}
		e.wakes, e.ready = wakes, false
		return e, true
	}); err != nil {
		return err
	}
	return b.reread(ctx, id, sandbox)
}

// reread replaces machine id's row with sandbox's current one.
func (b *Backend) reread(ctx context.Context, id, sandbox string) error {
	r, err := b.api.get(ctx, sandbox)
	if err != nil {
		return err
	}
	r.Token = ""
	_, err = b.mutate(ctx, id, func(e *entry) (*entry, bool) {
		if e == nil {
			return e, false
		}
		e.row = &r
		return e, true
	})
	return err
}

// lookup refreshes id's row from sandboxd, finding it by metadata when the index lacks it.
func (b *Backend) lookup(ctx context.Context, id string) (machines.Machine, error) {
	b.mu.Lock()
	e := b.entries[id]
	var sandbox string
	if e != nil && e.row != nil {
		sandbox = e.row.ID
	}
	b.mu.Unlock()
	if e != nil && sandbox == "" {
		return b.cached(id)
	}
	var r row
	var err error
	if sandbox != "" {
		r, err = b.api.get(ctx, sandbox)
	} else {
		var rows []row
		if rows, err = b.api.find(ctx, metaMachine, id); err == nil && len(rows) == 0 {
			err = machines.ErrNotFound
		} else if err == nil {
			r = rows[0]
		}
	}
	if errors.Is(err, machines.ErrNotFound) {
		if err = b.forget(ctx, id, sandbox); err != nil {
			return machines.Machine{}, err
		}
		return machines.Machine{}, machines.ErrNotFound
	}
	if err != nil {
		return machines.Machine{}, err
	}
	r.Token = ""
	if err := b.index(ctx, []row{r}, time.Time{}); err != nil {
		return machines.Machine{}, err
	}
	return b.cached(id)
}

func (b *Backend) cached(id string) (machines.Machine, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	e := b.entries[id]
	if e == nil {
		return machines.Machine{}, machines.ErrNotFound
	}
	return e.machine(b.opts.DiskGiB), nil
}

func (b *Backend) notify(id string) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if e := b.entries[id]; e != nil {
		e.notifyLocked()
	}
}

func (b *Backend) changes(id string) <-chan struct{} {
	b.mu.Lock()
	defer b.mu.Unlock()
	e := b.entries[id]
	if e == nil {
		return nil
	}
	if e.changed == nil {
		e.changed = make(chan struct{})
	}
	return e.changed
}

func (b *Backend) sandboxOf(ctx context.Context, id string) (string, error) {
	if _, err := b.lookup(ctx, id); err != nil {
		return "", err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if e := b.entries[id]; e != nil && e.row != nil {
		return e.row.ID, nil
	}
	return "", machines.ErrNotFound
}

// forget drops id unless an operation still owns it.
func (b *Backend) forget(ctx context.Context, id, sandbox string) error {
	now := time.Now()
	_, err := b.drop(ctx, id, func(e *entry) bool { return !e.busy(now) && (e.row == nil || e.row.ID == sandbox) })
	return err
}

// refresh re-reads every claim, drops machines whose claim is gone and probes agents not yet seen.
func (b *Backend) refresh(ctx context.Context) error {
	listed := time.Now()
	rows, err := b.api.list(ctx)
	if err != nil {
		return err
	}
	if err := b.index(ctx, rows, listed); err != nil {
		return err
	}
	now := time.Now()
	b.mu.Lock()
	var unready []string
	for id, e := range b.entries {
		if state, _ := e.state(); state == machines.StateStarting && !e.busy(now) && e.row != nil {
			unready = append(unready, id)
		}
	}
	b.mu.Unlock()
	var g errgroup.Group
	g.SetLimit(probeFanout)
	for _, id := range unready {
		g.Go(func() error {
			b.probe(ctx, id)
			return nil
		})
	}
	_ = g.Wait()
	b.mu.Lock()
	b.publishLocked()
	b.mu.Unlock()
	return nil
}

// index merges rows, reading the env of unseen claims; a full list taken at listed, not zero, also drops what it lacks.
func (b *Backend) index(ctx context.Context, rows []row, listed time.Time) error {
	present := map[string]bool{}
	for _, r := range rows {
		id := r.Metadata[metaMachine]
		if id == "" {
			continue
		}
		present[id] = true
		b.mu.Lock()
		e := b.entries[id]
		known := e != nil && e.row != nil && e.row.ID == r.ID
		unchanged := known && reflect.DeepEqual(*e.row, r) && e.owner == cmp.Or(r.Tenant, e.owner) && e.image == r.Key.Template
		b.mu.Unlock()
		if unchanged {
			continue
		}
		var env map[string]envVar
		if !known {
			var err error
			if env, err = b.api.env(ctx, r.ID); err != nil {
				return fmt.Errorf("read env of %s: %w", r.ID, err)
			}
		}
		if _, err := b.mutate(ctx, id, func(e *entry) (*entry, bool) {
			e = cmp.Or(e, &entry{id: id, owner: r.Metadata[metaOwner]})
			// The tenant stamp is sandboxd's own, so it outranks the owner metadata the api wrote.
			e.owner = cmp.Or(r.Tenant, e.owner)
			if env != nil && (e.row == nil || e.row.ID != r.ID) {
				e.loadEnv(env)
			}
			cur := r
			e.row, e.image = &cur, r.Key.Template
			return e, true
		}); err != nil {
			return err
		}
	}
	if listed.IsZero() {
		return nil
	}
	now := time.Now()
	b.mu.Lock()
	gone := map[string]string{}
	for id, e := range b.entries {
		if !present[id] && !e.busy(now) && e.row != nil && e.rowAt.Before(listed) {
			gone[id] = e.row.ID
		}
	}
	b.mu.Unlock()
	for id, sandbox := range gone {
		if _, err := b.drop(ctx, id, func(e *entry) bool { return !e.busy(now) && e.row != nil && e.row.ID == sandbox }); err != nil {
			return err
		}
	}
	return nil
}

// publishLocked wakes the owners of the machines whose state, image, flavor, incarnation or meta changed since the last call, or that went away.
func (b *Backend) publishLocked() {
	views := make(map[string]view, len(b.entries))
	for id, e := range b.entries {
		m := e.machine(b.opts.DiskGiB)
		views[id] = view{owner: m.Owner, key: fmt.Sprintf("%d|%s|%s|%v|%s|%v", m.State, m.Reason, m.Image, m.Flavor, m.Incarnation, m.Meta)}
		if b.views[id] != views[id] {
			b.wakeLocked(m.Owner)
		}
	}
	for id, v := range b.views {
		if _, ok := views[id]; !ok {
			b.wakeLocked(v.owner)
		}
	}
	b.views = views
}

func (b *Backend) wakeLocked(owner string) {
	if ch := b.owners[owner]; ch != nil {
		close(ch)
		delete(b.owners, owner)
	}
}

// probe marks id ready once its agent answers through the relay.
func (b *Backend) probe(ctx context.Context, id string) {
	b.mu.Lock()
	e := b.entries[id]
	if e == nil || e.row == nil || e.busy(time.Now()) || e.row.Hibernated || e.row.Archived || e.row.Failed != "" ||
		(e.ready && e.readyRestarts == e.row.Restarts) {
		b.mu.Unlock()
		return
	}
	sandbox, restarts := e.row.ID, e.row.Restarts
	b.mu.Unlock()
	ctx, cancel := context.WithTimeout(ctx, probeTimeout)
	defer cancel()
	conn, err := b.api.dialPort(ctx, sandbox, b.opts.AgentPort)
	if err != nil {
		return
	}
	_ = conn.Close()
	if _, err := b.mutate(ctx, id, func(e *entry) (*entry, bool) {
		if e == nil || e.row == nil || e.row.ID != sandbox || e.row.Restarts != restarts {
			return e, false
		}
		e.ready, e.readyRestarts = true, restarts
		return e, true
	}); err != nil {
		log.WithFunc("sandboxd.Backend.probe").WithField("machine", id).Warnf(ctx, "record the agent ready: %v", err)
	}
}

// renew extends the lease of every claim that is not archived.
func (b *Backend) renew(ctx context.Context) error {
	b.mu.Lock()
	var sandboxes []string
	for _, e := range b.entries {
		if e.row != nil && !e.row.Archived {
			sandboxes = append(sandboxes, e.row.ID)
		}
	}
	b.mu.Unlock()
	var errs []error
	for _, sandbox := range sandboxes {
		if err := b.api.verb(ctx, sandbox, "renew", renewRequest{TTLSeconds: leaseSeconds, OnExpire: onExpire}); err != nil && !isStatus(err, http.StatusConflict) {
			errs = append(errs, err)
		}
	}
	return errors.Join(errs...)
}

// reload reads the shared table into the cache, only owner's machines unless owner is empty.
func (b *Backend) reload(ctx context.Context, owner string) error {
	var recs []cluster.Machine
	var err error
	if owner == "" {
		recs, err = b.opts.Shared.Machines(ctx)
	} else {
		recs, err = b.opts.Shared.MachinesOf(ctx, owner)
	}
	if err != nil {
		return err
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	present := make(map[string]bool, len(recs))
	for _, rec := range recs {
		present[rec.ID] = true
		b.loadLocked(rec)
	}
	maps.DeleteFunc(b.entries, func(id string, e *entry) bool { return (owner == "" || e.owner == owner) && !present[id] })
	b.publishLocked()
	return nil
}

func (b *Backend) changed(ctx context.Context, payload string) {
	c, err := cluster.ParseChange(payload)
	if err != nil {
		log.WithFunc("sandboxd.Backend.changed").Warnf(ctx, "take a machines notify: %v", err)
		return
	}
	b.mu.Lock()
	e := b.entries[c.ID]
	current := e != nil && c.Version != 0 && e.version >= c.Version
	b.mu.Unlock()
	if !current {
		b.notified(ctx, c.ID)
	}
}

// notified takes the shared row of machine id into the cache after a notify.
func (b *Backend) notified(ctx context.Context, id string) {
	rec, err := b.opts.Shared.Machine(ctx, id)
	if err != nil && !errors.Is(err, cluster.ErrNotFound) {
		log.WithFunc("sandboxd.Backend.notified").WithField("machine", id).Warnf(ctx, "read the shared machine: %v", err)
		return
	}
	b.mu.Lock()
	defer b.mu.Unlock()
	if err != nil {
		delete(b.entries, id)
	} else {
		b.loadLocked(rec)
	}
	b.publishLocked()
}

// loadLocked takes rec into the cache unless the cache holds a newer version, and returns the cached entry.
func (b *Backend) loadLocked(rec cluster.Machine) *entry {
	e := b.entries[rec.ID]
	if e == nil {
		e = &entry{id: rec.ID}
		b.entries[rec.ID] = e
	}
	if rec.Version >= e.version {
		busy, opErr, meta, sandbox := e.op != "", e.opErr, e.meta, e.sandbox()
		e.load(rec)
		if e.sandbox() != sandbox {
			e.rowAt = time.Now()
		}
		// Like a change in memory, only an op end, a failure or a meta change wakes the watchers before their next poll.
		if (busy && e.op == "") || opErr != e.opErr || !maps.Equal(meta, e.meta) {
			e.notifyLocked()
		}
	}
	return e
}

// touchLocked queues e's record for the ledger when it changed since the last one queued.
func (b *Backend) touchLocked(e *entry) {
	if b.mirror == nil || b.entries[e.id] != e {
		return
	}
	rec := e.record()
	if e.mirrored != nil && e.mirrored.Equal(rec) {
		return
	}
	e.mirrored = &rec
	b.mirror.Put(e.id, func(ctx context.Context) error { return b.opts.Ledger.PutMachine(ctx, rec) })
}

// dropLocked forgets machine id and queues its removal from the ledger.
func (b *Backend) dropLocked(id string) {
	delete(b.entries, id)
	if b.mirror != nil {
		b.mirror.Put(id, func(ctx context.Context) error { return b.opts.Ledger.DeleteMachine(ctx, id) })
	}
}

// differences returns the ids whose table row differs from the entry.
func (b *Backend) differences(ctx context.Context) ([]string, error) {
	var table func(context.Context) ([]cluster.Machine, error)
	if b.mirror != nil {
		table = b.opts.Ledger.Machines
	} else {
		table = b.opts.Shared.Machines
	}
	b.mu.Lock()
	entries := make([]entry, 0, len(b.entries))
	for _, e := range b.entries {
		copied := *e
		copied.meta, copied.env = maps.Clone(e.meta), maps.Clone(e.env)
		entries = append(entries, copied)
	}
	b.mu.Unlock()
	want := make(map[string]cluster.Machine, len(entries))
	for _, e := range entries {
		want[e.id] = e.record()
	}
	rows, err := table(ctx)
	if err != nil {
		return nil, err
	}
	have := make(map[string]cluster.Machine, len(rows))
	for _, r := range rows {
		have[r.ID] = r
	}
	ids := slices.Collect(maps.Keys(want))
	for id := range have {
		if _, ok := want[id]; !ok {
			ids = append(ids, id)
		}
	}
	slices.Sort(ids)
	var differ []string
	for _, id := range ids {
		w, inMemory := want[id]
		h, inTable := have[id]
		if inMemory != inTable || (inMemory && !w.Equal(h)) {
			differ = append(differ, id)
		}
	}
	return differ, nil
}

func (b *Backend) loop(ctx context.Context, interval time.Duration, fn func(context.Context) error) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
		callCtx, cancel := context.WithTimeout(ctx, requestTimeout)
		if err := fn(callCtx); err != nil {
			log.WithFunc("sandboxd.Backend.loop").Warnf(callCtx, "every %s: %v", interval, err)
		}
		cancel()
	}
}

func send(ctx context.Context, out chan<- machines.Machine, m machines.Machine) bool {
	select {
	case out <- m:
		return true
	case <-ctx.Done():
		return false
	}
}

func secretVar(sec *machines.Secret) *envVar {
	if sec == nil {
		return nil
	}
	v := &envVar{Value: sec.Value, Guest: new(false)}
	if len(sec.Hosts) > 0 {
		v.Inject = &inject{Hosts: sec.Hosts, Header: sec.Header, Query: sec.Query, Placeholder: sec.Placeholder}
	}
	return v
}

// fingerprint identifies an env entry without keeping its value; nil has none.
func fingerprint(v *envVar) string {
	if v == nil {
		return ""
	}
	raw, _ := json.Marshal(v)
	sum := sha256.Sum256(raw)
	return hex.EncodeToString(sum[:])
}
