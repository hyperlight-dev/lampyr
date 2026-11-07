// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/Azure/fiberd/pkg/core"
	consumer "github.com/Azure/fiberd/sdk/go/client"
	"github.com/hyperlight-dev/lampyr/internal/project"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

type testFiberClient struct {
	mu       sync.Mutex
	next     int
	sessions map[string]consumer.Fiber
	servers  map[string]*httptest.Server
	released []string
	handler  http.Handler
}

type testHomeResolver struct{ client FiberClient }

func (homes testHomeResolver) Resolve(_ context.Context, namespace, revision string) (HomeGrant, error) {
	return HomeGrant{UID: namespace + "--" + revision, Token: "signed-test-grant", MaxFibers: 2, Client: homes.client}, nil
}
func (testHomeResolver) Forget(string, string, string) {}
func (testHomeResolver) Retain(map[string]bool)        {}
func (testHomeResolver) Close() error                  { return nil }

type blockingFiberClient struct{}

func (blockingFiberClient) Clone(
	ctx context.Context,
	_, _ string,
	_ time.Duration,
	_ []byte,
) (consumer.Fiber, error) {
	<-ctx.Done()
	return consumer.Fiber{}, ctx.Err()
}

func (blockingFiberClient) Release(context.Context, string, bool) error { return nil }

func (client *testFiberClient) Clone(_ context.Context, _, session string, _ time.Duration, _ []byte) (consumer.Fiber, error) {
	client.mu.Lock()
	defer client.mu.Unlock()
	if current, exists := client.sessions[session]; session != "" && exists {
		current.Kind = consumer.Attach
		return current, nil
	}
	client.next++
	id := fmt.Sprint(client.next)
	count := 0
	handler := client.handler
	if handler == nil {
		handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
			if request.URL.Path == "/slow" {
				time.Sleep(100 * time.Millisecond)
			}
			count++
			w.Header().Set("X-Test-Proto", request.Proto)
			w.Header().Set("X-Test-Namespace", request.Header.Get("Knative-Serving-Namespace"))
			w.Header().Set("X-Test-Revision", request.Header.Get("Knative-Serving-Revision"))
			w.Header().Set("X-Test-Host", request.Host)
			w.Header().Set("X-Test-Original-Host", request.Header.Get("K-Original-Host"))
			w.Header().Set("X-Test-Path", request.URL.Path)
			fmt.Fprint(w, count)
		})
	}
	server := httptest.NewUnstartedServer(handler)
	// A fiber may serve h2c (gRPC) as well as HTTP/1.1.
	server.Config.Protocols = new(http.Protocols)
	server.Config.Protocols.SetHTTP1(true)
	server.Config.Protocols.SetUnencryptedHTTP2(true)
	server.Start()
	fiber := consumer.Fiber{ID: id, Endpoint: strings.Replace(server.URL, "http://", "tcp://", 1), Kind: consumer.Create}
	client.servers[id] = server
	if session != "" {
		client.sessions[session] = fiber
	}
	return fiber, nil
}

func (client *testFiberClient) Release(_ context.Context, id string, _ bool) error {
	client.mu.Lock()
	defer client.mu.Unlock()
	client.released = append(client.released, id)
	for session, fiber := range client.sessions {
		if fiber.ID == id {
			delete(client.sessions, session)
		}
	}
	return nil
}

func routerFixture(t *testing.T, mode string) (*Router, *testFiberClient, RevisionRoute) {
	t.Helper()
	routes := t.TempDir()
	revision := RevisionRoute{
		Kind:      "revision",
		Namespace: "default",
		Revision:  "demo-1234",
		Execution: project.ExecutionPolicy{Mode: mode},
		Timeout:   "30s",
		Routed:    true,
	}
	if mode == project.ExecutionReuse {
		revision.Execution.IdleTimeout = "30s"
	}
	data, err := json.Marshal(revision)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(routes, "revision-default--demo-1234.json"),
		data,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	client := &testFiberClient{sessions: map[string]consumer.Fiber{}, servers: map[string]*httptest.Server{}}
	router, err := NewRouter(testHomeResolver{client: client}, routes)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := router.Close(); err != nil {
			t.Error(err)
		}
		for _, server := range client.servers {
			server.Close()
		}
		// Metrics live in a process-wide registry; start the next run of
		// this test (go test -count=N) from zero.
		forgetRevision(revision.Namespace, revision.Revision)
	})
	return router, client, revision
}

func revisionRequest(method, path string, revision RevisionRoute) *http.Request {
	request := httptest.NewRequest(method, "http://fiber-router"+path, nil)
	request.Header.Set("Knative-Serving-Namespace", revision.Namespace)
	request.Header.Set("Knative-Serving-Revision", revision.Revision)
	return request
}

func TestFreshPolicyCreatesAndReleasesEveryRequest(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	for range 2 {
		response := httptest.NewRecorder()
		router.Handler().ServeHTTP(response, revisionRequest("GET", "/test", revision))
		if response.Code != 200 || response.Body.String() != "1" || response.Header().Get("X-Fiberd-Clone") != "CREATE" {
			t.Fatalf("fresh execution = %d %s", response.Code, response.Body.String())
		}
	}
	if client.next != 2 || len(client.released) != 2 {
		t.Fatalf("fresh lifecycle created %d and released %d", client.next, len(client.released))
	}
}

func TestKnativeRevisionHeaderSelectsRevisionMetadata(t *testing.T) {
	routes := t.TempDir()
	revision := RevisionRoute{
		Kind:      "revision",
		Namespace: "default",
		Revision:  "demo-00001",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh},
		Timeout:   "30s",
		Routed:    true,
	}
	data, err := json.Marshal(revision)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(routes, "revision-default--demo-00001.json"),
		data,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	client := &testFiberClient{
		sessions: map[string]consumer.Fiber{},
		servers:  map[string]*httptest.Server{},
	}
	router, err := NewRouter(testHomeResolver{client: client}, routes)
	if err != nil {
		t.Fatal(err)
	}
	defer router.Close()
	request := httptest.NewRequest("GET", "http://unregistered.example/test", nil)
	request.Header.Set("Knative-Serving-Namespace", "default")
	request.Header.Set("Knative-Serving-Revision", "demo-00001")
	response := httptest.NewRecorder()
	router.ServeHTTP(response, request)
	if response.Code != http.StatusOK || response.Body.String() != "1" {
		t.Fatalf("Knative Revision request = %d %q", response.Code, response.Body.String())
	}
	if response.Header().Get("X-Test-Namespace") != "" ||
		response.Header().Get("X-Test-Revision") != "" {
		t.Fatal("Knative routing headers reached the application Fiber")
	}
}

func TestUnknownKnativeRevisionIsUnavailable(t *testing.T) {
	router, _, _ := routerFixture(t, project.ExecutionFresh)
	request := httptest.NewRequest("GET", "http://demo.default.localhost/test", nil)
	request.Header.Set("Knative-Serving-Namespace", "default")
	request.Header.Set("Knative-Serving-Revision", "missing-00001")
	response := httptest.NewRecorder()
	router.ServeHTTP(response, request)
	if response.Code != http.StatusServiceUnavailable {
		t.Fatalf("unknown Knative Revision = %d, want 503", response.Code)
	}
}

func TestKnativeRevisionReadinessChecksMetadataVersion(t *testing.T) {
	routes := t.TempDir()
	revision := RevisionRoute{
		Kind:      "revision",
		Namespace: "default",
		Revision:  "demo-00001",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh},
		Timeout:   "30s",
	}
	data, err := json.Marshal(revision)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(routes, "revision-default--demo-00001.json"),
		data,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	router, err := NewRouter(testHomeResolver{client: &testFiberClient{}}, routes)
	if err != nil {
		t.Fatal(err)
	}
	defer router.Close()
	for _, test := range []struct {
		version string
		status  int
	}{
		{version: "sha256:stale", status: http.StatusServiceUnavailable},
		{version: revision.Version(), status: http.StatusOK},
	} {
		request := httptest.NewRequest(
			"GET",
			"http://fiber-router/readyz?namespace=default&revision=demo-00001&version="+
				url.QueryEscape(test.version),
			nil,
		)
		response := httptest.NewRecorder()
		router.ServeHTTP(response, request)
		if response.Code != test.status {
			t.Fatalf("Revision readiness for %q = %d", test.version, response.Code)
		}
	}
}

func TestReusePreservesLiveStateAndReleasesOnIdle(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionReuse)
	for index := 1; index <= 2; index++ {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		if response.Code != 200 || response.Body.String() != fmt.Sprint(index) {
			t.Fatalf("reuse execution = %d %s", response.Code, response.Body.String())
		}
	}
	if client.next != 1 || len(client.released) != 0 {
		t.Fatalf("reused fiber was not retained: created=%d released=%d", client.next, len(client.released))
	}
	pool := router.pools["default--demo-1234"]
	router.endPool(pool, false, time.Now().Add(time.Minute))
	router.releases.Wait()
	if len(client.released) != 1 || len(pool.slots) != 0 {
		t.Fatal("idle fiber was not released")
	}
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Body.String() != "1" || client.next != 2 {
		t.Fatal("a request after idle release did not start from the original snapshot")
	}
}

func TestBusyReusableFiberIsNotReleasedByIdleSweep(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionReuse)
	policy, err := (project.Manifest{Execution: &revision.Execution}).EffectiveExecution()
	if err != nil {
		t.Fatal(err)
	}
	_, finish, err := router.acquire(context.Background(), context.Background(), "default", "demo-1234", policy, nil)
	if err != nil {
		t.Fatal(err)
	}
	pool := router.pools["default--demo-1234"]
	router.endPool(pool, false, time.Now().Add(time.Hour))
	router.releases.Wait()
	if len(client.released) != 0 {
		t.Fatal("an active stream/request was ended by idle cleanup")
	}
	finish(false)
	router.endPool(pool, false, time.Now().Add(time.Hour))
	router.releases.Wait()
	if len(client.released) != 1 {
		t.Fatal("completed request was not eligible for idle cleanup")
	}
}

func TestRevisionRemovalDrainsReusePool(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionReuse)
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if err := os.Remove(
		filepath.Join(router.routesDir, "revision-default--demo-1234.json"),
	); err != nil {
		t.Fatal(err)
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	router.releases.Wait()
	if len(client.released) != 1 {
		t.Fatal("removed revision retained an idle fiber")
	}
	response = httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Code != http.StatusServiceUnavailable {
		t.Fatalf("removed Revision is still serving: %d", response.Code)
	}
}

func TestRevisionRoutingStateDrainsReusePool(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionReuse)
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	data, err := json.Marshal(map[string]any{
		"kind":      revision.Kind,
		"namespace": revision.Namespace,
		"revision":  revision.Revision,
		"execution": revision.Execution,
		"timeout":   revision.Timeout,
		"routed":    false,
	})
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(router.routesDir, "revision-default--demo-1234.json"),
		data,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	router.releases.Wait()
	if len(client.released) != 1 {
		t.Fatal("unreferenced Revision retained an idle Fiber")
	}
	response = httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Code != http.StatusOK {
		t.Fatalf("known unreferenced Revision request = %d", response.Code)
	}
}

func TestRevisionReloadUpdatesReuseIdleTimeout(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionReuse)
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	revision.Execution.IdleTimeout = "1s"
	data, err := json.Marshal(revision)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(router.routesDir, "revision-default--demo-1234.json"),
		data,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	router.releases.Wait()
	pool := router.pools["default--demo-1234"]
	router.endPool(pool, false, time.Now().Add(2*time.Second))
	router.releases.Wait()
	if len(client.released) != 1 {
		t.Fatal("reloaded idle timeout was not enforced")
	}
}

func TestDomainMappingRestoresOriginalHost(t *testing.T) {
	router, _, revision := routerFixture(t, project.ExecutionFresh)
	request := revisionRequest("GET", "/test", revision)
	request.Host = "internal.default.svc.cluster.local"
	request.Header.Set("K-Original-Host", "tenant.example.com")
	response := httptest.NewRecorder()
	router.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("DomainMapping request = %d %q", response.Code, response.Body.String())
	}
	if response.Header().Get("X-Test-Host") != "tenant.example.com" ||
		response.Header().Get("X-Test-Original-Host") != "" {
		t.Fatalf("application host headers = %#v", response.Header())
	}
}

func TestRevisionTimeoutReturnsGatewayTimeout(t *testing.T) {
	router, _, revision := routerFixture(t, project.ExecutionFresh)
	revision.Timeout = "10ms"
	data, err := json.Marshal(revision)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(router.routesDir, "revision-default--demo-1234.json"),
		data,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	router.releases.Wait()
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/slow", revision))
	// The client learns the timeout passed, not where the Fiber listens.
	const body = "the Revision's timeout passed before its Fiber answered\n"
	if response.Code != http.StatusGatewayTimeout || response.Body.String() != body {
		t.Fatalf("timed out Revision = %d %q, want 504 %q", response.Code, response.Body, body)
	}
}

func TestRevisionTimeoutDuringFiberAcquisitionReturnsGatewayTimeout(t *testing.T) {
	routes := t.TempDir()
	revision := RevisionRoute{
		Kind:      "revision",
		Namespace: "default",
		Revision:  "demo-1234",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh},
		Timeout:   "10ms",
		Routed:    true,
	}
	data, err := json.Marshal(revision)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(
		filepath.Join(routes, "revision-default--demo-1234.json"),
		data,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	router, err := NewRouter(testHomeResolver{client: blockingFiberClient{}}, routes)
	if err != nil {
		t.Fatal(err)
	}
	defer router.Close()
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Code != http.StatusGatewayTimeout {
		t.Fatalf("timed out Fiber acquisition = %d, want 504", response.Code)
	}
}

func TestRouterHealthDoesNotCreateFiber(t *testing.T) {
	router, client, _ := routerFixture(t, project.ExecutionFresh)
	response := httptest.NewRecorder()
	router.Handler().ServeHTTP(
		response,
		httptest.NewRequest("GET", "http://fiber-router/healthz", nil),
	)
	if response.Code != http.StatusOK || client.next != 0 {
		t.Fatal("health probe created an application fiber")
	}
}

func TestApplicationManagementPathsReachSelectedRevision(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	for _, path := range []string{"/healthz", "/readyz?namespace=probe&revision=probe"} {
		response := httptest.NewRecorder()
		router.Handler().ServeHTTP(response, revisionRequest("GET", path, revision))
		if response.Code != http.StatusOK ||
			response.Header().Get("X-Test-Path") != strings.Split(path, "?")[0] ||
			response.Header().Get("X-Fiberd-Clone") != "CREATE" {
			t.Fatalf("application path %q was intercepted: %d %#v", path, response.Code, response.Header())
		}
	}
	if client.next != 2 {
		t.Fatalf("management-looking application requests created %d Fibers", client.next)
	}
}

func TestRouterForwardsH2CAsH2C(t *testing.T) {
	router, _, revision := routerFixture(t, project.ExecutionFresh)
	server := httptest.NewUnstartedServer(router.Handler())
	server.Config.Protocols = new(http.Protocols)
	server.Config.Protocols.SetHTTP1(true)
	server.Config.Protocols.SetUnencryptedHTTP2(true)
	server.Start()
	defer server.Close()
	for _, test := range []struct {
		name  string
		h2c   bool
		proto string
	}{
		{"HTTP/1.1 stays HTTP/1.1", false, "HTTP/1.1"},
		{"h2c stays h2c", true, "HTTP/2.0"},
	} {
		t.Run(test.name, func(t *testing.T) {
			transport := http.DefaultTransport.(*http.Transport).Clone()
			if test.h2c {
				transport.Protocols = new(http.Protocols)
				transport.Protocols.SetUnencryptedHTTP2(true)
			}

			defer transport.CloseIdleConnections()
			request, err := http.NewRequest("GET", server.URL+"/test", nil)
			if err != nil {
				t.Fatal(err)
			}
			request.Header.Set("Knative-Serving-Namespace", revision.Namespace)
			request.Header.Set("Knative-Serving-Revision", revision.Revision)
			response, err := (&http.Client{Transport: transport}).Do(request)
			if err != nil {
				t.Fatal(err)
			}
			defer response.Body.Close()
			if response.StatusCode != http.StatusOK || response.Proto != test.proto ||
				response.Header.Get("X-Test-Proto") != test.proto {
				t.Fatalf("status %d via %s, fiber saw %q; want %s end to end",
					response.StatusCode, response.Proto, response.Header.Get("X-Test-Proto"), test.proto)
			}
		})
	}
}

func TestShedAndFullGrantGetRetryAfter(t *testing.T) {
	for _, test := range []struct {
		name  string
		err   error
		shed  bool
		after time.Duration
	}{
		{"pressure", fmt.Errorf("clone: %w", &consumer.Shed{RetryAfter: 2 * time.Second}), true, 2 * time.Second},
		{"shed without a hint", &consumer.Shed{}, true, time.Second},
		{"grant at fibers.max", &consumer.Deferred{Reason: "clone: " + core.ErrGrantFull.Error()}, true, time.Second},
		{"helper restoring too many", &consumer.Deferred{Reason: busyReason}, true, time.Second},
		{"other deferred fallback", &consumer.Deferred{Reason: "backend failed"}, false, 0},
		{"other helper error", &consumer.Deferred{Reason: "hyperlight: helper error: helper exited"}, false, 0},
		{"plain error", errors.New("dial failed"), false, 0},
	} {
		after, shed := shedRetryAfter(test.err)
		if shed != test.shed || after != test.after {
			t.Errorf("%s: shedRetryAfter = %s, %v; want %s, %v", test.name, after, shed, test.after, test.shed)
		}
	}
}

// rateLimitedHome refuses Clones as fiberd's clone rate limit does, with
// the SHED miss carrying core.ErrRateLimit's message, and otherwise
// refuses with refusal, if set.
type rateLimitedHome struct {
	FiberClient
	limit   *core.RateLimit
	refusal error
	clones  atomic.Int32
}

func (home *rateLimitedHome) Clone(ctx context.Context, token, session string, deadline time.Duration, payload []byte) (consumer.Fiber, error) {
	home.clones.Add(1)
	if home.refusal != nil {
		return consumer.Fiber{}, home.refusal
	}
	if err := home.limit.Take(); err != nil {
		return consumer.Fiber{}, &consumer.Shed{RetryAfter: time.Second, Reason: err.Error()}
	}
	return home.FiberClient.Clone(ctx, token, session, deadline, payload)
}

// A cold burst past a home's clone rate limit waits the limit out within
// the acquire deadline: every request gets a Fiber and none is shed.
func TestARateLimitedHomeIsWaitedOutWithoutAShed(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	router.AcquireTimeout = 20 * time.Second
	// 10 clones at once, then 10 a second.
	home := &rateLimitedHome{FiberClient: client, limit: core.NewRateLimit(10, 0)}
	router.homes = testHomeResolver{client: home}
	var wait sync.WaitGroup
	failures := make(chan string, 30)
	started := time.Now()
	for range 30 {
		wait.Go(func() {
			response := httptest.NewRecorder()
			router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
			if response.Code != http.StatusOK {
				failures <- fmt.Sprintf("%d %s", response.Code, response.Body)
			}
		})
	}
	wait.Wait()
	close(failures)
	for failure := range failures {
		t.Fatalf("request in a rate-limited burst = %s, want it to wait for a Fiber", failure)
	}
	if took := time.Since(started); took > 10*time.Second {
		t.Fatalf("burst took %s, want the limit's refill to pace it", took)
	}
	sheds := testutil.ToFloat64(shedTotal.WithLabelValues(revision.Namespace, revision.Revision))
	retries := testutil.ToFloat64(cloneRetriesTotal.WithLabelValues(revision.Namespace, revision.Revision, retryRateLimit))
	if sheds != 0 || retries < 20 {
		t.Fatalf("sheds %v and rate-limit retries %v, want none shed and the 20 over the burst retried", sheds, retries)
	}
}

// A rate limit that outlasts the acquire deadline is a shed with
// Retry-After, and a home's pressure shed is answered at once, as before.
func TestARefusalPastTheAcquireDeadlineIsShed(t *testing.T) {
	for _, test := range []struct {
		name     string
		home     func(FiberClient) *rateLimitedHome
		oneClone bool
	}{
		{name: "a rate limit that never clears", home: func(client FiberClient) *rateLimitedHome {
			return &rateLimitedHome{FiberClient: client, limit: core.NewRateLimit(0, 0)}
		}},
		{name: "memory pressure", oneClone: true, home: func(client FiberClient) *rateLimitedHome {
			return &rateLimitedHome{FiberClient: client, refusal: &consumer.Shed{RetryAfter: time.Second, Reason: core.ErrPressure.Error()}}
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			router, client, revision := routerFixture(t, project.ExecutionFresh)
			router.AcquireTimeout = 1200 * time.Millisecond
			home := test.home(client)
			router.homes = testHomeResolver{client: home}
			started := time.Now()
			response := httptest.NewRecorder()
			router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
			took := time.Since(started)
			if response.Code != http.StatusServiceUnavailable || response.Header().Get("Retry-After") != "1" {
				t.Fatalf("response = %d, Retry-After %q; want 503 and 1", response.Code, response.Header().Get("Retry-After"))
			}
			if sheds := testutil.ToFloat64(shedTotal.WithLabelValues(revision.Namespace, revision.Revision)); sheds != 1 {
				t.Fatalf("sheds counted = %v, want 1", sheds)
			}
			if clones := home.clones.Load(); test.oneClone && (clones != 1 || took > 500*time.Millisecond) {
				t.Fatalf("%d clones in %s, want the shed answered at once", clones, took)
			}
			if took > 2*time.Second {
				t.Fatalf("shed took %s, want it within the acquire deadline", took)
			}
		})
	}
}

// busyReason is the deferred fallback's reason fiberd sends for the
// helper's refusal to queue a restore that would miss its deadline: the
// backend's ErrHelper wrapping the helper's ERROR text.
const busyReason = "hyperlight: helper error: restores-busy: 4 restores running and 9 queued, " +
	"about 1.2s each: too many for the deadline"

// The router knows a busy helper only by the marker the helper starts its
// refusal with, so renaming either side alone would turn every busy
// refusal into an immediate 503.
func TestTheRestoresBusyMarkerIsTheHelpers(t *testing.T) {
	source, err := os.ReadFile(filepath.Join("..", "..", "helper", "src", "restores.rs"))
	if err != nil {
		t.Fatal(err)
	}
	match := regexp.MustCompile(`(?m)^pub const BUSY: &str = "([^"]*)";$`).FindSubmatch(source)
	if match == nil {
		t.Fatal("helper/src/restores.rs defines no BUSY marker")
	}
	if helper := string(match[1]); helper != helperRestoresBusy || !strings.Contains(busyReason, helper) {
		t.Fatalf("helper's BUSY = %q, router's marker = %q: want them equal", helper, helperRestoresBusy)
	}
}

// refusingHome refuses the first refusals Clones with err, then clones.
type refusingHome struct {
	FiberClient
	err      error
	refusals atomic.Int32
	clones   atomic.Int32
}

func (home *refusingHome) Clone(ctx context.Context, token, session string, deadline time.Duration, payload []byte) (consumer.Fiber, error) {
	home.clones.Add(1)
	if home.refusals.Add(-1) >= 0 {
		return consumer.Fiber{}, home.err
	}
	return home.FiberClient.Clone(ctx, token, session, deadline, payload)
}

// forgetfulHomes counts how often the router passes its home over.
type forgetfulHomes struct {
	testHomeResolver
	forgets *atomic.Int32
}

func (homes forgetfulHomes) Forget(string, string, string) { homes.forgets.Add(1) }

// A helper too busy restoring to take a Clone is backpressure: the request
// waits a moment and tries again, and gets its Fiber; if the helper stays
// busy past the acquire deadline, the request is shed with Retry-After.
// Either way the home is not passed over.
func TestAHomeTooBusyRestoringIsRetriedAndThenShed(t *testing.T) {
	for _, test := range []struct {
		name     string
		refusals int32
		code     int
		sheds    float64
	}{
		{name: "busy for a moment", refusals: 2, code: http.StatusOK},
		{name: "busy past the deadline", refusals: 1 << 20, code: http.StatusServiceUnavailable, sheds: 1},
	} {
		t.Run(test.name, func(t *testing.T) {
			router, client, revision := routerFixture(t, project.ExecutionFresh)
			router.AcquireTimeout = 2500 * time.Millisecond
			home := &refusingHome{FiberClient: client, err: fmt.Errorf("clone: %w", &consumer.Deferred{Reason: busyReason})}
			home.refusals.Store(test.refusals)
			forgets := new(atomic.Int32)
			router.homes = forgetfulHomes{testHomeResolver: testHomeResolver{client: home}, forgets: forgets}
			started := time.Now()
			response := httptest.NewRecorder()
			router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
			took := time.Since(started)
			if response.Code != test.code {
				t.Fatalf("response = %d %s, want %d", response.Code, response.Body, test.code)
			}
			if test.code != http.StatusOK && response.Header().Get("Retry-After") != "1" {
				t.Fatalf("Retry-After = %q, want 1", response.Header().Get("Retry-After"))
			}
			sheds := testutil.ToFloat64(shedTotal.WithLabelValues(revision.Namespace, revision.Revision))
			retries := testutil.ToFloat64(cloneRetriesTotal.WithLabelValues(revision.Namespace, revision.Revision, retryRestoresBusy))
			if sheds != test.sheds || retries < 1 || forgets.Load() != 0 {
				t.Fatalf("sheds %v, busy retries %v, homes forgotten %d; want %v, at least 1, 0",
					sheds, retries, forgets.Load(), test.sheds)
			}
			// Each retry waits between 250ms and 1s.
			if clones := home.clones.Load(); clones < 2 || clones > 11 || took > 3*time.Second {
				t.Fatalf("%d clones in %s, want a few, spaced out, within the acquire deadline", clones, took)
			}
		})
	}
}

// A client that leaves while its request waits out a home's refusal ended
// the request, not the home: it is not counted as a shed.
func TestAClientLeavingDuringABackoffIsNotAShed(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	router.AcquireTimeout = 10 * time.Second
	router.homes = testHomeResolver{client: &rateLimitedHome{FiberClient: client, limit: core.NewRateLimit(0, 0)}}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	leave := time.AfterFunc(300*time.Millisecond, cancel)
	defer leave.Stop()
	started := time.Now()
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision).WithContext(ctx))
	if took := time.Since(started); took > 2*time.Second {
		t.Fatalf("request took %s after its client left", took)
	}
	sheds := testutil.ToFloat64(shedTotal.WithLabelValues(revision.Namespace, revision.Revision))
	canceled := testutil.CollectAndCount(fiberAcquireSeconds.WithLabelValues(revision.Namespace, revision.Revision, "canceled").(prometheus.Histogram))
	if sheds != 0 || canceled != 1 {
		t.Fatalf("sheds %v, canceled acquisitions %d; want 0 and 1", sheds, canceled)
	}
}

// A reuse Fiber's attach refused for backpressure keeps its slot and its
// Fiber, so the retry attaches to it rather than restoring another.
func TestABackpressuredAttachKeepsItsFiber(t *testing.T) {
	for _, test := range []struct {
		name string
		err  error
	}{
		{"rate limit", &consumer.Shed{RetryAfter: time.Second, Reason: core.ErrRateLimit.Error()}},
		{"restores busy", &consumer.Deferred{Reason: busyReason}},
	} {
		t.Run(test.name, func(t *testing.T) {
			router, client, revision := routerFixture(t, project.ExecutionReuse)
			router.AcquireTimeout = 5 * time.Second
			home := &refusingHome{FiberClient: client, err: test.err}
			router.homes = testHomeResolver{client: home}
			response := httptest.NewRecorder()
			router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
			if response.Code != http.StatusOK {
				t.Fatalf("first request = %d %s", response.Code, response.Body)
			}
			home.refusals.Store(1)
			response = httptest.NewRecorder()
			router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
			if response.Code != http.StatusOK || response.Header().Get("X-Fiberd-Clone") != consumer.Attach.String() ||
				response.Body.String() != "2" {
				t.Fatalf("request after a refused attach = %d %s %q, want 200 from the same Fiber's ATTACH",
					response.Code, response.Header().Get("X-Fiberd-Clone"), response.Body)
			}
			client.mu.Lock()
			created, released := client.next, len(client.released)
			client.mu.Unlock()
			if created != 1 || released != 0 {
				t.Fatalf("created %d Fibers and released %d, want the one Fiber kept", created, released)
			}
		})
	}
}

type refusingHomes struct {
	testHomeResolver
	err error
}

func (homes refusingHomes) Resolve(context.Context, string, string) (HomeGrant, error) {
	return HomeGrant{}, homes.err
}

// The controller refusing to name a grant's homes tells the client when
// to retry, but no home refused it, so it is not counted as a shed.
func TestAControllerRefusalGetsRetryAfterWithoutCountingAShed(t *testing.T) {
	for _, test := range []struct {
		name  string
		err   error
		after string
		sheds float64
	}{
		{
			name: "controller refusal",
			err: &GrantsRefused{Issuer: "https://controller", Grant: "default/refused-1001",
				Status: "429 Too Many Requests", Reason: "RateLimited", RetryAfter: 2 * time.Second},
			after: "2",
		},
		{name: "home shed", err: &consumer.Shed{RetryAfter: 3 * time.Second}, after: "3", sheds: 1},
	} {
		t.Run(test.name, func(t *testing.T) {
			router, _, route, _ := namedRouter(t, "refused-1001", "30s", project.ExecutionFresh)
			router.homes = refusingHomes{err: test.err}
			shedTotal.DeleteLabelValues(route.Namespace, route.Revision)
			response := httptest.NewRecorder()
			router.ServeHTTP(response, revisionRequest("GET", "/", route))
			// The client learns when to retry, not the controller, grant
			// or Pods behind the refusal.
			const body = "no Fiber is available for this Revision\n"
			if response.Code != http.StatusServiceUnavailable || response.Header().Get("Retry-After") != test.after ||
				response.Body.String() != body {
				t.Fatalf("response = %d, Retry-After %q, %q; want 503, %q, %q",
					response.Code, response.Header().Get("Retry-After"), response.Body.String(), test.after, body)
			}
			if sheds := testutil.ToFloat64(shedTotal.WithLabelValues(route.Namespace, route.Revision)); sheds != test.sheds {
				t.Fatalf("sheds counted = %v, want %v", sheds, test.sheds)
			}
		})
	}
}

type switchingHomes struct {
	testHomeResolver
	current *FiberClient
}

func (homes switchingHomes) Resolve(ctx context.Context, namespace, revision string) (HomeGrant, error) {
	home, err := homes.testHomeResolver.Resolve(ctx, namespace, revision)
	home.Client = *homes.current
	return home, err
}

func TestReusedSlotLeavesAHomeThatIsNoLongerServed(t *testing.T) {
	router, first, revision := routerFixture(t, project.ExecutionReuse)
	second := &testFiberClient{sessions: map[string]consumer.Fiber{}, servers: map[string]*httptest.Server{}}
	t.Cleanup(func() {
		for _, server := range second.servers {
			server.Close()
		}
	})
	var current FiberClient = first
	router.homes = switchingHomes{current: &current}
	serve := func() {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
		if response.Code != http.StatusOK {
			t.Fatalf("request = %d %q", response.Code, response.Body.String())
		}
	}
	serve()
	current = second
	serve()
	router.releases.Wait()
	first.mu.Lock()
	released := slices.Clone(first.released)
	first.mu.Unlock()
	second.mu.Lock()
	sessions := len(second.sessions)
	second.mu.Unlock()
	if !slices.Equal(released, []string{"1"}) || sessions != 1 {
		t.Fatalf("first home released %v, second home holds %d sessions", released, sessions)
	}
}
