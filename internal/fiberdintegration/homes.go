// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"bytes"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	consumer "github.com/Azure/fiberd/sdk/go/client"
	"google.golang.org/grpc"
	"google.golang.org/grpc/backoff"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/peer"
	"google.golang.org/grpc/status"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/dynamic"
)

var capacityGrantResource = schema.GroupVersionResource{
	Group: "fiberd.io", Version: "v1alpha1", Resource: "capacitygrants",
}

type HomeGrant struct {
	UID, Token string
	MaxFibers  int
	Client     FiberClient
}

type HomeResolver interface {
	Resolve(context.Context, string, string) (HomeGrant, error)
	// Forget drops what is known about a CapacityGrant's homes after one
	// failed, so the next Resolve asks the controller again. A failed home,
	// named by its grant UID, is not served again for a while.
	Forget(namespace, name, failedUID string)
	// Retain drops the CapacityGrants whose namespace/name keep does not
	// list, once their Revisions are no longer routed.
	Retain(keep map[string]bool)
	Close() error
}

// callerGrantPath is where fiberd-k8s-controller hands a caller the grants
// it presents to a CapacityGrant's Pods, if the CapacityGrant names the key
// of the caller's client certificate.
const callerGrantPath = "/v1/caller-grants"

// callerGrant is one Pod's grant, whose UID is the Pod's.
type callerGrant struct {
	Token     string    `json:"token"`
	UID       string    `json:"uid"`
	ExpiresAt time.Time `json:"expiresAt"`
	Address   string    `json:"address"`
}

// homeAddress is where a grant Pod is dialled and the name its serving
// certificate carries, which every Pod of a CapacityGrant shares.
type homeAddress struct {
	address, serverName string
}

// askBackoff is how long a refused or empty answer keeps the router from
// asking again when the controller names no Retry-After, so retries stay
// within the controller's per-caller rate limit.
const askBackoff = time.Second

// failedHomeBackoff is how long a home a Clone failed on is passed over, so
// a Pod that is not admitted yet, or keeps crashing while it runs, does not
// take every request while another home serves.
const failedHomeBackoff = 5 * time.Second

// askTimeout bounds one refresh, which no request's context cancels: its
// answer serves every request after it.
const askTimeout = 10 * time.Second

type cachedHome struct {
	HomeGrant
	home      homeAddress
	expiresAt time.Time
}

type homeCache struct {
	mu        sync.Mutex
	refreshAt time.Time
	nextAsk   time.Time
	homes     []cachedHome
	// failed holds back the homes a Clone failed on, by grant UID, until
	// the time each maps to.
	failed map[string]time.Time
	// asking is closed when the refresh in flight ends; nil when none is.
	asking  chan struct{}
	lastErr error
}

// CallerGrants resolves a CapacityGrant to the grant Pods the router may
// Clone from, with a grant for each, by asking fiberd-k8s-controller over
// the router's client certificate. Every grant is bound to that
// certificate's key, so the router presents it to the grant Pods too.
type CallerGrants struct {
	// API reads the CapacityGrant's fiber cap, which the caller grant does
	// not repeat.
	API dynamic.Interface
	// Issuer is the controller's issuer URL.
	Issuer string
	// CertFile and KeyFile are the router's client certificate, and CAFile
	// the fiberd CA that signs it, the controller's and every grant Pod's.
	CertFile, KeyFile, CAFile string
	// HTTP overrides the client built from the files, for tests.
	HTTP *http.Client
	// Dial overrides how a home is dialled, for tests.
	Dial func(ctx context.Context, address, serverName string) (FiberClient, error)

	mu          sync.Mutex
	homes       map[string]*homeCache
	connections map[homeAddress]*homeConnection
	// draining are pruned connections that still hold fibers.
	draining map[*homeConnection]bool
	client   *http.Client

	tlsMu sync.Mutex
	tls   *tls.Config
}

func (grants *CallerGrants) Resolve(ctx context.Context, namespace, name string) (HomeGrant, error) {
	if !grantName.MatchString(namespace) || !grantName.MatchString(name) {
		return HomeGrant{}, fmt.Errorf("invalid CapacityGrant reference")
	}
	cache := grants.cache(namespace + "/" + name)
	cache.mu.Lock()
	now := time.Now()
	if (!now.Before(cache.refreshAt) || !cache.live(now)) && !now.Before(cache.nextAsk) && cache.asking == nil {
		cache.asking = make(chan struct{})
		go grants.refresh(cache, namespace, name)
	}
	// A refresh in flight, failed or empty leaves the homes already held
	// in use until their own tokens lapse. A home a Clone has just failed
	// on is the exception while a refresh is in flight: it is most likely
	// a Pod a rollout replaced, and the answer names its replacement.
	asking := cache.asking
	if home, failed, ok := cache.first(now); ok && (!failed || asking == nil) {
		cache.mu.Unlock()
		return home, nil
	}
	cache.mu.Unlock()
	if asking != nil {
		select {
		case <-asking:
		case <-ctx.Done():
			return HomeGrant{}, ctx.Err()
		}
	}
	cache.mu.Lock()
	defer cache.mu.Unlock()
	if home, _, ok := cache.first(time.Now()); ok {
		return home, nil
	}
	// Clients are told to retry when the backoff the refusal set ends, in
	// whole seconds rounded up, not after the whole of it again.
	var refused *GrantsRefused
	if errors.As(cache.lastErr, &refused) {
		remaining := *refused
		left := time.Until(cache.nextAsk) + time.Second - 1
		remaining.RetryAfter = max(left.Truncate(time.Second), time.Second)
		return HomeGrant{}, &remaining
	}
	if cache.lastErr != nil {
		return HomeGrant{}, cache.lastErr
	}
	return HomeGrant{}, fmt.Errorf("CapacityGrant %s/%s has no ready home", namespace, name)
}

// refresh asks the controller once for every request waiting on cache.
func (grants *CallerGrants) refresh(cache *homeCache, namespace, name string) {
	ctx, cancel := context.WithTimeout(context.Background(), askTimeout)
	defer cancel()
	homes, earliest, err := grants.ask(ctx, namespace, name)
	// Connections are made and pruned under one lock, so a prune never
	// closes one a refresh has just made.
	grants.mu.Lock()
	defer grants.mu.Unlock()
	// A Retain while the controller was asked may have dropped the cache,
	// and a prune would then close the connections made for its waiters.
	// It goes back unless a new cache has taken its place; the next Retain
	// drops it again if its Revision is still not routed.
	key := namespace + "/" + name
	if _, replaced := grants.homes[key]; !replaced {
		if grants.homes == nil {
			grants.homes = make(map[string]*homeCache)
		}
		grants.homes[key] = cache
	}
	for i := range homes {
		if err == nil {
			homes[i].Client, err = grants.connect(ctx, homes[i].home)
		}
	}
	// The backoff runs from when the answer came, however long it took.
	now := time.Now()
	cache.mu.Lock()
	switch {
	case err != nil:
		cache.lastErr = err
		cache.nextAsk = now.Add(retryAfter(err))
	case len(homes) == 0:
		cache.lastErr = fmt.Errorf("CapacityGrant %s/%s has no ready home", namespace, name)
		cache.nextAsk = now.Add(askBackoff)
	default:
		cache.homes = homes
		// A home the answer no longer lists has gone; one it still lists
		// stays held back until its backoff ends.
		for uid := range cache.failed {
			if !slices.ContainsFunc(homes, func(home cachedHome) bool { return home.UID == uid }) {
				delete(cache.failed, uid)
			}
		}
		// The controller asks callers to come back before the earliest
		// grant lapses; half its life lands well before any home refuses
		// it. A grant that has as good as lapsed already, as when the two
		// clocks disagree, is asked about again after askBackoff, not at
		// once on every request.
		cache.nextAsk = time.Time{}
		wait := earliest.Sub(now) / 2
		if wait < askBackoff {
			wait = askBackoff
			cache.nextAsk = now.Add(askBackoff)
		}
		cache.refreshAt = now.Add(wait)
		cache.lastErr = nil
	}
	cache.mu.Unlock()
	// Pruning comes before the waiters wake, so a request that waited on
	// this refresh never finds a connection the answer dropped still open.
	if err == nil && len(homes) > 0 {
		grants.prune()
	}
	cache.mu.Lock()
	close(cache.asking)
	cache.asking = nil
	cache.mu.Unlock()
}

// first serves the live home with the lowest Pod UID that has not failed.
// The controller lists every running Pod of the grant, admitted or not. A
// grant runs one replica, which the Recreate strategy replaces only once
// the old Pod has gone, so an answer usually lists one home, and it may
// not Clone yet. Should an answer list more, a home that failed is passed
// over while another can still Clone. With every live home failed it
// serves the one whose backoff ends first, and reports it failed, so one
// transient error does not leave a grant with a single Pod unserved for
// the whole backoff.
func (cache *homeCache) first(now time.Time) (grant HomeGrant, failed, ok bool) {
	var soonest *cachedHome
	for i := range cache.homes {
		home := &cache.homes[i]
		if !now.Before(home.expiresAt) {
			continue
		}
		if !now.Before(cache.failed[home.UID]) {
			return home.HomeGrant, false, true
		}
		if soonest == nil || cache.failed[home.UID].Before(cache.failed[soonest.UID]) {
			soonest = home
		}
	}
	if soonest == nil {
		return HomeGrant{}, false, false
	}
	return soonest.HomeGrant, true, true
}

func (grants *CallerGrants) Retain(keep map[string]bool) {
	grants.mu.Lock()
	defer grants.mu.Unlock()
	for key := range grants.homes {
		if !keep[key] {
			delete(grants.homes, key)
		}
	}
	grants.prune()
}

// prune closes the connections to Pods no cached grant names any more, as
// Pod IPs are reused once their Pods are gone. grants.mu must be held.
func (grants *CallerGrants) prune() {
	used := map[homeAddress]bool{}
	for _, cache := range grants.homes {
		cache.mu.Lock()
		for _, home := range cache.homes {
			used[home.home] = true
		}
		cache.mu.Unlock()
	}
	for address, connection := range grants.connections {
		if used[address] {
			continue
		}
		delete(grants.connections, address)
		if connection.refs == 0 {
			connection.close()
			continue
		}
		// A request or a pooled slot still holds it, and its fiber must
		// still be released there, so the last holder closes it.
		if grants.draining == nil {
			grants.draining = make(map[*homeConnection]bool)
		}
		grants.draining[connection] = true
	}
}

// heldClient is a FiberClient whose connection must stay open while a
// request or a pooled slot uses it. Hold fails once it has been closed, and
// every successful Hold is matched by one Drop.
type heldClient interface {
	Hold() error
	Drop()
}

// homeConnection is one grant Pod's connection. It counts its holders, so
// pruning never closes it under a clone in flight or a fiber not yet
// released.
type homeConnection struct {
	FiberClient
	grants *CallerGrants
	refs   int
	closed bool
}

func (connection *homeConnection) Hold() error {
	connection.grants.mu.Lock()
	defer connection.grants.mu.Unlock()
	if connection.closed {
		return errors.New("home connection is closed")
	}
	connection.refs++
	return nil
}

func (connection *homeConnection) Drop() {
	grants := connection.grants
	grants.mu.Lock()
	defer grants.mu.Unlock()
	connection.refs--
	if connection.refs == 0 && grants.draining[connection] {
		delete(grants.draining, connection)
		connection.close()
	}
}

// ResetConnectBackoff has the connection try its Pod again at once, if
// its client can.
func (connection *homeConnection) ResetConnectBackoff() {
	resetConnectBackoff(connection.FiberClient)
}

// close closes the connection. grants.mu must be held.
func (connection *homeConnection) close() error {
	connection.closed = true
	if closer, ok := connection.FiberClient.(io.Closer); ok {
		return closer.Close()
	}
	return nil
}

func (cache *homeCache) live(now time.Time) bool {
	for _, home := range cache.homes {
		if now.Before(home.expiresAt) {
			return true
		}
	}
	return false
}

// GrantsRefused is the controller refusing to answer for a grant's homes
// for now, with Retry-After. No home refused anything, so it is not a
// shed, but the client is still told when to retry.
type GrantsRefused struct {
	Issuer, Grant, Status, Reason string
	RetryAfter                    time.Duration
}

func (e *GrantsRefused) Error() string {
	return fmt.Sprintf("%s refused caller grants for %s: %s %s, retry after %s",
		e.Issuer, e.Grant, e.Status, e.Reason, e.RetryAfter)
}

func retryAfter(err error) time.Duration {
	var refused *GrantsRefused
	if errors.As(err, &refused) {
		return refused.RetryAfter
	}
	return askBackoff
}

func (grants *CallerGrants) Forget(namespace, name, failedUID string) {
	cache := grants.cache(namespace + "/" + name)
	cache.mu.Lock()
	cache.refreshAt = time.Time{}
	if failedUID != "" {
		if cache.failed == nil {
			cache.failed = make(map[string]time.Time)
		}
		cache.failed[failedUID] = time.Now().Add(failedHomeBackoff)
	}
	cache.mu.Unlock()
}

func (grants *CallerGrants) cache(key string) *homeCache {
	grants.mu.Lock()
	defer grants.mu.Unlock()
	if grants.homes == nil {
		grants.homes = make(map[string]*homeCache)
	}
	cache := grants.homes[key]
	if cache == nil {
		cache = &homeCache{}
		grants.homes[key] = cache
	}
	return cache
}

// ask returns the grant's homes ordered by Pod UID, so that the first one,
// which Resolve serves, stays the same while its Pod does, and the earliest
// time one of their grants lapses.
func (grants *CallerGrants) ask(ctx context.Context, namespace, name string) ([]cachedHome, time.Time, error) {
	capacity, err := grants.API.Resource(capacityGrantResource).Namespace(namespace).Get(ctx, name, metav1.GetOptions{})
	if err != nil {
		return nil, time.Time{}, fmt.Errorf("read CapacityGrant: %w", err)
	}
	maxFibers, _, err := unstructured.NestedInt64(capacity.Object, "spec", "fibers", "max")
	if err != nil {
		return nil, time.Time{}, fmt.Errorf("read CapacityGrant: %w", err)
	}
	if maxFibers <= 0 {
		return nil, time.Time{}, fmt.Errorf("CapacityGrant %s/%s has no fiber cap", namespace, name)
	}
	client, err := grants.httpClient()
	if err != nil {
		return nil, time.Time{}, err
	}
	body, _ := json.Marshal(map[string]string{"namespace": namespace, "name": name})
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, strings.TrimSuffix(grants.Issuer, "/")+callerGrantPath, bytes.NewReader(body))
	if err != nil {
		return nil, time.Time{}, err
	}
	request.Header.Set("Content-Type", "application/json")
	response, err := client.Do(request)
	if err != nil {
		return nil, time.Time{}, fmt.Errorf("ask for caller grants: %w", err)
	}
	defer response.Body.Close()
	data, err := io.ReadAll(io.LimitReader(response.Body, 1<<20))
	if err != nil {
		return nil, time.Time{}, fmt.Errorf("read caller grants: %w", err)
	}
	if response.StatusCode == http.StatusUnauthorized || response.StatusCode == http.StatusForbidden {
		// A kept-alive connection still presents the certificate it was
		// opened with, so after a key change it would be refused forever.
		// The next ask handshakes again with the current files. The body
		// is closed first: until it is, HTTP/2 still counts the stream and
		// keeps the connection.
		response.Body.Close()
		client.CloseIdleConnections()
	}
	if response.StatusCode != http.StatusOK {
		var refusal struct {
			Error string `json:"error"`
		}
		_ = json.Unmarshal(data, &refusal)
		if seconds, err := strconv.Atoi(response.Header.Get("Retry-After")); err == nil && seconds > 0 {
			return nil, time.Time{}, &GrantsRefused{
				Issuer: grants.Issuer, Grant: namespace + "/" + name,
				Status: response.Status, Reason: refusal.Error,
				RetryAfter: time.Duration(seconds) * time.Second,
			}
		}
		return nil, time.Time{}, fmt.Errorf("caller grants for %s/%s refused: %s %s", namespace, name, response.Status, refusal.Error)
	}
	var answer struct {
		ServerName string        `json:"serverName"`
		Grants     []callerGrant `json:"grants"`
	}
	if err := json.Unmarshal(data, &answer); err != nil {
		return nil, time.Time{}, fmt.Errorf("decode caller grants: %w", err)
	}
	homes := make([]cachedHome, 0, len(answer.Grants))
	var earliest time.Time
	for _, grant := range answer.Grants {
		// Each token names its Pod as audience, so it is presented to
		// that Pod's own address and never to the grant's Service.
		homes = append(homes, cachedHome{
			HomeGrant: HomeGrant{UID: grant.UID, Token: grant.Token, MaxFibers: int(maxFibers)},
			home:      homeAddress{address: grant.Address, serverName: answer.ServerName},
			expiresAt: grant.ExpiresAt,
		})
		if earliest.IsZero() || grant.ExpiresAt.Before(earliest) {
			earliest = grant.ExpiresAt
		}
	}
	slices.SortFunc(homes, func(a, b cachedHome) int { return strings.Compare(a.UID, b.UID) })
	return homes, earliest, nil
}

func (grants *CallerGrants) httpClient() (*http.Client, error) {
	if grants.HTTP != nil {
		return grants.HTTP, nil
	}
	config, err := grants.tlsConfig()
	if err != nil {
		return nil, err
	}
	grants.mu.Lock()
	defer grants.mu.Unlock()
	if grants.client == nil {
		transport := http.DefaultTransport.(*http.Transport).Clone()
		transport.TLSClientConfig = config
		grants.client = &http.Client{Transport: transport, Timeout: 5 * time.Second}
	}
	return grants.client, nil
}

// tlsConfig is the router's mutual TLS to the controller and to grant Pods.
// It has a lock of its own, as connect calls it under grants.mu.
func (grants *CallerGrants) tlsConfig() (*tls.Config, error) {
	grants.tlsMu.Lock()
	defer grants.tlsMu.Unlock()
	if grants.tls != nil {
		return grants.tls, nil
	}
	ca, err := os.ReadFile(grants.CAFile)
	if err != nil {
		return nil, fmt.Errorf("read fiberd CA: %w", err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(ca) {
		return nil, fmt.Errorf("fiberd CA %s holds no certificate", grants.CAFile)
	}
	certFile, keyFile := grants.CertFile, grants.KeyFile
	grants.tls = &tls.Config{
		RootCAs:    roots,
		MinVersion: tls.VersionTLS13,
		// Read for every handshake, so a renewal cert-manager writes into
		// the mounted Secret is presented without a restart. The renewal
		// keeps the key, which is what the grants are bound to.
		GetClientCertificate: func(*tls.CertificateRequestInfo) (*tls.Certificate, error) {
			certificate, err := tls.LoadX509KeyPair(certFile, keyFile)
			if err != nil {
				return nil, fmt.Errorf("load client certificate: %w", err)
			}
			return &certificate, nil
		},
	}
	return grants.tls, nil
}

// connect returns the connection to home, dialling it once. grants.mu must
// be held.
func (grants *CallerGrants) connect(ctx context.Context, home homeAddress) (FiberClient, error) {
	if connection := grants.connections[home]; connection != nil {
		return connection, nil
	}
	dial := grants.Dial
	if dial == nil {
		dial = grants.dialTLS
	}
	client, err := dial(ctx, home.address, home.serverName)
	if err != nil {
		return nil, fmt.Errorf("dial home %s: %w", home.address, err)
	}
	if grants.connections == nil {
		grants.connections = make(map[homeAddress]*homeConnection)
	}
	connection := &homeConnection{FiberClient: client, grants: grants}
	grants.connections[home] = connection
	return connection, nil
}

// dialTLS dials a grant Pod by its IP, verifying the name its certificate
// carries instead, since the Pod's certificate names only its Service.
func (grants *CallerGrants) dialTLS(_ context.Context, address, serverName string) (FiberClient, error) {
	config, err := grants.tlsConfig()
	if err != nil {
		return nil, err
	}
	config = config.Clone()
	config.ServerName = serverName
	return dialHome(address, credentials.NewTLS(config))
}

// homeDialTimeout bounds the TCP connect to a grant Pod, after which a
// Clone on it fails as unsent. A Pod a rollout has replaced leaves an IP
// that drops packets, which would otherwise hold every request sent there
// for its whole acquire budget; a Pod that is up accepts at once however
// busy it is, since its kernel answers.
const homeDialTimeout = time.Second

// homeConnectTimeout bounds the whole connect, TLS handshake included. A
// grant Pod restoring a burst of Fibers can be slow to handshake, and a
// connect that fails leaves its connection backing off, failing every
// Clone at once until the next try, so the handshake may take most of an
// acquire budget. gRPC's default of 20 seconds would hold the connection
// to a Pod that accepts but never answers for much longer.
const homeConnectTimeout = 3 * time.Second

// homeReconnectBackoff caps the wait between connects to a grant Pod at
// a few seconds. gRPC's default grows it to two minutes, and a Pod that
// kept its IP through a minute's outage would then fail every Clone at
// once, as unsent, for up to two minutes after it is back.
var homeReconnectBackoff = backoff.Config{
	BaseDelay:  backoff.DefaultConfig.BaseDelay,
	Multiplier: backoff.DefaultConfig.Multiplier,
	Jitter:     backoff.DefaultConfig.Jitter,
	MaxDelay:   2 * time.Second,
}

// dialHome is consumer.DialTLS with short connect timeouts, and with every
// call that never reached a connection marked as notSent, which the router
// may retry on another home without risking a second Fiber. A Clone does
// not wait for a connection that is backing off: it fails as unsent, so
// the router asks whether the grant's home has moved, where waiting would
// hold it on a dead IP for its whole budget. Its own dialer reaches Pod IPs
// directly, never through a proxy from the environment.
func dialHome(address string, transport credentials.TransportCredentials) (FiberClient, error) {
	connection, err := grpc.NewClient(address,
		grpc.WithTransportCredentials(transport),
		grpc.WithContextDialer(dialPod),
		grpc.WithConnectParams(grpc.ConnectParams{Backoff: homeReconnectBackoff, MinConnectTimeout: homeConnectTimeout}),
		grpc.WithChainUnaryInterceptor(markNotSent))
	if err != nil {
		return nil, err
	}
	return &homeClient{Client: consumer.New(connection), connection: connection}, nil
}

// homeClient is a grant Pod's fiberd client that can also cut short its
// connection's wait before the next connect.
type homeClient struct {
	*consumer.Client
	connection *grpc.ClientConn
}

func (client *homeClient) ResetConnectBackoff() { client.connection.ResetConnectBackoff() }

func dialPod(ctx context.Context, address string) (net.Conn, error) {
	dialer := net.Dialer{Timeout: homeDialTimeout}
	return dialer.DialContext(ctx, "tcp", address)
}

// notSent is a call that failed before gRPC gave it a stream on any
// connection, so no home received it: the home was unreachable, or its
// connection never became ready in time.
type notSent struct{ err error }

func (e *notSent) Error() string { return e.err.Error() }
func (e *notSent) Unwrap() error { return e.err }

// GRPCStatus keeps the call's status readable through the wrapper, so the
// fiberd client still classifies it.
func (e *notSent) GRPCStatus() *status.Status { return status.Convert(e.err) }

// markNotSent tells a call no home received from one that reached a home:
// gRPC fills in the call's peer only once the call has a stream.
func markNotSent(ctx context.Context, method string, request, reply any, connection *grpc.ClientConn, invoke grpc.UnaryInvoker, options ...grpc.CallOption) error {
	var reached peer.Peer
	err := invoke(ctx, method, request, reply, connection, append(options, grpc.Peer(&reached))...)
	if err != nil && reached.Addr == nil {
		return &notSent{err: err}
	}
	return err
}

func (grants *CallerGrants) Close() error {
	grants.mu.Lock()
	defer grants.mu.Unlock()
	var failures []error
	for _, connection := range grants.connections {
		failures = append(failures, connection.close())
	}
	for connection := range grants.draining {
		failures = append(failures, connection.close())
	}
	return errors.Join(failures...)
}
