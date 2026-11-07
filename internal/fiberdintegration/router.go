// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"math/rand/v2"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"regexp"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/Azure/fiberd/pkg/core"
	consumer "github.com/Azure/fiberd/sdk/go/client"
	"github.com/hyperlight-dev/lampyr/internal/project"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	netheader "knative.dev/networking/pkg/http/header"
	"knative.dev/networking/pkg/http/probe"
)

// Namespaces and names arrive in requests, so only those a Kubernetes
// object could have are looked up.
var grantName = regexp.MustCompile(`^[a-z0-9][a-z0-9_.-]*$`)

func tokenDigest(token []byte) string {
	return fmt.Sprintf("sha256:%x", sha256.Sum256(token))
}

type RevisionRoute struct {
	Kind      string                  `json:"kind"`
	Namespace string                  `json:"namespace"`
	Revision  string                  `json:"revision"`
	Execution project.ExecutionPolicy `json:"execution"`
	Timeout   string                  `json:"timeout"`
	Routed    bool                    `json:"routed"`
}

func (revision RevisionRoute) Version() string {
	data, _ := json.Marshal(revision)
	return tokenDigest(data)
}

func (revision RevisionRoute) Validate() error {
	if revision.Kind != "revision" ||
		!grantName.MatchString(revision.Namespace) ||
		!grantName.MatchString(revision.Revision) {
		return fmt.Errorf("Revision route kind, namespace and revision are required")
	}
	if _, err := (project.Manifest{Execution: &revision.Execution}).EffectiveExecution(); err != nil {
		return err
	}
	timeout, err := time.ParseDuration(revision.Timeout)
	if err != nil || timeout <= 0 {
		return fmt.Errorf("Revision route timeout must be a positive duration")
	}
	return nil
}

type FiberClient interface {
	Clone(context.Context, string, string, time.Duration, []byte) (consumer.Fiber, error)
	Release(context.Context, string, bool) error
}

type routerSlot struct {
	session string
	fiber   consumer.Fiber
	client  FiberClient
	busy    bool
	lastUse time.Time
}

type routerPool struct {
	mu     sync.Mutex
	slots  []*routerSlot
	idle   time.Duration
	closed bool
}

type Router struct {
	homes     HomeResolver
	routesDir string
	mu        sync.RWMutex
	revisions map[string]RevisionRoute
	// generations counts each time a Revision's route appears, so metrics
	// recorded for a route that has since gone (and maybe come back) can
	// be told apart; see revisionMetrics.
	generations    map[string]uint64
	nextGeneration uint64
	pools          map[string]*routerPool
	// releases are the requests holding or acquiring a fiber and the slot
	// releases running in the background, which Close waits for before
	// closing the homes' connections, so every fiber is released. They join
	// only under releaseMu while closing is unset, so none joins once Close
	// waits.
	releases  sync.WaitGroup
	releaseMu sync.Mutex
	closing   bool
	sequence  atomic.Uint64
	transport http.RoundTripper
	// h2cTransport forwards requests that arrived as HTTP/2 without TLS: a
	// gRPC fiber serves h2c and rejects HTTP/1.1.
	h2cTransport http.RoundTripper
	// AcquireTimeout bounds how long a request waits for its grant's home,
	// and then, separately, for that home's Clone, so a slow answer about
	// homes doesn't eat the Clone's time and get a healthy home passed
	// over. Restores compete for CPU, so a large burst of cold requests on
	// a small node may need more than the default.
	AcquireTimeout time.Duration
	observer       func(RequestMetric)
	requests       *requestTracker
}

// DefaultAcquireTimeout is AcquireTimeout unless the router is told
// otherwise.
const DefaultAcquireTimeout = 3 * time.Second

// shedRetryAfter reports whether a failed acquisition was the home refusing
// a new Fiber for now, and when to retry. A home sheds under pressure, and
// reports a grant at fibers.max, or its helper restoring too many Fibers to
// start one more in time, as a deferred fallback, since fiberd would rather
// another home take the Clone; the router serves one home per grant at a
// time and passes it over only when it fails, so all mean "try again
// shortly".
func shedRetryAfter(err error) (time.Duration, bool) {
	var shed *consumer.Shed
	if errors.As(err, &shed) {
		return max(shed.RetryAfter, time.Second), true
	}
	var deferred *consumer.Deferred
	if errors.As(err, &deferred) && strings.Contains(deferred.Reason, core.ErrGrantFull.Error()) {
		return time.Second, true
	}
	if restoresBusy(err) {
		return time.Second, true
	}
	return 0, false
}

// helperRestoresBusy starts every refusal of lampyr-hluk-helper's for
// restoring too many Fibers to start one more by its deadline.
const helperRestoresBusy = "restores-busy:"

// restoresBusy reports a grant Pod's helper refusing a Clone because the
// restores ahead of it would outlast its deadline. fiberd takes any helper
// refusal as a failed birth and sends it as a deferred fallback, as it
// does a broken helper's, with the helper's text in the reason, so only
// the helper's marker tells a busy home from a broken one.
func restoresBusy(err error) bool {
	var deferred *consumer.Deferred
	return errors.As(err, &deferred) && strings.Contains(deferred.Reason, helperRestoresBusy)
}

// backpressure reports a home refusing a Clone only until its restores or
// its clone rate limit catch up: nothing started, and nothing is wrong with
// the home.
func backpressure(err error) bool {
	_, limited := rateLimited(err)
	return limited || restoresBusy(err)
}

type requestTracker struct {
	mu      sync.Mutex
	active  int
	closed  bool
	drained chan struct{}
	once    sync.Once
}

func newRequestTracker() *requestTracker {
	return &requestTracker{drained: make(chan struct{})}
}

func (tracker *requestTracker) Begin() bool {
	tracker.mu.Lock()
	defer tracker.mu.Unlock()
	if tracker.closed {
		return false
	}
	tracker.active++
	return true
}

func (tracker *requestTracker) Done() {
	tracker.mu.Lock()
	tracker.active--
	tracker.signalDrained()
	tracker.mu.Unlock()
}

func (tracker *requestTracker) Stop() {
	tracker.mu.Lock()
	tracker.closed = true
	tracker.signalDrained()
	tracker.mu.Unlock()
}

func (tracker *requestTracker) signalDrained() {
	if tracker.closed && tracker.active == 0 {
		tracker.once.Do(func() { close(tracker.drained) })
	}
}

func (tracker *requestTracker) Wait(ctx context.Context) bool {
	select {
	case <-tracker.drained:
		return true
	case <-ctx.Done():
		return false
	}
}

type RequestMetric struct {
	Service, Method, Path, FiberID, FiberKind string
	Status                                    int
	Duration, CloneDuration                   time.Duration
}

func (router *Router) Observe(observer func(RequestMetric)) { router.observer = observer }

func (router *Router) StopRequests()                         { router.requests.Stop() }
func (router *Router) WaitRequests(ctx context.Context) bool { return router.requests.Wait(ctx) }

func NewRouter(homes HomeResolver, routesDir string) (*Router, error) {
	router := &Router{
		homes: homes, routesDir: routesDir,
		revisions:   make(map[string]RevisionRoute),
		generations: make(map[string]uint64),
		pools:       make(map[string]*routerPool),
		transport:   http.DefaultTransport, h2cTransport: newH2CTransport(),
		AcquireTimeout: DefaultAcquireTimeout,
		requests:       newRequestTracker(),
	}
	if err := router.Reload(); err != nil {
		homes.Close()
		return nil, err
	}
	return router, nil
}

func newH2CTransport() http.RoundTripper {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Protocols = new(http.Protocols)
	transport.Protocols.SetUnencryptedHTTP2(true)
	return transport
}

func (router *Router) Reload() error {
	entries, err := os.ReadDir(router.routesDir)
	if err != nil {
		return err
	}
	revisions := map[string]RevisionRoute{}
	retain := map[string]bool{}
	routed := map[string]bool{}
	idle := map[string]time.Duration{}
	for _, entry := range entries {
		if !strings.HasSuffix(entry.Name(), ".json") {
			continue
		}
		data, err := os.ReadFile(router.routesDir + "/" + entry.Name())
		if err != nil {
			return err
		}
		var metadata struct {
			Kind string `json:"kind"`
		}
		if err := json.Unmarshal(data, &metadata); err != nil {
			return fmt.Errorf("decode route metadata %s: %w", entry.Name(), err)
		}
		if metadata.Kind != "revision" {
			return fmt.Errorf("route metadata %s is not a Revision", entry.Name())
		}
		var revision RevisionRoute
		if err := json.Unmarshal(data, &revision); err != nil {
			return fmt.Errorf("decode Revision route %s: %w", entry.Name(), err)
		}
		if err := revision.Validate(); err != nil {
			return fmt.Errorf("Revision route %s: %w", entry.Name(), err)
		}
		key := revision.Namespace + "/" + revision.Revision
		if _, exists := revisions[key]; exists {
			return fmt.Errorf("duplicate Revision route %q", key)
		}
		revisions[key] = revision
		poolKey := revision.Namespace + "--" + revision.Revision
		if revision.Routed {
			retain[poolKey] = true
			routed[key] = true
			execution, _ := (project.Manifest{Execution: &revision.Execution}).EffectiveExecution()
			idle[poolKey], _ = time.ParseDuration(execution.IdleTimeout)
		}
	}
	router.mu.Lock()
	for key, previous := range router.revisions {
		if _, kept := revisions[key]; !kept {
			delete(router.generations, key)
			forgetRevision(previous.Namespace, previous.Revision)
		}
	}
	for key := range revisions {
		if _, known := router.generations[key]; !known {
			router.nextGeneration++
			router.generations[key] = router.nextGeneration
		}
	}
	router.revisions = revisions
	var removed []*routerPool
	for key, pool := range router.pools {
		if !retain[key] {
			delete(router.pools, key)
			removed = append(removed, pool)
			continue
		}
		pool.mu.Lock()
		pool.idle = idle[key]
		pool.mu.Unlock()
	}
	router.mu.Unlock()
	for _, pool := range removed {
		router.endPool(pool, true, time.Now())
	}
	router.homes.Retain(routed)
	return nil
}

func (router *Router) Handler() http.Handler { return probe.NewHandler(router) }

func (router *Router) Run(ctx context.Context) {
	ticker := time.NewTicker(500 * time.Millisecond)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case now := <-ticker.C:
			if err := router.Reload(); err != nil {
				log.Printf("reload protected routes: %v", err)
			}
			router.mu.RLock()
			pools := make([]*routerPool, 0, len(router.pools))
			for _, pool := range router.pools {
				pools = append(pools, pool)
			}
			router.mu.RUnlock()
			for _, pool := range pools {
				router.endPool(pool, false, now)
			}
		}
	}
}

func (router *Router) endPool(pool *routerPool, closePool bool, now time.Time) {
	pool.mu.Lock()
	pool.closed = pool.closed || closePool
	var released []*routerSlot
	retained := pool.slots[:0]
	for _, slot := range pool.slots {
		if !slot.busy && (pool.closed || now.Sub(slot.lastUse) >= pool.idle) {
			released = append(released, slot)
		} else {
			retained = append(retained, slot)
		}
	}
	pool.slots = retained
	pool.mu.Unlock()
	for _, slot := range released {
		router.endSlotLater(slot.client, slot.fiber.ID)
	}
}

// endSlotLater is endSlot in the background, so a frozen home, whose
// release takes its whole timeout, holds up neither a request nor the
// reload loop's sweeps.
func (router *Router) endSlotLater(client FiberClient, id string) {
	if client == nil {
		return
	}
	if !router.enter() {
		router.endSlot(client, id)
		return
	}
	go func() {
		defer router.releases.Done()
		router.endSlot(client, id)
	}()
}

// enter joins releases, unless Close has begun.
func (router *Router) enter() bool {
	router.releaseMu.Lock()
	defer router.releaseMu.Unlock()
	if router.closing {
		return false
	}
	router.releases.Add(1)
	return true
}

// endSlot releases a slot's fiber, if it has one, and drops the slot's
// hold on its home's connection.
func (router *Router) endSlot(client FiberClient, id string) {
	if client == nil {
		return
	}
	if id != "" {
		router.release(client, id)
	}
	drop(client)
}

// resetConnectBackoff has client's connection try its home again at once,
// if client has a connection that backs off.
func resetConnectBackoff(client FiberClient) {
	if connection, ok := client.(interface{ ResetConnectBackoff() }); ok {
		connection.ResetConnectBackoff()
	}
}

func hold(client FiberClient) error {
	if held, ok := client.(heldClient); ok {
		return held.Hold()
	}
	return nil
}

func drop(client FiberClient) {
	if held, ok := client.(heldClient); ok {
		held.Drop()
	}
}

func (router *Router) release(client FiberClient, id string) {
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	if err := client.Release(ctx, id, true); err != nil && !consumer.NotFound(err) {
		log.Printf("release fiber %s: %v", id, err)
	}
}

func (router *Router) Close() error {
	router.mu.Lock()
	for _, pool := range router.pools {
		router.endPool(pool, true, time.Now())
	}
	router.mu.Unlock()
	router.releaseMu.Lock()
	router.closing = true
	router.releaseMu.Unlock()
	router.releases.Wait()
	return router.homes.Close()
}

// acquire resolves a home within ctx and clones within the request's own
// context, which also tells a home that failed from a request that ended.
// The first Clone gets its own AcquireTimeout, starting after the home is
// resolved and any stale fiber let go, so a Clone that runs out of it hung
// on its home. Retries share that deadline: a Clone the home's clone rate
// limit refused waits the refusal's Retry-After and tries again, one the
// home's helper refused for restoring too many Fibers waits a moment and
// tries again, and one no home received, as when a rollout has replaced the
// grant's Pod, asks for the grant's homes again and tries the one named.
// retried, if set, is told of each retry.
func (router *Router) acquire(ctx, request context.Context, namespace, revision string, execution project.ExecutionPolicy, retried func(reason string)) (consumer.Fiber, func(bool), error) {
	// The request holds the connection from before its clone until its
	// fiber is released, or hands the hold to its pooled slot, so a prune
	// never closes it in between.
	grant, err := router.resolveHeld(ctx, namespace, revision)
	if err != nil {
		return consumer.Fiber{}, nil, err
	}
	deadline := time.Now().Add(router.AcquireTimeout)
	for attempt := 0; ; attempt++ {
		fiber, finish, err := router.clone(request, deadline, attempt == 0, namespace, revision, execution, grant)
		if err == nil {
			return fiber, finish, nil
		}
		var reason string
		var wait time.Duration
		if after, limited := rateLimited(err); limited {
			reason, wait = retryRateLimit, jitter(after)
		} else if restoresBusy(err) {
			reason, wait = retryRestoresBusy, restoresBusyBackoff()
		} else if unsent(request, err) {
			reason = retryUnreachable
		} else {
			return consumer.Fiber{}, nil, err
		}
		if !pause(request, deadline, wait) {
			return consumer.Fiber{}, nil, err
		}
		failed := grant.UID
		retryCtx, cancel := context.WithDeadline(request, deadline)
		var resolveErr error
		grant, resolveErr = router.resolveHeld(retryCtx, namespace, revision)
		cancel()
		if resolveErr != nil {
			// The Clone's own error says why there is no Fiber: a lapsed
			// deadline here only means the retry ran out of time.
			if retryCtx.Err() != nil {
				return consumer.Fiber{}, nil, err
			}
			return consumer.Fiber{}, nil, resolveErr
		}
		// A fresh answer that still names the home no Clone reached, as
		// while its Pod is listed, gets a moment before the next try, so
		// the request does not spin on a connection that fails fast.
		if reason == retryUnreachable && grant.UID == failed {
			if !pause(request, deadline, jitter(unreachableBackoff)) {
				drop(grant.Client)
				return consumer.Fiber{}, nil, err
			}
			// The Pod kept its IP, so it may be back already: try it
			// now rather than when its connection's backoff ends.
			resetConnectBackoff(grant.Client)
		}
		if retried != nil {
			retried(reason)
		}
	}
}

// Why acquire retried a Clone, as the clone retry metric names it.
const (
	retryRateLimit    = "rate_limit"
	retryRestoresBusy = "restores_busy"
	retryUnreachable  = "home_unreachable"
)

// restoresBusyBackoff is how long a retry waits after a home's helper
// refused a Clone for restoring too many Fibers: long enough for a restore
// or two to finish, and spread so a refused burst does not come back at
// once. The helper gives no Retry-After.
func restoresBusyBackoff() time.Duration {
	return 250*time.Millisecond + rand.N(750*time.Millisecond)
}

// unreachableBackoff is how long a retry waits before it tries again a home
// no Clone reached, once a fresh answer still names it.
const unreachableBackoff = 250 * time.Millisecond

// rateLimited reports a home's clone rate limit refusing a Clone, and the
// refusal's Retry-After. fiberd sends it as the same SHED miss as memory or
// task pressure, so only its message, core.ErrRateLimit's, tells it apart.
// It limits how fast Fibers start and attach, up to 200 a second, not how
// many run, and at its refill rate it clears within a second, so the
// request waits rather than being shed.
func rateLimited(err error) (time.Duration, bool) {
	var shed *consumer.Shed
	if errors.As(err, &shed) && strings.Contains(shed.Reason, core.ErrRateLimit.Error()) {
		return max(shed.RetryAfter, time.Second), true
	}
	return 0, false
}

// unsent reports a Clone no home received (see notSent) while the request
// itself still runs. Such a Clone cannot have started a Fiber, so trying
// again cannot start a second one.
func unsent(request context.Context, err error) bool {
	var unsent *notSent
	return request.Err() == nil && errors.As(err, &unsent)
}

// jitter spreads a wait over half to one and a half times d, so the
// requests one refusal turned away do not all come back at once.
func jitter(d time.Duration) time.Duration {
	if d <= 0 {
		return 0
	}
	return d/2 + rand.N(d)
}

// pause waits for d unless that would reach the deadline or the request
// ends first, and reports whether to try again.
func pause(request context.Context, deadline time.Time, d time.Duration) bool {
	if !time.Now().Add(d).Before(deadline) {
		return false
	}
	if d <= 0 {
		return request.Err() == nil
	}
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-timer.C:
		return true
	case <-request.Done():
		return false
	}
}

// clone clones a Fiber on grant's home, whose connection the caller holds,
// by deadline. whole says the Clone has the whole AcquireTimeout: a retry
// that runs out of what was left of it says nothing about the home. The
// Clone's time.Until(deadline) may be taken before its connection is up:
// the client turns it back into deadline before it waits, and fiberd
// measures what is left of that from when the Clone arrives, so a slow
// connect does not give the helper more time than the request has.
func (router *Router) clone(request context.Context, deadline time.Time, whole bool, namespace, revision string, execution project.ExecutionPolicy, grant HomeGrant) (consumer.Fiber, func(bool), error) {
	uid := namespace + "--" + revision
	if execution.Mode == project.ExecutionFresh {
		cloneCtx, cancel := context.WithDeadline(request, deadline)
		fiber, err := grant.Client.Clone(cloneCtx, grant.Token, "", time.Until(deadline), nil)
		cut := !whole && cloneCtx.Err() != nil
		cancel()
		if err != nil {
			drop(grant.Client)
			if !cut {
				router.forgetHome(request, namespace, revision, grant.UID, err)
			}
			return consumer.Fiber{}, nil, err
		}
		return fiber, func(bool) { router.endSlot(grant.Client, fiber.ID) }, nil
	}
	idle, err := time.ParseDuration(execution.IdleTimeout)
	if err != nil {
		drop(grant.Client)
		return consumer.Fiber{}, nil, err
	}
	router.mu.Lock()
	pool := router.pools[uid]
	if pool == nil {
		pool = &routerPool{idle: idle}
		router.pools[uid] = pool
	}
	router.mu.Unlock()
	pool.mu.Lock()
	if pool.closed {
		pool.mu.Unlock()
		drop(grant.Client)
		return consumer.Fiber{}, nil, fmt.Errorf("revision is draining")
	}
	var selected *routerSlot
	for _, slot := range pool.slots {
		if !slot.busy {
			selected = slot
			break
		}
	}
	if selected == nil {
		if len(pool.slots) >= grant.MaxFibers {
			pool.mu.Unlock()
			drop(grant.Client)
			return consumer.Fiber{}, nil, &consumer.Shed{RetryAfter: time.Second}
		}
		selected = &routerSlot{session: fmt.Sprintf("%s-%d", grant.UID, router.sequence.Add(1))}
		pool.slots = append(pool.slots, selected)
	}
	selected.busy = true
	// A slot's session lives on the home it was first cloned on, and the
	// slot holds that home's connection for as long as it keeps the
	// session. Once that home is no longer the one served, its fiber is
	// released there and the slot starts a new session, so no session runs
	// on two homes.
	var stale routerSlot
	if selected.client != nil && selected.client != grant.Client {
		stale = *selected
		selected.session = fmt.Sprintf("%s-%d", grant.UID, router.sequence.Add(1))
		selected.fiber, selected.client = consumer.Fiber{}, nil
	}
	// The slot keeps one hold on its home's connection: one that already
	// holds it lets go of the request's, and an empty one takes it.
	held := selected.client != nil
	if !held {
		selected.client = grant.Client
	}
	pool.mu.Unlock()
	if held {
		drop(grant.Client)
	}
	// Off the request path, since a hung home would hold it for the whole
	// release timeout. The stale slot's hold keeps that connection open
	// until the release ends, and the new session has a name of its own.
	router.endSlotLater(stale.client, stale.fiber.ID)
	finish := func(gone bool) {
		pool.mu.Lock()
		selected.busy = false
		selected.lastUse = time.Now()
		destroy := gone || pool.closed
		if destroy {
			for index, slot := range pool.slots {
				if slot == selected {
					pool.slots = append(pool.slots[:index], pool.slots[index+1:]...)
					break
				}
			}
		}
		client, id := selected.client, selected.fiber.ID
		pool.mu.Unlock()
		if destroy {
			router.endSlot(client, id)
		}
	}
	cloneCtx, cancel := context.WithDeadline(request, deadline)
	fiber, cloneErr := grant.Client.Clone(cloneCtx, grant.Token, selected.session, time.Until(deadline), nil)
	cut := !whole && cloneCtx.Err() != nil
	cancel()
	if cloneErr != nil {
		// A home that refused for backpressure did nothing to the session:
		// the slot and its running Fiber stay for the retry, which attaches
		// rather than restoring a new one.
		finish(!backpressure(cloneErr))
		if !cut {
			router.forgetHome(request, namespace, revision, grant.UID, cloneErr)
		}
		return consumer.Fiber{}, nil, cloneErr
	}
	pool.mu.Lock()
	selected.fiber = fiber
	pool.mu.Unlock()
	return fiber, finish, nil
}

// resolveHeld resolves a home and holds its connection. A refresh can
// prune the connection Resolve returned before it is held, but a prune
// comes only after the cache names the homes that replace it, so one more
// Resolve finds a connection that is open.
func (router *Router) resolveHeld(ctx context.Context, namespace, revision string) (HomeGrant, error) {
	for attempt := 0; ; attempt++ {
		grant, err := router.homes.Resolve(ctx, namespace, revision)
		if err != nil {
			return HomeGrant{}, err
		}
		if err = hold(grant.Client); err == nil {
			return grant, nil
		}
		if attempt > 0 {
			router.homes.Forget(namespace, revision, "")
			return HomeGrant{}, err
		}
	}
}

// forgetHome makes the next clone ask for the grant's homes again and go to
// another home meanwhile, unless the home only shed load or had its grant
// at fibers.max (see shedRetryAfter): a home that refused the grant or went
// away is one a rollout has replaced, has not admitted yet, or whose token
// has lapsed. A request that ended, by its client leaving or its own
// timeout, says nothing about the home, so it neither passes the home over
// nor asks the controller again: a burst of clients leaving would
// otherwise refresh the grant once for each.
func (router *Router) forgetHome(request context.Context, namespace, revision, uid string, err error) {
	if _, shed := shedRetryAfter(err); shed {
		return
	}
	if request.Err() != nil || errors.Is(err, context.Canceled) || status.Code(err) == codes.Canceled {
		return
	}
	router.homes.Forget(namespace, revision, uid)
}

func (router *Router) ServeHTTP(w http.ResponseWriter, request *http.Request) {
	if !router.requests.Begin() {
		http.Error(w, "router is shutting down", http.StatusServiceUnavailable)
		return
	}
	defer router.requests.Done()
	requestStarted := time.Now()
	namespace := request.Header.Get("Knative-Serving-Namespace")
	revisionName := request.Header.Get("Knative-Serving-Revision")
	selected := namespace != "" || revisionName != ""
	if !selected && request.URL.Path == "/healthz" {
		w.WriteHeader(http.StatusOK)
		return
	}
	if !selected && request.URL.Path == "/readyz" {
		probeNamespace := request.URL.Query().Get("namespace")
		probeRevision := request.URL.Query().Get("revision")
		if probeNamespace != "" || probeRevision != "" {
			router.mu.RLock()
			current, exists := router.revisions[probeNamespace+"/"+probeRevision]
			router.mu.RUnlock()
			if !exists || current.Version() != request.URL.Query().Get("version") {
				http.Error(w, "Revision projection is not current", http.StatusServiceUnavailable)
			} else {
				fmt.Fprintln(w, "ready")
			}
			return
		}
	}
	if !grantName.MatchString(namespace) || !grantName.MatchString(revisionName) {
		http.Error(w, "invalid Knative Revision selection", http.StatusBadRequest)
		return
	}
	router.mu.RLock()
	revision, known := router.revisions[namespace+"/"+revisionName]
	router.mu.RUnlock()
	if !known {
		http.Error(w, "Knative Revision is not ready", http.StatusServiceUnavailable)
		return
	}
	metric := RequestMetric{
		Service: revision.Revision,
		Method:  request.Method,
		Path:    request.URL.RequestURI(),
	}
	request.Header.Del("Knative-Serving-Namespace")
	request.Header.Del("Knative-Serving-Revision")
	netheader.RewriteHostOut(request)
	// Labels come only from Revisions the router has a route for, and are
	// recorded only while that route still exists.
	metrics := router.metricsFor(namespace, revision.Revision)
	recorder := &statusRecorder{ResponseWriter: w}
	w = recorder
	clientCtx := request.Context()
	var revisionCtx context.Context
	received := time.Now()
	defer func() {
		status := requestStatus(recorder, clientCtx, revisionCtx)
		metrics.record(func() {
			observeRequest(namespace, revision.Revision, status, received)
		})
		if router.observer != nil {
			metric.Status = status
			metric.Duration = time.Since(requestStarted)
			router.observer(metric)
		}
	}()
	execution, err := (project.Manifest{Execution: &revision.Execution}).EffectiveExecution()
	if err != nil {
		http.Error(w, err.Error(), http.StatusServiceUnavailable)
		return
	}
	// Close waits for the request until its fiber is released, after the
	// proxy returns, so no fiber outlives the router on its home. The
	// server ends the request's context when its drain runs out.
	if !router.enter() {
		http.Error(w, "fiber-router is shutting down", http.StatusServiceUnavailable)
		return
	}
	defer router.releases.Done()
	timeout, _ := time.ParseDuration(revision.Timeout)
	ctx, cancel := context.WithTimeout(request.Context(), timeout)
	defer cancel()
	revisionCtx = ctx
	request = request.WithContext(ctx)
	start := time.Now()
	acquireCtx, stopAcquire := context.WithTimeout(ctx, router.AcquireTimeout)
	fiber, finish, err := router.acquire(acquireCtx, ctx, namespace, revision.Revision, execution, func(reason string) {
		metrics.record(func() { cloneRetriesTotal.WithLabelValues(namespace, revision.Revision, reason).Inc() })
	})
	stopAcquire()
	metric.CloneDuration = time.Since(start)
	if err != nil {
		recorder.failed = true
		outcome := "error"
		if clientCtx.Err() != nil {
			outcome = "canceled"
		}
		metrics.record(func() {
			fiberAcquireSeconds.WithLabelValues(namespace, revision.Revision, outcome).Observe(time.Since(start).Seconds())
		})
		// The error names the controller, the grant and its Pods'
		// addresses, which are the operator's to read, not the client's.
		if errors.Is(ctx.Err(), context.DeadlineExceeded) {
			log.Printf("acquire revision %s/%s fiber: %v", namespace, revision.Revision, err)
			http.Error(w, "the Revision's timeout passed before a Fiber was available", http.StatusGatewayTimeout)
			return
		}
		var refused *GrantsRefused
		if errors.As(err, &refused) {
			w.Header().Set("Retry-After", fmt.Sprintf("%.0f", refused.RetryAfter.Seconds()))
			log.Printf("acquire revision %s/%s fiber: %v", namespace, revision.Revision, err)
		} else if retryAfter, shed := shedRetryAfter(err); shed {
			// Counted rather than logged: a home sheds under load, when a
			// line per request would flood the log. A client that left
			// while the request waited out a refusal is counted as
			// canceled, not as the home's shed.
			if clientCtx.Err() == nil {
				metrics.record(func() { shedTotal.WithLabelValues(namespace, revision.Revision).Inc() })
			}
			w.Header().Set("Retry-After", fmt.Sprintf("%.0f", retryAfter.Seconds()))
		} else {
			log.Printf("acquire revision %s/%s fiber: %v", namespace, revision.Revision, err)
		}
		http.Error(w, "no Fiber is available for this Revision", http.StatusServiceUnavailable)
		return
	}
	acquired := metric.CloneDuration
	cloneMS := acquired.Milliseconds()
	metrics.record(func() {
		fiberAcquireSeconds.WithLabelValues(namespace, revision.Revision, fiber.Kind.String()).Observe(acquired.Seconds())
		fibersBusy.WithLabelValues(namespace, revision.Revision).Inc()
	})
	metric.FiberID = fiber.ID
	metric.FiberKind = fiber.Kind.String()
	gone := false
	defer func() {
		metrics.record(func() { fibersBusy.WithLabelValues(namespace, revision.Revision).Dec() })
		finish(gone)
	}()
	target, err := url.Parse(strings.Replace(fiber.Endpoint, "tcp://", "http://", 1))
	if err != nil || target.Scheme != "http" || target.Host == "" {
		gone = true
		http.Error(w, "Fiberd returned an invalid HTTP fiber endpoint", http.StatusBadGateway)
		return
	}
	proxy := httputil.NewSingleHostReverseProxy(target)
	proxy.Transport = router.transport
	if request.ProtoMajor == 2 {
		proxy.Transport = router.h2cTransport
	}
	proxy.FlushInterval = -1
	proxy.ModifyResponse = func(response *http.Response) error {
		if _, present := response.Header["Content-Type"]; !present {
			// Preserve untyped streams instead of letting net/http sniff HTML.
			w.Header()["Content-Type"] = nil
		}
		response.Header.Set("X-Fiberd-Clone-Ms", fmt.Sprint(cloneMS))
		response.Header.Set("X-Fiberd-Clone", fiber.Kind.String())
		return nil
	}
	proxy.ErrorHandler = func(w http.ResponseWriter, _ *http.Request, err error) {
		gone = true
		recorder.failed = true
		log.Printf("proxy revision %s fiber %s endpoint %s: %v", revision.Revision, fiber.ID, fiber.Endpoint, err)
		// The error names the Fiber's address on its home, which is the
		// operator's to read, in the log above, not the client's.
		if errors.Is(ctx.Err(), context.DeadlineExceeded) ||
			errors.Is(err, context.DeadlineExceeded) {
			http.Error(w, "the Revision's timeout passed before its Fiber answered", http.StatusGatewayTimeout)
			return
		}
		http.Error(w, "the Revision's Fiber did not answer", http.StatusBadGateway)
	}
	// Once the response has started, ReverseProxy reports a broken copy
	// (client or Fiber gone) by panicking with ErrAbortHandler. Note it, so
	// the request isn't recorded as its upstream status and the Fiber isn't
	// pooled again, then let the server abort the connection.
	defer func() {
		if failure := recover(); failure != nil {
			if failure == http.ErrAbortHandler {
				gone = true
				recorder.failed = true
			}
			panic(failure)
		}
	}()
	proxy.ServeHTTP(w, request)
}
