// Package auth resolves the bearer token on every call to a user id, or to a share of a box.
package auth

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"slices"
	"strings"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
)

const (
	// HeaderAuthorization carries `Bearer <user token>`, or `Bearer <share secret>` on MCP.
	HeaderAuthorization = "authorization"
	// HeaderGuestToken carries the caller's own user token next to the secret of a share that names its users.
	HeaderGuestToken = "x-cocoonbox-guest-token" //nolint:gosec // a header name, not a credential
	// HeaderOperatorUser names the user an operator call acts as; only the operator socket reads it.
	HeaderOperatorUser = "x-cocoonbox-operator-user"
	// SecretPrefix starts every share secret; a bearer is tried as a share secret only when no user token matches it.
	SecretPrefix = "cbs_"
	// InviteTokenPrefix starts every invite-scoped token, which is never a user token.
	InviteTokenPrefix = "cbit_"

	bearerPrefix = "bearer "
)

type (
	userKey       struct{}
	credentialKey struct{}
	operatorKey   struct{}
)

// Users resolves the sha256 of an API token, in lowercase hex, to an enabled user id, and gives the context that ends when the user is disabled or deleted or the credential ends.
type Users interface {
	ByTokenSHA256(ctx context.Context, sum string) (string, bool)
	Lifetime(user, credential string) (context.Context, func())
}

// Shares resolves the sha256 of a share secret, in lowercase hex, to a live share of an enabled owner.
type Shares interface {
	ShareBySecretSHA256(sum string) (Share, bool)
}

// Share is a live share as the edge admits it.
type Share struct {
	ID    string
	Owner string
	Box   string
	// Users are the user ids the share admits; empty admits anyone with the link.
	Users     []string
	Write     bool
	ExpiresAt time.Time
	// Cut closes when the streams admitted so far must end: revoke, regenerate, expiry, or the owner disabled or deleted.
	Cut <-chan struct{}
	// Moded closes when the owner switches write.
	Moded <-chan struct{}
}

// Grant is one admitted share call: the share and the guest's user id, empty for an open share.
type Grant struct {
	Share
	Guest string
}

// Principal names the caller in the box credential: share:<id>:<read|write>[:<guest>].
func (g Grant) Principal() string {
	mode := "read"
	if g.Write {
		mode = "write"
	}
	p := "share:" + g.ID + ":" + mode
	if g.Guest != "" {
		p += ":" + g.Guest
	}
	return p
}

// Authenticator maps bearer tokens to user ids and share secrets to shares.
type Authenticator struct {
	users  Users
	shares Shares
}

// New authenticates against users and shares.
func New(users Users, shares Shares) *Authenticator {
	return &Authenticator{users: users, shares: shares}
}

// Admit admits a share secret for box, or for the share's own box when box is empty; guest is the caller's user id or empty.
func (a *Authenticator) Admit(secret, box, guest string) (Grant, error) {
	sum := sha256.Sum256([]byte(secret))
	sh, ok := a.shares.ShareBySecretSHA256(hex.EncodeToString(sum[:]))
	switch {
	case !ok || !time.Now().Before(sh.ExpiresAt):
		return Grant{}, status.Error(codes.PermissionDenied, "the share link is revoked, regenerated or expired")
	case box != "" && box != sh.Box:
		return Grant{}, status.Error(codes.PermissionDenied, "the share link is for another box")
	case len(sh.Users) == 0:
		return Grant{Share: sh}, nil
	case guest == "":
		return Grant{}, status.Error(codes.Unauthenticated, "this share names its users: send your own cocoonbox token too")
	case !slices.Contains(sh.Users, guest):
		return Grant{}, status.Error(codes.PermissionDenied, "this share does not name you")
	}
	return Grant{Share: sh, Guest: guest}, nil
}

// Guest maps a guest token header value to its user; the error is a gRPC status.
func (a *Authenticator) Guest(ctx context.Context, token string) (string, error) {
	if token == "" {
		return "", nil
	}
	user, _, ok := a.byToken(ctx, token)
	if !ok {
		return "", status.Error(codes.Unauthenticated, "invalid guest token")
	}
	return user, nil
}

func (a *Authenticator) Unary(ctx context.Context, req any, _ *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (any, error) {
	ctx, err := a.authenticate(ctx)
	if err != nil {
		return nil, err
	}
	return handler(ctx, req)
}

// Stream authenticates streaming calls, including those served by the unknown-service handler, and ends each when its user is disabled or deleted or its token revoked.
func (a *Authenticator) Stream(srv any, ss grpc.ServerStream, _ *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
	ctx, err := a.authenticate(ss.Context())
	if err != nil {
		return err
	}
	ctx, stop := a.Bind(ctx, User(ctx), Credential(ctx))
	defer stop()
	return handler(srv, &authedStream{ServerStream: ss, ctx: ctx})
}

// Authenticate maps an Authorization header value to its user; the error is a gRPC status.
func (a *Authenticator) Authenticate(ctx context.Context, header string) (string, error) {
	user, _, err := a.admit(ctx, header)
	return user, err
}

func (a *Authenticator) Bind(ctx context.Context, user, credential string) (context.Context, context.CancelFunc) {
	life, release := a.users.Lifetime(user, credential)
	ctx, cancel := context.WithCancel(ctx)
	stop := context.AfterFunc(life, cancel)
	return ctx, func() {
		stop()
		cancel()
		release()
	}
}

func (a *Authenticator) Identify(ctx context.Context) string {
	md, _ := metadata.FromIncomingContext(ctx)
	values := md.Get(HeaderAuthorization)
	if len(values) != 1 {
		return ""
	}
	user, _ := a.Authenticate(ctx, values[0])
	return user
}

func (a *Authenticator) admit(ctx context.Context, header string) (string, string, error) {
	token, ok := Bearer(header)
	if !ok {
		return "", "", status.Error(codes.Unauthenticated, "missing bearer token")
	}
	user, credential, ok := a.byToken(ctx, token)
	if !ok {
		return "", "", status.Error(codes.Unauthenticated, "invalid bearer token")
	}
	return user, credential, nil
}

func (a *Authenticator) byToken(ctx context.Context, token string) (string, string, bool) {
	if strings.HasPrefix(token, InviteTokenPrefix) {
		return "", "", false
	}
	credential := TokenCredential(token)
	user, ok := a.users.ByTokenSHA256(ctx, credential)
	return user, credential, ok
}

func (a *Authenticator) authenticate(ctx context.Context) (context.Context, error) {
	md, _ := metadata.FromIncomingContext(ctx)
	values := md.Get(HeaderAuthorization)
	if len(values) != 1 {
		return nil, status.Error(codes.Unauthenticated, "missing bearer token")
	}
	user, credential, err := a.admit(ctx, values[0])
	if err != nil {
		return nil, err
	}
	return context.WithValue(WithUser(ctx, user), credentialKey{}, credential), nil
}

type authedStream struct {
	grpc.ServerStream
	ctx context.Context
}

func (s *authedStream) Context() context.Context { return s.ctx }

// ShareSecret returns the share secret an Authorization header carries.
func ShareSecret(header string) (string, bool) {
	secret, ok := Bearer(header)
	return secret, ok && strings.HasPrefix(secret, SecretPrefix)
}

func Bearer(header string) (string, bool) {
	if len(header) <= len(bearerPrefix) || !strings.EqualFold(header[:len(bearerPrefix)], bearerPrefix) {
		return "", false
	}
	return header[len(bearerPrefix):], true
}

// TokenCredential names a user or device token's credential for Bind: its sha256 in lowercase hex.
func TokenCredential(token string) string {
	sum := sha256.Sum256([]byte(token))
	return hex.EncodeToString(sum[:])
}

// Credential returns the credential the interceptors admitted, "" when ctx did not pass them.
func Credential(ctx context.Context) string {
	credential, _ := ctx.Value(credentialKey{}).(string)
	return credential
}

// User returns the authenticated user id, "" when ctx did not pass the interceptors.
func User(ctx context.Context) string {
	user, _ := ctx.Value(userKey{}).(string)
	return user
}

func Caller(ctx context.Context) (string, error) {
	if user := User(ctx); user != "" {
		return user, nil
	}
	return "", status.Error(codes.Unauthenticated, "unauthenticated")
}

// WithUser returns ctx carrying user, as the interceptors do.
func WithUser(ctx context.Context, user string) context.Context {
	return context.WithValue(ctx, userKey{}, user)
}

// Operator reports whether ctx is a call of the operator socket.
func Operator(ctx context.Context) bool {
	operator, _ := ctx.Value(operatorKey{}).(bool)
	return operator
}

// OperatorUnary admits unary calls as the user in HeaderOperatorUser; the operator socket's file mode is the access check.
func OperatorUnary(ctx context.Context, req any, _ *grpc.UnaryServerInfo, handler grpc.UnaryHandler) (any, error) {
	ctx, err := operatorUser(ctx)
	if err != nil {
		return nil, err
	}
	return handler(ctx, req)
}

// OperatorStream admits streaming calls as the user in HeaderOperatorUser.
func OperatorStream(srv any, ss grpc.ServerStream, _ *grpc.StreamServerInfo, handler grpc.StreamHandler) error {
	ctx, err := operatorUser(ss.Context())
	if err != nil {
		return err
	}
	return handler(srv, &authedStream{ServerStream: ss, ctx: ctx})
}

func operatorUser(ctx context.Context) (context.Context, error) {
	md, _ := metadata.FromIncomingContext(ctx)
	values := md.Get(HeaderOperatorUser)
	if len(values) != 1 || values[0] == "" {
		return nil, status.Error(codes.Unauthenticated, "missing operator user")
	}
	return context.WithValue(WithUser(ctx, values[0]), operatorKey{}, true), nil
}
