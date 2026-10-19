// Package machines defines the interface between the cocoonbox services and a machine backend.
package machines

import (
	"context"
	"errors"
	"net"
	"time"
)

const (
	StateStarting State = iota + 1
	StateRunning
	StateHibernating
	StateHibernated
	StateFailed
	// StateGone reports that a watched machine was deleted.
	StateGone

	ActionNone    Action = 0
	ActionCreated Action = 1
	ActionWoken   Action = 2

	tcpDialTimeout = 5 * time.Second
)

var (
	ErrNotFound = errors.New("machine not found")
	// ErrConflict reports a concurrent change; the caller may retry.
	ErrConflict = errors.New("concurrent machine change")
	// ErrRefused reports a request the backend refused as invalid; a retry with the same values fails again.
	ErrRefused  = errors.New("request refused by the backend")
	ErrCapacity = errors.New("node at capacity, try again shortly")
)

// State is the lifecycle state of a machine.
type State int

// Action is what Ensure did to reach its result.
type Action int

// Flavor sizes a machine.
type Flavor struct {
	VCPUs     uint32
	MemoryMiB uint32
	DiskGiB   uint32
}

// Spec describes a machine to create; Owner is an opaque key the backend stores and filters on.
type Spec struct {
	ID    string
	Owner string
	// Tenant is the owner's backend credential; empty claims with the backend's own.
	Tenant string
	Flavor Flavor
	// Image overrides the backend's default image.
	Image string
}

// Machine is a backend's view of one machine.
type Machine struct {
	ID     string
	Owner  string
	State  State
	Image  string
	Flavor Flavor
	// Reason explains StateFailed.
	Reason string
	// Detail summarizes the backend's raw status for logs; it may name internal hosts.
	Detail    string
	CreatedAt time.Time
	// Revision increases with every lifecycle request; Watch judges state no older than it.
	Revision int64
	// Incarnation changes whenever the machine boots anew: first boot, wake, rebuild.
	Incarnation string
	// Meta holds the service's small key/value records stored with the machine.
	Meta map[string]string
	// Routed reports that the machine's egress leaves through an upstream proxy.
	Routed bool
}

// Identity tells a box agent which box and wake it serves and which edge key signs its credentials; the guest takes Name as its hostname.
type Identity struct {
	BoxID     string `json:"box_id"`
	WakeID    string `json:"wake_id"`
	KeyID     string `json:"key_id"`
	PublicKey string `json:"public_key"`
	Name      string `json:"name,omitempty"`
}

// Secret is a host-only value; with Hosts, the backend writes it into Header, or the Query parameter, on requests to Hosts that carry Placeholder.
type Secret struct {
	Value       string
	Hosts       []string
	Header      string
	Query       string
	Placeholder string
}

// Credentials are host-only secrets and the guest-visible flags that switch their use on.
type Credentials struct {
	// Secrets maps a name to its secret; a nil secret removes the name.
	Secrets map[string]*Secret
	// Flags maps a guest-visible name to its value; "" removes the name.
	Flags map[string]string
	// Prune removes the machine's names that start with it and that Secrets lacks.
	Prune string
}

// DialFunc opens a connection to one port of a machine.
type DialFunc func(ctx context.Context) (net.Conn, error)

// Endpoint reaches a machine's agent and its RFB server; Key names the agent as host:port and changes when the machine moves.
type Endpoint struct {
	Key    string
	Dial   DialFunc
	Screen DialFunc
}

// TCPEndpoint reaches an agent listening on addr.
func TCPEndpoint(addr string) Endpoint {
	return Endpoint{Key: addr, Dial: func(ctx context.Context) (net.Conn, error) {
		return (&net.Dialer{Timeout: tcpDialTimeout}).DialContext(ctx, "tcp", addr)
	}}
}

// NotRunningError is returned by Address for a machine that cannot take traffic.
type NotRunningError struct {
	State State
}

func (e *NotRunningError) Error() string { return "machine is not running" }

// Machines is implemented by every machine backend.
type Machines interface {
	// Ensure creates the machine, or wakes it when hibernated, and returns without waiting.
	Ensure(ctx context.Context, spec Spec) (Machine, Action, error)
	Get(ctx context.Context, id string) (Machine, error)
	// List returns owner's machines that are not being deleted, or every such machine when owner is empty.
	List(ctx context.Context, owner string) ([]Machine, error)
	// Hibernate requests hibernation and returns without waiting.
	Hibernate(ctx context.Context, id string) (Machine, error)
	// Watch sends the machine's state, judged no older than revision, on every change until ctx ends or the machine is gone.
	Watch(ctx context.Context, id string, revision int64) <-chan Machine
	// Changes returns a channel closed on the next change of one of owner's machines, its removal included.
	Changes(owner string) <-chan struct{}
	// Address returns how to reach the running machine's agent.
	Address(ctx context.Context, id string) (Endpoint, error)
	// Delete removes the machine and its disk.
	Delete(ctx context.Context, id string) error
	// SetMeta stores value under key in the machine's Meta.
	SetMeta(ctx context.Context, id, key, value string) error
	// Deliver installs keys as host-only secrets on the running machine, then hands its agent the identity.
	Deliver(ctx context.Context, id string, identity Identity, keys map[string]*Secret) error
	// SetCredentials applies c to the machine; flags change only while it runs.
	SetCredentials(ctx context.Context, id string, c Credentials) error
	// SetEgress sends the machine's new connections through the proxy at upstream, or direct when upstream is empty.
	SetEgress(ctx context.Context, id, upstream string) error
}

// Tenants manages each owner's backend tenant, named by the owner.
type Tenants interface {
	// PutTenant adds or changes tenant name with its claim cap; an empty token keeps the stored one.
	PutTenant(ctx context.Context, name, token string, maxClaims int) error
	// DeleteTenant removes tenant name; removing an absent tenant succeeds.
	DeleteTenant(ctx context.Context, name string) error
}
