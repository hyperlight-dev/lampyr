// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"io"
	"math/big"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	grantv1 "github.com/Azure/fiberd/api/grant/v1"
	"github.com/Azure/fiberd/pkg/core"
	consumer "github.com/Azure/fiberd/sdk/go/client"
	"github.com/hyperlight-dev/lampyr/internal/project"
	"google.golang.org/grpc"
	grpccodes "google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/rest"
)

// serveDemoCapacityGrant answers as the API serves default/demo with a cap
// of 4.
func serveDemoCapacityGrant(w http.ResponseWriter) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(map[string]any{
		"apiVersion": "fiberd.io/v1alpha1", "kind": "CapacityGrant",
		"metadata": map[string]any{"namespace": "default", "name": "demo"},
		"spec":     map[string]any{"fibers": map[string]any{"max": 4}},
	})
}

func apiAt(t *testing.T, url string) dynamic.Interface {
	t.Helper()
	client, err := dynamic.NewForConfig(&rest.Config{Host: url})
	if err != nil {
		t.Fatal(err)
	}
	return client
}

type dialledHome struct {
	address    string
	serverName string
	closed     *atomic.Bool
	released   *atomic.Int32
}

func (home dialledHome) Clone(context.Context, string, string, time.Duration, []byte) (consumer.Fiber, error) {
	return consumer.Fiber{ID: "fiber-" + home.address}, nil
}

func (home dialledHome) Release(context.Context, string, bool) error {
	if home.closed.Load() {
		return errors.New("client connection is closing")
	}
	home.released.Add(1)
	return nil
}

func (home dialledHome) Close() error {
	home.closed.Store(true)
	return nil
}

func dialled(home HomeGrant) dialledHome {
	return home.Client.(*homeConnection).FiberClient.(dialledHome)
}

// controllerFixture serves a CapacityGrant with a cap of 4 and the caller
// grants answer returns, counting the caller grant requests.
type controllerFixture struct {
	mu     sync.Mutex
	asked  int
	answer func() (int, []callerGrant)
	gate   chan struct{}
}

func (fixture *controllerFixture) start(t *testing.T) *CallerGrants {
	t.Helper()
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		if strings.HasPrefix(request.URL.Path, "/apis/") {
			if request.URL.Path != "/apis/fiberd.io/v1alpha1/namespaces/default/capacitygrants/demo" {
				http.NotFound(w, request)
				return
			}
			serveDemoCapacityGrant(w)
			return
		}
		if request.Method != http.MethodPost || request.URL.Path != callerGrantPath {
			t.Errorf("unexpected caller grant request %s %s", request.Method, request.URL.Path)
		}
		var body map[string]string
		json.NewDecoder(request.Body).Decode(&body)
		if body["namespace"] != "default" || body["name"] != "demo" {
			t.Errorf("caller grant body = %#v", body)
		}
		fixture.mu.Lock()
		fixture.asked++
		gate := fixture.gate
		status, homes := fixture.answer()
		fixture.mu.Unlock()
		if gate != nil {
			<-gate
		}
		if status == http.StatusTooManyRequests {
			w.Header().Set("Retry-After", "2")
			w.WriteHeader(status)
			w.Write([]byte(`{"error":"RateLimited"}`))
			return
		}
		json.NewEncoder(w).Encode(map[string]any{"serverName": "fiberd-demo.default.svc", "grants": append([]callerGrant{}, homes...)})
	}))
	t.Cleanup(server.Close)
	return &CallerGrants{
		API: apiAt(t, server.URL), Issuer: server.URL,
		HTTP: server.Client(),
		Dial: func(_ context.Context, address, serverName string) (FiberClient, error) {
			return dialledHome{address: address, serverName: serverName, closed: new(atomic.Bool), released: new(atomic.Int32)}, nil
		},
	}
}

func (fixture *controllerFixture) set(status int, homes ...callerGrant) {
	fixture.mu.Lock()
	defer fixture.mu.Unlock()
	fixture.answer = func() (int, []callerGrant) { return status, homes }
}

func (fixture *controllerFixture) count() int {
	fixture.mu.Lock()
	defer fixture.mu.Unlock()
	return fixture.asked
}

// settle waits for the refresh in flight, if any, to end.
func settle(grants *CallerGrants) {
	cache := grants.cache("default/demo")
	cache.mu.Lock()
	asking := cache.asking
	cache.mu.Unlock()
	if asking != nil {
		<-asking
	}
}

func testHome(uid, address, token string) callerGrant {
	return callerGrant{UID: uid, Address: address, Token: token, ExpiresAt: time.Now().Add(time.Hour)}
}

func TestCallerGrantsAskTheControllerAndCacheUntilHalfExpiry(t *testing.T) {
	fixture := &controllerFixture{}
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
	grants := fixture.start(t)
	resolved, err := grants.Resolve(context.Background(), "default", "demo")
	if err != nil {
		t.Fatal(err)
	}
	if home := dialled(resolved); resolved.UID != "uid-a" || resolved.Token != "token-a" || resolved.MaxFibers != 4 ||
		home.address != "10.0.0.1:8484" || home.serverName != "fiberd-demo.default.svc" {
		t.Fatalf("home = %#v dialled at %#v", resolved, home)
	}
	if _, err := grants.Resolve(context.Background(), "default", "demo"); err != nil {
		t.Fatal(err)
	}
	if asked := fixture.count(); asked != 1 {
		t.Fatalf("asked the controller %d times before half expiry, want 1", asked)
	}

	// A failed home makes the next Resolve ask again. The served home is
	// the one with the lowest Pod UID, so pooled sessions stay on one Pod.
	fixture.set(http.StatusOK, testHome("uid-b", "10.0.0.2:8484", "token-b"), testHome("uid-a", "10.0.0.1:8484", "token-a2"))
	grants.Forget("default", "demo", "")
	grants.Resolve(context.Background(), "default", "demo")
	settle(grants)
	for range 3 {
		resolved, err := grants.Resolve(context.Background(), "default", "demo")
		if err != nil || resolved.UID != "uid-a" || resolved.Token != "token-a2" {
			t.Fatalf("home = %#v, %v, want uid-a with its renewed token", resolved, err)
		}
	}
	if asked := fixture.count(); asked != 2 {
		t.Fatalf("asked %d times after a failed home, want 2", asked)
	}

	// A refused answer keeps the held homes in use and holds off asking
	// until Retry-After has passed.
	fixture.set(http.StatusTooManyRequests)
	grants.Forget("default", "demo", "")
	for range 3 {
		resolved, err := grants.Resolve(context.Background(), "default", "demo")
		if err != nil || resolved.Token != "token-a2" {
			t.Fatalf("rate-limited refresh = %#v, %v, want the held home", resolved, err)
		}
		settle(grants)
	}
	cache := grants.cache("default/demo")
	if asked := fixture.count(); asked != 3 || time.Until(cache.nextAsk) < time.Second {
		t.Fatalf("asked %d times with next ask in %s, want 3 and about 2s", asked, time.Until(cache.nextAsk))
	}

	// An empty answer keeps them too, until their tokens lapse.
	fixture.set(http.StatusOK)
	cache.mu.Lock()
	cache.nextAsk = time.Time{}
	cache.mu.Unlock()
	if resolved, err := grants.Resolve(context.Background(), "default", "demo"); err != nil || resolved.Token != "token-a2" {
		t.Fatalf("empty refresh = %#v, %v, want the held home", resolved, err)
	}
	settle(grants)
	cache.mu.Lock()
	cache.nextAsk = time.Time{}
	for i := range cache.homes {
		cache.homes[i].expiresAt = time.Now().Add(-time.Second)
	}
	cache.mu.Unlock()
	if _, err := grants.Resolve(context.Background(), "default", "demo"); err == nil {
		t.Fatal("a CapacityGrant whose held tokens lapsed resolved without a ready home")
	}
	fixture.set(http.StatusTooManyRequests)
	cache.mu.Lock()
	cache.nextAsk = time.Time{}
	cache.mu.Unlock()
	var refused *GrantsRefused
	_, err = grants.Resolve(context.Background(), "default", "demo")
	if !errors.As(err, &refused) || refused.RetryAfter != 2*time.Second ||
		refused.Issuer != grants.Issuer || refused.Grant != "default/demo" ||
		refused.Status != "429 Too Many Requests" || refused.Reason != "RateLimited" {
		t.Fatalf("rate-limited caller grant without held homes = %#v, want the controller's refusal with Retry-After", err)
	}
	var shed *consumer.Shed
	if errors.As(err, &shed) {
		t.Fatalf("the controller's refusal %v is a home's shed", err)
	}
}

// issueCert signs a certificate over key with ca, or self-signs it when ca
// is nil, and returns it in PEM.
func issueCert(t *testing.T, template *x509.Certificate, key *ecdsa.PrivateKey, ca *x509.Certificate, caKey *ecdsa.PrivateKey) (*x509.Certificate, []byte) {
	t.Helper()
	template.SerialNumber = big.NewInt(time.Now().UnixNano())
	template.NotBefore, template.NotAfter = time.Now().Add(-time.Minute), time.Now().Add(time.Hour)
	if ca == nil {
		ca, caKey = template, key
	}
	der, err := x509.CreateCertificate(rand.Reader, template, ca, &key.PublicKey, caKey)
	if err != nil {
		t.Fatal(err)
	}
	cert, err := x509.ParseCertificate(der)
	if err != nil {
		t.Fatal(err)
	}
	return cert, pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
}

func newKey(t *testing.T) *ecdsa.PrivateKey {
	t.Helper()
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	return key
}

func TestCallerGrantsAskOverTheRoutersClientCertificate(t *testing.T) {
	caKey := newKey(t)
	ca, caPEM := issueCert(t, &x509.Certificate{Subject: pkix.Name{CommonName: "fiberd-ca"}, IsCA: true, BasicConstraintsValid: true, KeyUsage: x509.KeyUsageCertSign}, caKey, nil, nil)
	serverKey := newKey(t)
	server, _ := issueCert(t, &x509.Certificate{IPAddresses: []net.IP{net.IPv4(127, 0, 0, 1)}, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth}}, serverKey, ca, caKey)
	routerKey := newKey(t)
	client := func(name string, key *ecdsa.PrivateKey) []byte {
		_, certPEM := issueCert(t, &x509.Certificate{Subject: pkix.Name{CommonName: name}, ExtKeyUsage: []x509.ExtKeyUsage{x509.ExtKeyUsageClientAuth}}, key, ca, caKey)
		return certPEM
	}
	keyPEM := func(key *ecdsa.PrivateKey) []byte {
		der, err := x509.MarshalPKCS8PrivateKey(key)
		if err != nil {
			t.Fatal(err)
		}
		return pem.EncodeToMemory(&pem.Block{Type: "PRIVATE KEY", Bytes: der})
	}
	dir := t.TempDir()
	write := func(name string, data []byte) string {
		t.Helper()
		path := filepath.Join(dir, name)
		if err := os.WriteFile(path, data, 0o600); err != nil {
			t.Fatal(err)
		}
		return path
	}
	certFile, keyFile, caFile := write("tls.crt", client("router", routerKey)), write("tls.key", keyPEM(routerKey)), write("ca.crt", caPEM)

	pool := x509.NewCertPool()
	pool.AddCert(ca)
	var seen []string
	var mu sync.Mutex
	named := &routerKey.PublicKey
	issuer := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		peer := request.TLS.PeerCertificates[0]
		mu.Lock()
		seen = append(seen, peer.Subject.CommonName)
		if request.ProtoMajor != 2 {
			t.Errorf("caller grants asked over %s, want HTTP/2", request.Proto)
		}
		forbidden := !named.Equal(peer.PublicKey)
		mu.Unlock()
		if forbidden {
			w.WriteHeader(http.StatusForbidden)
			w.Write([]byte(`{"error":"Forbidden"}`))
			return
		}
		json.NewEncoder(w).Encode(map[string]any{"serverName": "fiberd-demo.default.svc", "grants": []callerGrant{testHome("uid-a", "10.0.0.1:8484", "token-a")}})
	}))
	// The controller serves h2, where a refused stream keeps the
	// connection until its body is closed.
	issuer.EnableHTTP2 = true
	issuer.TLS = &tls.Config{
		Certificates: []tls.Certificate{{Certificate: [][]byte{server.Raw}, PrivateKey: serverKey}},
		ClientCAs:    pool, ClientAuth: tls.RequireAndVerifyClientCert,
	}
	issuer.StartTLS()
	t.Cleanup(issuer.Close)
	api := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		serveDemoCapacityGrant(w)
	}))
	t.Cleanup(api.Close)
	grants := &CallerGrants{
		API: apiAt(t, api.URL), Issuer: issuer.URL,
		CertFile: certFile, KeyFile: keyFile, CAFile: caFile,
		Dial: func(_ context.Context, address, serverName string) (FiberClient, error) {
			return dialledHome{address: address, serverName: serverName, closed: new(atomic.Bool), released: new(atomic.Int32)}, nil
		},
	}
	if _, err := grants.Resolve(context.Background(), "default", "demo"); err != nil {
		t.Fatal(err)
	}
	// A renewal over the same key is presented on the next handshake,
	// without a restart.
	write("tls.crt", client("router-renewed", routerKey))
	grants.client.CloseIdleConnections()
	grants.Forget("default", "demo", "")
	grants.Resolve(context.Background(), "default", "demo")
	settle(grants)

	// A new key, which the CapacityGrant names once the serving controller
	// sees it: the kept-alive connection is refused once, and the next ask
	// handshakes with the new key.
	rotated := newKey(t)
	write("tls.key", keyPEM(rotated))
	write("tls.crt", client("router-rotated", rotated))
	mu.Lock()
	named = &rotated.PublicKey
	mu.Unlock()
	cache := grants.cache("default/demo")
	for range 2 {
		grants.Forget("default", "demo", "")
		cache.mu.Lock()
		cache.nextAsk = time.Time{}
		cache.mu.Unlock()
		grants.Resolve(context.Background(), "default", "demo")
		settle(grants)
	}
	cache.mu.Lock()
	lastErr := cache.lastErr
	cache.mu.Unlock()
	mu.Lock()
	defer mu.Unlock()
	if want := []string{"router", "router-renewed", "router-renewed", "router-rotated"}; !slices.Equal(seen, want) || lastErr != nil {
		t.Fatalf("issuer saw client certificates %v, last error %v, want %v and an accepted ask", seen, lastErr, want)
	}
}

func TestCallerGrantsRefreshBeforeTheEarliestGrantLapses(t *testing.T) {
	fixture := &controllerFixture{}
	soon := testHome("uid-b", "10.0.0.2:8484", "token-b")
	soon.ExpiresAt = time.Now().Add(10 * time.Minute)
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"), soon)
	grants := fixture.start(t)
	if _, err := grants.Resolve(context.Background(), "default", "demo"); err != nil {
		t.Fatal(err)
	}
	cache := grants.cache("default/demo")
	cache.mu.Lock()
	until := time.Until(cache.refreshAt)
	cache.mu.Unlock()
	if until > 5*time.Minute || until < 4*time.Minute {
		t.Fatalf("refresh in %s, want half the life of the grant that lapses first", until)
	}
}

// A refusal served from the cache tells clients when its backoff ends,
// not the controller's Retry-After over again.
func TestCallerGrantsRetryAfterCountsDownTheBackoff(t *testing.T) {
	fixture := &controllerFixture{}
	fixture.set(http.StatusTooManyRequests)
	grants := fixture.start(t)
	var refused *GrantsRefused
	if _, err := grants.Resolve(context.Background(), "default", "demo"); !errors.As(err, &refused) || refused.RetryAfter != 2*time.Second {
		t.Fatalf("refusal = %v, want Retry-After 2s", err)
	}
	cache := grants.cache("default/demo")
	cache.mu.Lock()
	cache.nextAsk = time.Now().Add(300 * time.Millisecond)
	cache.mu.Unlock()
	if _, err := grants.Resolve(context.Background(), "default", "demo"); !errors.As(err, &refused) || refused.RetryAfter != time.Second {
		t.Fatalf("cached refusal = %v, want Retry-After 1s, what is left of the backoff rounded up", err)
	}
	if asked := fixture.count(); asked != 1 {
		t.Fatalf("asked %d times during the backoff, want 1", asked)
	}
}

// An answer whose grants have already lapsed, as when the router's clock
// runs ahead of the controller's, is not asked again on every request.
func TestCallerGrantsBackOffFromAnAnswerThatHasLapsed(t *testing.T) {
	fixture := &controllerFixture{}
	lapsed := testHome("uid-a", "10.0.0.1:8484", "token-a")
	lapsed.ExpiresAt = time.Now().Add(-time.Minute)
	fixture.set(http.StatusOK, lapsed)
	grants := fixture.start(t)
	for range 3 {
		if _, err := grants.Resolve(context.Background(), "default", "demo"); err == nil {
			t.Fatal("a lapsed grant resolved")
		}
		settle(grants)
	}
	cache := grants.cache("default/demo")
	cache.mu.Lock()
	refresh, next := time.Until(cache.refreshAt), time.Until(cache.nextAsk)
	cache.mu.Unlock()
	if asked := fixture.count(); asked != 1 || refresh <= 0 || next <= 0 {
		t.Fatalf("asked %d times, refresh in %s, next ask in %s, want one ask and a backoff", asked, refresh, next)
	}
}

func TestCallerGrantsRefreshOnceWhileServingTheHeldHomes(t *testing.T) {
	fixture := &controllerFixture{}
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
	grants := fixture.start(t)
	if _, err := grants.Resolve(context.Background(), "default", "demo"); err != nil {
		t.Fatal(err)
	}

	// While a slow refresh that will fail is in flight, every request is
	// served from the held homes and none starts another ask.
	gate := make(chan struct{})
	fixture.mu.Lock()
	fixture.gate = gate
	fixture.mu.Unlock()
	fixture.set(http.StatusTooManyRequests)
	grants.Forget("default", "demo", "")
	var waiters sync.WaitGroup
	for range 8 {
		waiters.Go(func() {
			resolved, err := grants.Resolve(context.Background(), "default", "demo")
			if err != nil || resolved.Token != "token-a" {
				t.Errorf("request during a refresh = %#v, %v, want the held home", resolved, err)
			}
		})
	}
	waiters.Wait()
	// A request whose context ends does not end the refresh it waited on.
	cancelled, cancel := context.WithCancel(context.Background())
	cancel()
	grants.cache("default/demo").mu.Lock()
	grants.cache("default/demo").homes = nil
	grants.cache("default/demo").mu.Unlock()
	if _, err := grants.Resolve(cancelled, "default", "demo"); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled request = %v, want its own cancellation", err)
	}
	close(gate)
	settle(grants)
	if asked := fixture.count(); asked != 2 {
		t.Fatalf("asked %d times, want one ask per backoff window", asked)
	}
	// The backoff runs from the slow answer, not from when it was asked.
	if until := time.Until(grants.cache("default/demo").nextAsk); until < time.Second {
		t.Fatalf("next ask in %s, want about Retry-After after the answer", until)
	}
	if _, err := grants.Resolve(context.Background(), "default", "demo"); err == nil {
		t.Fatal("a request without held homes resolved during the backoff")
	}
	if asked := fixture.count(); asked != 2 {
		t.Fatalf("asked %d times during the backoff, want 2", asked)
	}
}

func TestCallerGrantsCloseConnectionsNoGrantUses(t *testing.T) {
	fixture := &controllerFixture{}
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
	grants := fixture.start(t)
	first, err := grants.Resolve(context.Background(), "default", "demo")
	if err != nil {
		t.Fatal(err)
	}
	fixture.set(http.StatusOK, testHome("uid-b", "10.0.0.2:8484", "token-b"))
	grants.Forget("default", "demo", "")
	grants.Resolve(context.Background(), "default", "demo")
	settle(grants)
	if !dialled(first).closed.Load() {
		t.Fatal("the connection to a Pod no grant names stayed open")
	}
	second, err := grants.Resolve(context.Background(), "default", "demo")
	if err != nil || second.UID != "uid-b" {
		t.Fatalf("home = %#v, %v", second, err)
	}
	grants.Retain(map[string]bool{"default/other": true})
	if !dialled(second).closed.Load() || len(grants.homes) != 0 {
		t.Fatal("a Revision no longer routed kept its grant or connection")
	}
}

// A Retain while the controller is asked leaves the answer's waiters an
// open connection, and the next Retain still drops the grant.
func TestCallerGrantsServeARefreshThatARetainOverlaps(t *testing.T) {
	fixture := &controllerFixture{gate: make(chan struct{})}
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
	grants := fixture.start(t)
	resolved := make(chan HomeGrant)
	go func() {
		home, err := grants.Resolve(context.Background(), "default", "demo")
		if err != nil {
			t.Error(err)
		}
		resolved <- home
	}()
	for fixture.count() == 0 {
		runtime.Gosched()
	}
	grants.Retain(map[string]bool{})
	close(fixture.gate)
	home := <-resolved
	if err := hold(home.Client); err != nil {
		t.Fatalf("the waiter's home = %v, want its connection open", err)
	}
	drop(home.Client)
	grants.Retain(map[string]bool{})
	if !dialled(home).closed.Load() || len(grants.homes) != 0 {
		t.Fatal("a Revision no longer routed kept its grant or connection")
	}
}

func TestCallerGrantsKeepConnectionsUntilTheirFibersAreReleased(t *testing.T) {
	dropHold := drop
	for _, drop := range []string{"Revision leaves traffic", "Pod leaves the answer"} {
		fixture := &controllerFixture{}
		fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
		grants := fixture.start(t)
		held, err := grants.Resolve(context.Background(), "default", "demo")
		if err != nil {
			t.Fatal(err)
		}
		if err := hold(held.Client); err != nil {
			t.Fatal(err)
		}
		fiber, err := held.Client.Clone(context.Background(), held.Token, "", time.Second, nil)
		if err != nil {
			t.Fatal(err)
		}
		if drop == "Revision leaves traffic" {
			grants.Retain(map[string]bool{})
		} else {
			fixture.set(http.StatusOK, testHome("uid-b", "10.0.0.2:8484", "token-b"))
			grants.Forget("default", "demo", "")
			grants.Resolve(context.Background(), "default", "demo")
			settle(grants)
		}
		home := dialled(held)
		if home.closed.Load() {
			t.Fatalf("%s: the connection closed under a held fiber", drop)
		}
		if err := held.Client.Release(context.Background(), fiber.ID, true); err != nil || home.released.Load() != 1 {
			t.Fatalf("%s: release = %v after %d releases, want it to reach the home", drop, err, home.released.Load())
		}
		if home.closed.Load() {
			t.Fatalf("%s: the connection closed before its holder let go", drop)
		}
		dropHold(held.Client)
		if !home.closed.Load() {
			t.Fatalf("%s: the pruned connection stayed open after its last fiber", drop)
		}
	}
}

// servingHome is a grant Pod whose fibers serve HTTP, whose clones can be
// held back or refused, and whose every clone is a new fiber.
type servingHome struct {
	server  *httptest.Server
	mu      sync.Mutex
	next    int
	block   chan struct{}
	started chan struct{}
	fail    bool
	// cloneErr, when set, is what a Clone within ctx returns.
	cloneErr func(context.Context) error
	// cloneDelay is how long a healthy Clone takes, within its ctx.
	cloneDelay time.Duration
	// releaseHang, when set, holds every Release until it closes or the
	// Release's ctx ends, as a frozen Pod does.
	releaseHang chan struct{}
	released    []string
	closed      atomic.Bool
}

func (home *servingHome) Clone(ctx context.Context, _, _ string, _ time.Duration, _ []byte) (consumer.Fiber, error) {
	home.mu.Lock()
	block, started, fail, cloneErr, delay := home.block, home.started, home.fail, home.cloneErr, home.cloneDelay
	home.next++
	id := "fiber-" + string(rune('0'+home.next))
	home.mu.Unlock()
	if started != nil {
		close(started)
	}
	if block != nil {
		<-block
	}
	if fail {
		return consumer.Fiber{}, errors.New("clone refused")
	}
	if cloneErr != nil {
		return consumer.Fiber{}, cloneErr(ctx)
	}
	select {
	case <-time.After(delay):
	case <-ctx.Done():
		return consumer.Fiber{}, status.FromContextError(ctx.Err()).Err()
	}
	return consumer.Fiber{ID: id, Endpoint: strings.Replace(home.server.URL, "http://", "tcp://", 1)}, nil
}

func (home *servingHome) Release(ctx context.Context, id string, _ bool) error {
	home.mu.Lock()
	hang := home.releaseHang
	home.mu.Unlock()
	if hang != nil {
		select {
		case <-hang:
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	if home.closed.Load() {
		return errors.New("client connection is closing")
	}
	home.mu.Lock()
	defer home.mu.Unlock()
	home.released = append(home.released, id)
	return nil
}

func (home *servingHome) Close() error {
	home.closed.Store(true)
	return nil
}

func (home *servingHome) releases() []string {
	home.mu.Lock()
	defer home.mu.Unlock()
	return slices.Clone(home.released)
}

func routedCallerGrants(t *testing.T, mode string) (*Router, *CallerGrants, *servingHome, RevisionRoute) {
	t.Helper()
	router, _, revision := routerFixture(t, mode)
	revision.Revision = "demo"
	fixture := &controllerFixture{}
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
	grants := fixture.start(t)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.Write([]byte("ok")) }))
	t.Cleanup(server.Close)
	home := &servingHome{server: server}
	grants.Dial = func(context.Context, string, string) (FiberClient, error) { return home, nil }
	router.homes = grants
	router.mu.Lock()
	router.revisions["default/demo"] = revision
	router.mu.Unlock()
	return router, grants, home, revision
}

func TestAnInFlightCloneKeepsItsConnectionThroughAPrune(t *testing.T) {
	router, grants, home, revision := routedCallerGrants(t, project.ExecutionFresh)
	home.block, home.started = make(chan struct{}), make(chan struct{})
	served := make(chan int)
	go func() {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		served <- response.Code
	}()
	<-home.started
	grants.Retain(map[string]bool{})
	if home.closed.Load() {
		t.Fatal("a prune closed the connection under a clone in flight")
	}
	close(home.block)
	if code := <-served; code != http.StatusOK {
		t.Fatalf("request = %d", code)
	}
	if released := home.releases(); len(released) != 1 || !home.closed.Load() {
		t.Fatalf("released %v, closed %v, want the fiber released and then the connection closed", released, home.closed.Load())
	}
}

func TestAPooledSlotLetsItsPrunedConnectionCloseWhenItEnds(t *testing.T) {
	router, grants, home, revision := routedCallerGrants(t, project.ExecutionReuse)
	// Each clone of the slot's session is a new fiber, as when the old one
	// died between requests.
	for range 2 {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		if response.Code != http.StatusOK {
			t.Fatalf("request = %d", response.Code)
		}
	}
	grants.Retain(map[string]bool{})
	if home.closed.Load() {
		t.Fatal("a prune closed the connection a pooled slot holds")
	}
	router.mu.RLock()
	pool := router.pools["default--demo"]
	router.mu.RUnlock()
	router.endPool(pool, true, time.Now())
	router.releases.Wait()
	if released := home.releases(); !slices.Equal(released, []string{"fiber-2"}) || !home.closed.Load() {
		t.Fatalf("released %v, closed %v, want the slot's fiber released and the connection closed", released, home.closed.Load())
	}
}

func TestAFailedCloneLetsGoOfItsConnection(t *testing.T) {
	for _, mode := range []string{project.ExecutionFresh, project.ExecutionReuse} {
		router, grants, home, revision := routedCallerGrants(t, mode)
		home.fail = true
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		if response.Code == http.StatusOK {
			t.Fatalf("%s: a refused clone served", mode)
		}
		grants.Retain(map[string]bool{})
		if !home.closed.Load() {
			t.Fatalf("%s: a failed clone kept holding its connection", mode)
		}
	}
}

// The controller lists a rollout's new Pod while it runs, admitted or not,
// so a home that refuses Clone must not take every request from one that
// serves.
func TestAFailedHomeIsPassedOverUntilItsBackoffEnds(t *testing.T) {
	router, _, revision := routerFixture(t, project.ExecutionFresh)
	revision.Revision = "demo"
	fixture := &controllerFixture{}
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"), testHome("uid-b", "10.0.0.2:8484", "token-b"))
	grants := fixture.start(t)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.Write([]byte("ok")) }))
	t.Cleanup(server.Close)
	homes := map[string]*servingHome{"10.0.0.1:8484": {server: server, fail: true}, "10.0.0.2:8484": {server: server}}
	grants.Dial = func(_ context.Context, address, _ string) (FiberClient, error) { return homes[address], nil }
	router.homes = grants
	router.mu.Lock()
	router.revisions["default/demo"] = revision
	router.mu.Unlock()
	clones := func(address string) int {
		home := homes[address]
		home.mu.Lock()
		defer home.mu.Unlock()
		return home.next
	}
	serve := func() int {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		settle(grants)
		return response.Code
	}

	if code := serve(); code == http.StatusOK {
		t.Fatal("the refusing home served")
	}
	for range 3 {
		if code := serve(); code != http.StatusOK {
			t.Fatalf("request after a failed home = %d, want the other home to serve", code)
		}
	}
	if a, b := clones("10.0.0.1:8484"), clones("10.0.0.2:8484"); a != 1 || b != 3 {
		t.Fatalf("clones = %d on the failed home and %d on the other, want 1 and 3", a, b)
	}

	// Once its backoff ends, the recovered home with the lower UID serves
	// again.
	homes["10.0.0.1:8484"].mu.Lock()
	homes["10.0.0.1:8484"].fail = false
	homes["10.0.0.1:8484"].mu.Unlock()
	cache := grants.cache("default/demo")
	cache.mu.Lock()
	cache.failed["uid-a"] = time.Now().Add(-time.Second)
	cache.mu.Unlock()
	if code := serve(); code != http.StatusOK || clones("10.0.0.1:8484") != 2 {
		t.Fatalf("request after the backoff = %d with %d clones on the recovered home, want it served there", code, clones("10.0.0.1:8484"))
	}

	// With every home failed, the one whose backoff ends first is still
	// tried, so a grant with one Pod is not unserved for the backoff.
	for _, home := range homes {
		home.mu.Lock()
		home.fail = true
		home.mu.Unlock()
	}
	serve()
	serve()
	if home, err := grants.Resolve(context.Background(), "default", "demo"); err != nil || home.UID != "uid-a" {
		t.Fatalf("resolve with every home failed = %#v, %v, want uid-a, failed first", home, err)
	}
	// An answer that no longer lists a failed home forgets its failure.
	fixture.set(http.StatusOK, testHome("uid-b", "10.0.0.2:8484", "token-b"))
	grants.Forget("default", "demo", "")
	grants.Resolve(context.Background(), "default", "demo")
	settle(grants)
	cache.mu.Lock()
	_, listed := cache.failed["uid-a"]
	cache.mu.Unlock()
	if listed {
		t.Fatal("a home the answer dropped stayed marked failed")
	}
}

// Only the home is judged: a home that shed load or whose grant is full,
// and a request that ended by its client leaving or its own timeout, leave
// it served without asking the controller again.
func TestARequestThatEndedLeavesItsHomeServed(t *testing.T) {
	waitDone := func(ctx context.Context) error {
		<-ctx.Done()
		return status.FromContextError(ctx.Err()).Err()
	}
	for _, row := range []struct {
		name     string
		timeout  string
		cancel   bool
		cloneErr func(context.Context) error
	}{
		{name: "a shed", cloneErr: func(context.Context) error { return &consumer.Shed{RetryAfter: time.Second} }},
		{name: "a grant at fibers.max", cloneErr: func(context.Context) error {
			return &consumer.Deferred{Reason: "clone: " + core.ErrGrantFull.Error()}
		}},
		{name: "a cancelled request", cancel: true, cloneErr: waitDone},
		{name: "a short Revision timeout", timeout: "50ms", cloneErr: waitDone},
	} {
		router, _, revision := routerFixture(t, project.ExecutionFresh)
		revision.Revision = "demo"
		if row.timeout != "" {
			revision.Timeout = row.timeout
		}
		fixture := &controllerFixture{}
		fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"), testHome("uid-b", "10.0.0.2:8484", "token-b"))
		grants := fixture.start(t)
		server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.Write([]byte("ok")) }))
		t.Cleanup(server.Close)
		home := &servingHome{server: server}
		grants.Dial = func(context.Context, string, string) (FiberClient, error) { return home, nil }
		router.homes = grants
		router.mu.Lock()
		router.revisions["default/demo"] = revision
		router.mu.Unlock()
		if _, err := grants.Resolve(context.Background(), "default", "demo"); err != nil {
			t.Fatal(err)
		}

		home.mu.Lock()
		home.cloneErr = row.cloneErr
		home.mu.Unlock()
		ctx, cancel := context.WithCancel(context.Background())
		if row.cancel {
			cancel()
		}
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision).WithContext(ctx))
		cancel()
		settle(grants)
		if response.Code == http.StatusOK {
			t.Fatalf("%s: the failing clone served", row.name)
		}
		cache := grants.cache("default/demo")
		cache.mu.Lock()
		failed := len(cache.failed)
		cache.mu.Unlock()
		if resolved, err := grants.Resolve(context.Background(), "default", "demo"); failed != 0 || err != nil || resolved.UID != "uid-a" {
			t.Fatalf("%s: %d homes marked failed, next home %q %v, want uid-a still served", row.name, failed, resolved.UID, err)
		}
		settle(grants)
		if asked := fixture.count(); asked != 1 {
			t.Fatalf("%s: asked the controller %d times, want no refresh for it", row.name, asked)
		}
	}
}

// twoHomeRouter routes demo to a fresh grant answer listing uid-a and
// uid-b, each its own servingHome.
func twoHomeRouter(t *testing.T, mode string) (*Router, *CallerGrants, *controllerFixture, map[string]*servingHome, RevisionRoute) {
	t.Helper()
	router, _, revision := routerFixture(t, mode)
	revision.Revision = "demo"
	fixture := &controllerFixture{}
	fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"), testHome("uid-b", "10.0.0.2:8484", "token-b"))
	grants := fixture.start(t)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.Write([]byte("ok")) }))
	t.Cleanup(server.Close)
	homes := map[string]*servingHome{"10.0.0.1:8484": {server: server}, "10.0.0.2:8484": {server: server}}
	grants.Dial = func(_ context.Context, address, _ string) (FiberClient, error) { return homes[address], nil }
	router.homes = grants
	router.mu.Lock()
	router.revisions["default/demo"] = revision
	router.mu.Unlock()
	return router, grants, fixture, homes, revision
}

func failedHomes(grants *CallerGrants) int {
	cache := grants.cache("default/demo")
	cache.mu.Lock()
	defer cache.mu.Unlock()
	return len(cache.failed)
}

// pruningResolver runs prune once, after its first Resolve returns and
// before the router holds the home, as a background refresh may.
type pruningResolver struct {
	*CallerGrants
	prune func()
}

func (resolver *pruningResolver) Resolve(ctx context.Context, namespace, name string) (HomeGrant, error) {
	grant, err := resolver.CallerGrants.Resolve(ctx, namespace, name)
	if prune := resolver.prune; prune != nil {
		resolver.prune = nil
		prune()
	}
	return grant, err
}

// A refresh that prunes the connection Resolve has just returned, before
// the request holds it, leaves the request to the home that replaced it.
func TestAHomePrunedBeforeItsHoldLeavesTheRequestToItsReplacement(t *testing.T) {
	for _, mode := range []string{project.ExecutionFresh, project.ExecutionReuse} {
		router, grants, fixture, homes, revision := twoHomeRouter(t, mode)
		fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
		if _, err := grants.Resolve(context.Background(), "default", "demo"); err != nil {
			t.Fatal(err)
		}
		router.homes = &pruningResolver{CallerGrants: grants, prune: func() {
			fixture.set(http.StatusOK, testHome("uid-b", "10.0.0.2:8484", "token-b"))
			grants.Forget("default", "demo", "")
			grants.Resolve(context.Background(), "default", "demo")
			settle(grants)
		}}
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		if response.Code != http.StatusOK {
			t.Fatalf("%s: request = %d %s, want the replacing home to serve", mode, response.Code, response.Body)
		}
		old, replacement := homes["10.0.0.1:8484"], homes["10.0.0.2:8484"]
		if !old.closed.Load() || old.next != 0 || replacement.next != 1 {
			t.Fatalf("%s: old home closed %v with %d clones, replacement %d clones, want the pruned home skipped",
				mode, old.closed.Load(), old.next, replacement.next)
		}
		if asked, failed := fixture.count(), failedHomes(grants); asked != 2 || failed != 0 {
			t.Fatalf("%s: asked %d times with %d homes failed, want the retry to cost no ask", mode, asked, failed)
		}
	}
}

// A slot pinned to a frozen home lets go of its fiber there without
// spending the Clone's budget on the healthy home it moves to.
func TestASlotLeavingAFrozenHomeServesFromTheNextOne(t *testing.T) {
	router, grants, fixture, homes, revision := twoHomeRouter(t, project.ExecutionReuse)
	serve := func() int {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		return response.Code
	}
	if code := serve(); code != http.StatusOK {
		t.Fatalf("first request = %d", code)
	}
	frozen := homes["10.0.0.1:8484"]
	hang := make(chan struct{})
	frozen.mu.Lock()
	frozen.releaseHang = hang
	frozen.mu.Unlock()
	// Its Pod leaves the answer, so the slot must move to uid-b.
	fixture.set(http.StatusOK, testHome("uid-b", "10.0.0.2:8484", "token-b"))
	grants.Forget("default", "demo", "")
	grants.Resolve(context.Background(), "default", "demo")
	settle(grants)

	started := time.Now()
	if code := serve(); code != http.StatusOK || failedHomes(grants) != 0 {
		t.Fatalf("request after the move = %d with %d homes marked failed, want 200 and none", code, failedHomes(grants))
	}
	if took := time.Since(started); took > time.Second {
		t.Fatalf("request after the move took %s, waiting on the frozen home's release", took)
	}
	// Once the home answers, the stale fiber is released there and the
	// connection, which no answer lists, closes with it.
	close(hang)
	router.releases.Wait()
	if released := frozen.releases(); !slices.Equal(released, []string{"fiber-1"}) || !frozen.closed.Load() {
		t.Fatalf("released %v, closed %v, want the stale fiber released and the connection closed", released, frozen.closed.Load())
	}
}

// Ending a frozen home's pool holds up neither a reload nor anything but
// Close, which waits for the release before closing the connections.
func TestAFrozenHomesPoolEndsInTheBackground(t *testing.T) {
	router, _, _, homes, revision := twoHomeRouter(t, project.ExecutionReuse)
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Code != http.StatusOK {
		t.Fatalf("first request = %d", response.Code)
	}
	frozen := homes["10.0.0.1:8484"]
	hang := make(chan struct{})
	frozen.mu.Lock()
	frozen.releaseHang = hang
	frozen.mu.Unlock()

	// The routes on disk do not list demo, so the reload ends its pool.
	started := time.Now()
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	if took := time.Since(started); took > time.Second {
		t.Fatalf("reload took %s, waiting on the frozen home's release", took)
	}
	closed := make(chan error)
	go func() { closed <- router.Close() }()
	select {
	case <-closed:
		t.Fatal("Close returned before the release in flight ended")
	case <-time.After(100 * time.Millisecond):
	}
	close(hang)
	if err := <-closed; err != nil {
		t.Fatal(err)
	}
	if released := frozen.releases(); !slices.Equal(released, []string{"fiber-1"}) {
		t.Fatalf("released %v, want the pool's fiber released before Close cut the connection", released)
	}
}

// A cold Resolve that spends most of the time to resolve leaves a slow but
// healthy Clone its whole budget.
func TestASlowColdResolveLeavesTheCloneItsBudget(t *testing.T) {
	router, grants, fixture, homes, revision := twoHomeRouter(t, project.ExecutionFresh)
	gate := make(chan struct{})
	fixture.mu.Lock()
	fixture.gate = gate
	fixture.mu.Unlock()
	time.AfterFunc(2500*time.Millisecond, func() { close(gate) })
	homes["10.0.0.1:8484"].cloneDelay = time.Second
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	settle(grants)
	if response.Code != http.StatusOK || failedHomes(grants) != 0 {
		t.Fatalf("request after a slow resolve = %d with %d homes marked failed, want 200 and none", response.Code, failedHomes(grants))
	}
}

// Close racing a request that moves a slot off a frozen home neither
// panics nor cuts off the stale release, and later requests are refused.
func TestCloseWaitsForARequestAndTheReleaseItStarts(t *testing.T) {
	router, grants, fixture, homes, revision := twoHomeRouter(t, project.ExecutionReuse)
	serve := func() int {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		return response.Code
	}
	if code := serve(); code != http.StatusOK {
		t.Fatalf("first request = %d", code)
	}
	frozen := homes["10.0.0.1:8484"]
	hang := make(chan struct{})
	frozen.mu.Lock()
	frozen.releaseHang = hang
	frozen.mu.Unlock()
	fixture.set(http.StatusOK, testHome("uid-b", "10.0.0.2:8484", "token-b"))
	grants.Forget("default", "demo", "")
	grants.Resolve(context.Background(), "default", "demo")
	settle(grants)

	served, closed := make(chan int), make(chan error)
	go func() { served <- serve() }()
	go func() { closed <- router.Close() }()
	time.AfterFunc(100*time.Millisecond, func() { close(hang) })
	<-served
	if err := <-closed; err != nil {
		t.Fatal(err)
	}
	if released := frozen.releases(); !slices.Equal(released, []string{"fiber-1"}) {
		t.Fatalf("released %v, want the slot's fiber on the frozen home released before Close", released)
	}
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Code == http.StatusOK || !strings.Contains(response.Body.String(), "shutting down") {
		t.Fatalf("request after Close = %d %q, want it refused as shutting down", response.Code, response.Body.String())
	}
}

// A request still proxying when Close starts keeps it waiting until the
// request ends, as the server's drain deadline ends it, and its fiber is
// released before Close returns.
func TestCloseWaitsForARequestStillProxying(t *testing.T) {
	router, _, _, homes, revision := twoHomeRouter(t, project.ExecutionFresh)
	streaming := make(chan struct{})
	stream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		w.WriteHeader(http.StatusOK)
		w.(http.Flusher).Flush()
		close(streaming)
		<-request.Context().Done()
	}))
	t.Cleanup(stream.Close)
	home := homes["10.0.0.1:8484"]
	home.server = stream

	ctx, endRequest := context.WithCancel(context.Background())
	served := make(chan struct{})
	go func() {
		defer close(served)
		router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/stream", revision).WithContext(ctx))
	}()
	<-streaming
	closed := make(chan error)
	go func() { closed <- router.Close() }()
	select {
	case <-closed:
		t.Fatal("Close returned while a request still held its fiber")
	case <-time.After(100 * time.Millisecond):
	}
	endRequest()
	<-served
	if err := <-closed; err != nil {
		t.Fatal(err)
	}
	if released := home.releases(); !slices.Equal(released, []string{"fiber-1"}) {
		t.Fatalf("released %v, want the streaming request's fiber released before Close returned", released)
	}
}

// A rollout replaces a grant's only Pod with one at a new address, as a
// `ly scale` does. Requests that still find the old Pod cached are not
// lost: no home received their Clone, so they ask for the grant's homes
// again and clone on the new Pod, even while the controller still lists
// the old one for a moment.
func TestARequestToAReplacedHomeClonesOnItsReplacement(t *testing.T) {
	unreachable := func(context.Context) error {
		return &notSent{err: status.Error(grpccodes.Unavailable, "connection error: connect: no route to host")}
	}
	for _, mode := range []string{project.ExecutionFresh, project.ExecutionReuse} {
		for _, listedAgain := range []bool{false, true} {
			name := fmt.Sprintf("%s, old Pod listed again %v", mode, listedAgain)
			router, _, revision := routerFixture(t, mode)
			revision.Revision = "demo"
			fixture := &controllerFixture{}
			fixture.set(http.StatusOK, testHome("uid-a", "10.0.0.1:8484", "token-a"))
			grants := fixture.start(t)
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) { w.Write([]byte("ok")) }))
			t.Cleanup(server.Close)
			old, replacement := &servingHome{server: server}, &servingHome{server: server}
			homes := map[string]*servingHome{"10.0.0.1:8484": old, "10.0.0.2:8484": replacement}
			grants.Dial = func(_ context.Context, address, _ string) (FiberClient, error) { return homes[address], nil }
			router.homes = grants
			router.mu.Lock()
			router.revisions["default/demo"] = revision
			router.mu.Unlock()
			serve := func() (int, string) {
				response := httptest.NewRecorder()
				router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
				return response.Code, response.Body.String()
			}
			if code, body := serve(); code != http.StatusOK {
				t.Fatalf("%s: request before the rollout = %d %s", name, code, body)
			}

			old.mu.Lock()
			old.cloneErr = unreachable
			old.mu.Unlock()
			answers := 0
			fixture.mu.Lock()
			fixture.answer = func() (int, []callerGrant) {
				answers++
				if listedAgain && answers == 1 {
					return http.StatusOK, []callerGrant{testHome("uid-a", "10.0.0.1:8484", "token-a")}
				}
				return http.StatusOK, []callerGrant{testHome("uid-b", "10.0.0.2:8484", "token-b")}
			}
			fixture.mu.Unlock()

			// The grant caps fibers at 4, which a pooled burst may not pass.
			var wait sync.WaitGroup
			failures := make(chan string, 4)
			for range 4 {
				wait.Go(func() {
					if code, body := serve(); code != http.StatusOK {
						failures <- fmt.Sprintf("%d %s", code, body)
					}
				})
			}
			wait.Wait()
			close(failures)
			for failure := range failures {
				t.Fatalf("%s: request during the rollout = %s, want none lost", name, failure)
			}
			for range 2 {
				if code, body := serve(); code != http.StatusOK {
					t.Fatalf("%s: request after the rollout = %d %s", name, code, body)
				}
			}
			replacement.mu.Lock()
			served := replacement.next
			replacement.mu.Unlock()
			if served != 6 {
				t.Fatalf("%s: the new Pod served %d clones, want all 6", name, served)
			}
		}
	}
}

// reconnectingHome fails its first refusals Clones as received by no
// home, then clones, and counts how often its connection is told to stop
// backing off.
type reconnectingHome struct {
	FiberClient
	refusals, clones, resets atomic.Int32
}

func (home *reconnectingHome) Clone(ctx context.Context, token, session string, deadline time.Duration, payload []byte) (consumer.Fiber, error) {
	home.clones.Add(1)
	if home.refusals.Add(-1) >= 0 {
		return consumer.Fiber{}, &notSent{err: status.Error(grpccodes.Unavailable, "connection error: connect: connection refused")}
	}
	return home.FiberClient.Clone(ctx, token, session, deadline, payload)
}

func (home *reconnectingHome) ResetConnectBackoff() { home.resets.Add(1) }

// A home no Clone reached that a fresh answer still names, as a Pod that
// kept its IP through an outage, is tried again without waiting out its
// connection's backoff, so the request gets its Fiber once the Pod is back.
func TestAnUnreachableHomeStillNamedIsReconnectedAtOnce(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	router.AcquireTimeout = 5 * time.Second
	home := &reconnectingHome{FiberClient: client}
	home.refusals.Store(2)
	router.homes = testHomeResolver{client: home}
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Code != http.StatusOK {
		t.Fatalf("response = %d %s, want 200", response.Code, response.Body)
	}
	if resets, clones := home.resets.Load(), home.clones.Load(); resets != 2 || clones != 3 {
		t.Fatalf("%d backoff resets over %d clones, want one before each of 2 retries", resets, clones)
	}
}

// A home whose connection never comes up fails a Clone, marked as
// received by no home: a replaced Pod's IP that drops packets within the
// dial timeout, and a Pod that accepts but never handshakes within the
// connect timeout. A home that answers, whatever it answers, does not.
func TestAHomeThatNeverConnectsFailsTheCloneAsUnsent(t *testing.T) {
	// TEST-NET-1 is never routed: its packets go nowhere, or the network
	// says at once it cannot reach it.
	dead, err := dialHome("192.0.2.1:8443", insecure.NewCredentials())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { dead.(io.Closer).Close() })
	deadCtx, cancelDead := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancelDead()
	started := time.Now()
	_, err = dead.Clone(deadCtx, "token", "", 10*time.Second, nil)
	var unsent *notSent
	if took := time.Since(started); !errors.As(err, &unsent) || took > homeDialTimeout+time.Second {
		t.Fatalf("clone on a dead IP = %v after %s, want it unsent within the dial timeout", err, took)
	}

	silent, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { silent.Close() })
	go func() {
		var held []net.Conn
		defer func() {
			for _, connection := range held {
				connection.Close()
			}
		}()
		for {
			connection, err := silent.Accept()
			if err != nil {
				return
			}
			held = append(held, connection)
		}
	}()
	home, err := dialHome(silent.Addr().String(), insecure.NewCredentials())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { home.(io.Closer).Close() })
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	started = time.Now()
	_, err = home.Clone(ctx, "token", "", 10*time.Second, nil)
	if took := time.Since(started); !errors.As(err, &unsent) || took > 5*time.Second {
		t.Fatalf("clone on a silent home = %v after %s, want it unsent within the connect timeout", err, took)
	}

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	answering := grpc.NewServer()
	grantv1.RegisterFibersServer(answering, grantv1.UnimplementedFibersServer{})
	go answering.Serve(listener)
	t.Cleanup(answering.Stop)
	home, err = dialHome(listener.Addr().String(), insecure.NewCredentials())
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { home.(io.Closer).Close() })
	_, err = home.Clone(ctx, "token", "", 10*time.Second, nil)
	if errors.As(err, &unsent) || status.Code(err) != grpccodes.Unimplemented {
		t.Fatalf("clone on an answering home = %v, want its Unimplemented, not unsent", err)
	}
}
