// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	consumer "github.com/Azure/fiberd/sdk/go/client"
	"github.com/gorilla/websocket"
	"github.com/hyperlight-dev/lampyr/internal/project"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

type roundTripFunc func(*http.Request) (*http.Response, error)

func (roundTrip roundTripFunc) RoundTrip(request *http.Request) (*http.Response, error) {
	return roundTrip(request)
}

type failedUpgradeBody struct{}

func (*failedUpgradeBody) Read([]byte) (int, error) {
	return 0, errors.New("upgraded backend read failed")
}
func (*failedUpgradeBody) Write(data []byte) (int, error) { return len(data), nil }
func (*failedUpgradeBody) Close() error                   { return nil }

type closedUpgradeBody struct{}

func (*closedUpgradeBody) Read([]byte) (int, error)  { return 0, net.ErrClosed }
func (*closedUpgradeBody) Write([]byte) (int, error) { return 0, io.ErrClosedPipe }
func (*closedUpgradeBody) Close() error              { return nil }
func (*closedUpgradeBody) CloseWrite() error         { return net.ErrClosed }

type delayedFiberClient struct {
	client       *testFiberClient
	cloneDelay   time.Duration
	releaseDelay time.Duration
}

type blockingReleaseFiberClient struct {
	client  *testFiberClient
	started chan struct{}
	release chan struct{}
}

func (client blockingReleaseFiberClient) Clone(
	ctx context.Context,
	token, session string,
	timeout time.Duration,
	facts []byte,
) (consumer.Fiber, error) {
	return client.client.Clone(ctx, token, session, timeout, facts)
}

func (client blockingReleaseFiberClient) Release(ctx context.Context, id string, gone bool) error {
	close(client.started)
	<-client.release
	return client.client.Release(ctx, id, gone)
}

func (client delayedFiberClient) Clone(
	ctx context.Context,
	token, session string,
	timeout time.Duration,
	facts []byte,
) (consumer.Fiber, error) {
	time.Sleep(client.cloneDelay)
	return client.client.Clone(ctx, token, session, timeout, facts)
}

func (client delayedFiberClient) Release(ctx context.Context, id string, gone bool) error {
	time.Sleep(client.releaseDelay)
	return client.client.Release(ctx, id, gone)
}

func sampleValue(t *testing.T, body, prefix string) float64 {
	t.Helper()
	for line := range strings.Lines(body) {
		if strings.HasPrefix(line, prefix) {
			fields := strings.Fields(line)
			value, err := strconv.ParseFloat(fields[len(fields)-1], 64)
			if err != nil {
				t.Fatal(err)
			}
			return value
		}
	}
	t.Fatalf("metric sample %q is missing:\n%s", prefix, body)
	return 0
}

func namedRouter(t *testing.T, name, timeout, mode string) (*Router, *testFiberClient, RevisionRoute, string) {
	routes := t.TempDir()
	route := RevisionRoute{Kind: "revision", Namespace: "default", Revision: name,
		Execution: project.ExecutionPolicy{Mode: mode}, Timeout: timeout, Routed: true}
	if mode == project.ExecutionReuse {
		route.Execution.IdleTimeout = "30s"
	}
	data, _ := json.Marshal(route)
	file := filepath.Join(routes, "r.json")
	os.WriteFile(file, data, 0o644)
	client := &testFiberClient{sessions: map[string]consumer.Fiber{}, servers: map[string]*httptest.Server{}}
	router, err := NewRouter(testHomeResolver{client: client}, routes)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		router.Close()
		for _, s := range client.servers {
			s.Close()
		}
		forgetRevision(route.Namespace, route.Revision)
	})
	return router, client, route, file
}

func serveRouter(router *Router, route RevisionRoute) *httptest.Server {
	return httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		r.Header.Set("Knative-Serving-Namespace", route.Namespace)
		r.Header.Set("Knative-Serving-Revision", route.Revision)
		router.ServeHTTP(w, r)
	}))
}

func TestRouterMetricsClassifyUpgradedBackendCopyFailure(t *testing.T) {
	router, _, route, _ := namedRouter(t, "ws-error-4444", "30s", project.ExecutionFresh)
	router.transport = roundTripFunc(func(request *http.Request) (*http.Response, error) {
		return &http.Response{
			Status:     "101 Switching Protocols",
			StatusCode: http.StatusSwitchingProtocols,
			Header: http.Header{
				"Connection": {"Upgrade"},
				"Upgrade":    {"websocket"},
			},
			Body:    &failedUpgradeBody{},
			Request: request,
		}, nil
	})
	server := serveRouter(router, route)
	defer server.Close()
	address := strings.TrimPrefix(server.URL, "http://")
	connection, err := net.Dial("tcp", address)
	if err != nil {
		t.Fatal(err)
	}
	fmt.Fprintf(connection,
		"GET /socket HTTP/1.1\r\nHost: fiber-router\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
	status, err := bufio.NewReader(connection).ReadString('\n')
	if err != nil || !strings.Contains(status, "101") {
		t.Fatalf("upgrade status = %q, %v", status, err)
	}
	connection.Close()
	deadline := time.Now().Add(time.Second)
	prefix := `lampyr_router_websocket_session_duration_seconds_count{namespace="default",outcome="error",revision="` + route.Revision + `"} `
	for !strings.Contains(scrape(), prefix+"1") && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if body := scrape(); !strings.Contains(body, prefix+"1") {
		t.Fatalf("upgraded copy failure was not recorded as an error:\n%s", lines(body, route.Revision))
	}
}

func TestRouterMetricsIgnoreProxyUpgradeCloseErrors(t *testing.T) {
	recorder := &statusRecorder{ResponseWriter: httptest.NewRecorder()}
	connection := recorder.observeUpgrade(&closedUpgradeBody{})
	connection.Read(nil)
	connection.Write(nil)
	connection.(interface{ CloseWrite() error }).CloseWrite()
	if recorder.failed.Load() {
		t.Fatal("proxy teardown was classified as an upgraded backend failure")
	}
}

func TestTracingTransportPreservesUpgradeCloseWrite(t *testing.T) {
	body := &closedUpgradeBody{}
	transport := tracedTransport(roundTripFunc(func(request *http.Request) (*http.Response, error) {
		return &http.Response{
			Status:     "101 Switching Protocols",
			StatusCode: http.StatusSwitchingProtocols,
			Header: http.Header{
				"Connection": {"Upgrade"},
				"Upgrade":    {"websocket"},
			},
			Body:    body,
			Request: request,
		}, nil
	}))
	request := httptest.NewRequest("GET", "http://fiber/socket", nil)
	request.Header.Set("Connection", "Upgrade")
	request.Header.Set("Upgrade", "websocket")
	response, err := transport.RoundTrip(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	if _, ok := response.Body.(interface{ CloseWrite() error }); !ok {
		t.Fatal("tracing transport dropped CloseWrite from an upgraded connection")
	}
}

func TestRouterMetricsMeasureOnlyWebSocketSession(t *testing.T) {
	const delay = 150 * time.Millisecond
	routes := t.TempDir()
	route := RevisionRoute{
		Kind:      "revision",
		Namespace: "default",
		Revision:  "ws-duration-4445",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh},
		Timeout:   "30s",
		Routed:    true,
	}
	data, _ := json.Marshal(route)
	if err := os.WriteFile(filepath.Join(routes, "r.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	client := &testFiberClient{
		sessions: map[string]consumer.Fiber{},
		servers:  map[string]*httptest.Server{},
	}
	upgrader := websocket.Upgrader{}
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		connection, err := upgrader.Upgrade(w, request, nil)
		if err != nil {
			return
		}
		time.Sleep(20 * time.Millisecond)
		connection.Close()
	})
	router, err := NewRouter(
		testHomeResolver{client: delayedFiberClient{
			client: client, cloneDelay: delay, releaseDelay: delay,
		}},
		routes,
	)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		router.Close()
		for _, server := range client.servers {
			server.Close()
		}
		forgetRevision(route.Namespace, route.Revision)
	})
	server := serveRouter(router, route)
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial(
		"ws"+strings.TrimPrefix(server.URL, "http"),
		nil,
	)
	if err != nil {
		t.Fatal(err)
	}
	connection.ReadMessage()
	connection.Close()
	prefix := `lampyr_router_websocket_session_duration_seconds_sum{namespace="default",outcome="closed",revision="` + route.Revision + `"} `
	deadline := time.Now().Add(time.Second)
	for !strings.Contains(scrape(), prefix) && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if duration := sampleValue(t, scrape(), prefix); duration >= delay.Seconds() {
		t.Fatalf("WebSocket session duration includes clone or release: %fs", duration)
	}
}

func TestRouterMetricsFinalizeWebSocketBeforeRelease(t *testing.T) {
	routes := t.TempDir()
	route := RevisionRoute{
		Kind:      "revision",
		Namespace: "default",
		Revision:  "ws-release-4446",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh},
		Timeout:   "100ms",
		Routed:    true,
	}
	data, _ := json.Marshal(route)
	if err := os.WriteFile(filepath.Join(routes, "r.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	client := &testFiberClient{
		sessions: map[string]consumer.Fiber{},
		servers:  map[string]*httptest.Server{},
	}
	upgrader := websocket.Upgrader{}
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		connection, err := upgrader.Upgrade(w, request, nil)
		if err != nil {
			return
		}
		time.Sleep(10 * time.Millisecond)
		connection.Close()
	})
	releaseStarted := make(chan struct{})
	release := make(chan struct{})
	var releaseOnce sync.Once
	unblockRelease := func() { releaseOnce.Do(func() { close(release) }) }
	defer unblockRelease()
	router, err := NewRouter(
		testHomeResolver{client: blockingReleaseFiberClient{
			client: client, started: releaseStarted, release: release,
		}},
		routes,
	)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		router.Close()
		for _, server := range client.servers {
			server.Close()
		}
		forgetRevision(route.Namespace, route.Revision)
	})
	server := serveRouter(router, route)
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial(
		"ws"+strings.TrimPrefix(server.URL, "http"),
		nil,
	)
	if err != nil {
		t.Fatal(err)
	}
	connection.ReadMessage()
	connection.Close()
	select {
	case <-releaseStarted:
	case <-time.After(time.Second):
		t.Fatal("Fiber release did not start")
	}
	if got := testutil.ToFloat64(websocketSessions.WithLabelValues(route.Namespace, route.Revision)); got != 0 {
		t.Fatalf("WebSocket session remained active during Fiber release: %v", got)
	}
	body := scrape()
	closed := `lampyr_router_websocket_session_duration_seconds_count{namespace="default",outcome="closed",revision="` + route.Revision + `"} 1`
	if !strings.Contains(body, closed) {
		t.Fatalf("closed session was not finalized before Fiber release:\n%s", lines(body, route.Revision))
	}
	time.Sleep(120 * time.Millisecond)
	unblockRelease()
	body = scrape()
	if strings.Contains(body, `outcome="timeout",revision="`+route.Revision+`"} 1`) {
		t.Fatalf("slow Fiber release changed the WebSocket outcome:\n%s", lines(body, route.Revision))
	}
}

func TestRouterMetricsFinalizeProxyStageBeforeRelease(t *testing.T) {
	routes := t.TempDir()
	route := RevisionRoute{
		Kind:      "revision",
		Namespace: "default",
		Revision:  "proxy-release-4447",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh},
		Timeout:   "30s",
		Routed:    true,
	}
	data, _ := json.Marshal(route)
	if err := os.WriteFile(filepath.Join(routes, "r.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	client := &testFiberClient{
		sessions: map[string]consumer.Fiber{},
		servers:  map[string]*httptest.Server{},
	}
	releaseStarted := make(chan struct{})
	release := make(chan struct{})
	var releaseOnce sync.Once
	unblockRelease := func() { releaseOnce.Do(func() { close(release) }) }
	defer unblockRelease()
	router, err := NewRouter(
		testHomeResolver{client: blockingReleaseFiberClient{
			client: client, started: releaseStarted, release: release,
		}},
		routes,
	)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		router.Close()
		for _, server := range client.servers {
			server.Close()
		}
		forgetRevision(route.Namespace, route.Revision)
	})
	response := httptest.NewRecorder()
	done := make(chan struct{})
	go func() {
		router.ServeHTTP(response, revisionRequest("GET", "/", route))
		close(done)
	}()
	select {
	case <-releaseStarted:
	case <-time.After(time.Second):
		t.Fatal("Fiber release did not start")
	}
	if got := testutil.ToFloat64(requestsInFlight.WithLabelValues(
		route.Namespace, route.Revision, "proxy",
	)); got != 0 {
		t.Fatalf("proxy stage remained active during Fiber release: %v", got)
	}
	unblockRelease()
	<-done
	if response.Code != http.StatusOK {
		t.Fatalf("request = %d", response.Code)
	}
}

// A WebSocket session that runs to the Revision timeout is not a request.
func TestRouterMetricsSkipWebSocketAtRevisionTimeout(t *testing.T) {
	router, client, route, _ := namedRouter(t, "ws-5555", "1s", project.ExecutionFresh)
	upgrader := websocket.Upgrader{}
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer c.Close()
		for {
			if _, _, err := c.ReadMessage(); err != nil {
				return
			}
		}
	})
	server := serveRouter(router, route)
	defer server.Close()
	c, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	c.ReadMessage() // returns when the router closes the session at the timeout
	c.Close()
	time.Sleep(200 * time.Millisecond)
	t.Logf("\n%s", lines(scrape(), "ws-5555"))
	if strings.Contains(lines(scrape(), "ws-5555"), "request_duration") {
		t.Errorf("WebSocket session recorded as a request")
	}
}

// A client that reads a complete response and then closes its connection
// keeps the response status.
func TestRouterMetricsKeepStatusOfCompletedResponses(t *testing.T) {
	router, client, route, _ := namedRouter(t, "race-7777", "30s", project.ExecutionFresh)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Length", "2")
		w.Write([]byte("ok"))
	})
	server := serveRouter(router, route)
	defer server.Close()
	addr := strings.TrimPrefix(server.URL, "http://")
	const n = 300
	for i := 0; i < n; i++ {
		conn, err := net.Dial("tcp", addr)
		if err != nil {
			t.Fatal(err)
		}
		fmt.Fprintf(conn, "GET / HTTP/1.1\r\nHost: x\r\n\r\n")
		resp, err := http.ReadResponse(bufio.NewReader(conn), nil)
		if err != nil {
			t.Fatal(err)
		}
		io.ReadAll(resp.Body)
		if resp.StatusCode != 200 {
			t.Fatalf("status %d", resp.StatusCode)
		}
		conn.Close()
	}
	time.Sleep(300 * time.Millisecond)
	body := scrape()
	ok := countOf(body, "race-7777", "200")
	bad := countOf(body, "race-7777", "499")
	t.Logf("of %d complete 200 responses: recorded 200=%v 499=%v", n, ok, bad)
	if bad > 0 {
		t.Errorf("%v completed 200 responses recorded as 499", bad)
	}
}

// A request in flight across a remove and re-add of its Revision must not
// touch the new generation: the busy gauge stays at 0.
func TestRouterMetricsSurviveRemoveAndReAddInFlight(t *testing.T) {
	router, client, route, file := namedRouter(t, "readd-3333", "30s", project.ExecutionReuse)
	entered := make(chan struct{}, 1)
	release := make(chan struct{})
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		entered <- struct{}{}
		<-release
	})
	done := make(chan struct{})
	go func() {
		router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
		close(done)
	}()
	<-entered
	data, _ := os.ReadFile(file)
	os.Remove(file)
	router.Reload()
	os.WriteFile(file, data, 0o644)
	router.Reload()
	close(release)
	<-done
	got := testutil.ToFloat64(fibersBusy.WithLabelValues("default", "readd-3333"))
	t.Logf("busy after remove/re-add = %v\n%s", got, lines(scrape(), "readd-3333"))
	if got != 0 {
		t.Errorf("busy = %v", got)
	}
}

// An upstream that dies mid-body is a 502, and its Fiber is released.
func TestRouterMetricsRecordUpstreamAbortAs502(t *testing.T) {
	router, client, route, _ := namedRouter(t, "abort-2222", "30s", project.ExecutionReuse)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Length", "100")
		w.WriteHeader(200)
		w.Write([]byte("short"))
		w.(http.Flusher).Flush()
		c, _, _ := http.NewResponseController(w).Hijack()
		c.Close()
	})
	server := serveRouter(router, route)
	defer server.Close()
	resp, err := http.Get(server.URL)
	if err == nil {
		io.ReadAll(resp.Body)
		resp.Body.Close()
	}
	// The router records and releases after the client has its response,
	// so wait for both rather than for a fixed time.
	released := func() int {
		client.mu.Lock()
		defer client.mu.Unlock()
		return len(client.released)
	}
	deadline := time.Now().Add(10 * time.Second)
	body := scrape()
	for (countOf(body, "abort-2222", "502") != 1 || released() != 1) && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
		body = scrape()
	}
	if countOf(body, "abort-2222", "200") != 0 || countOf(body, "abort-2222", "502") != 1 {
		t.Fatalf("an upstream abort mid-body was not a 502:\n%s", lines(body, "abort-2222"))
	}
	if released() != 1 {
		t.Fatal("the aborted Fiber was not released")
	}
}

// Reloads racing requests must not deadlock or leave the busy gauge off.
func TestRouterMetricsUnderReloadStress(t *testing.T) {
	router, _, route, file := namedRouter(t, "stress-1111", "30s", project.ExecutionReuse)
	data, _ := os.ReadFile(file)
	stop := make(chan struct{})
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for i := 0; ; i++ {
			select {
			case <-stop:
				return
			default:
			}
			if i%2 == 0 {
				os.Remove(file)
			} else {
				os.WriteFile(file, data, 0o644)
			}
			router.Reload()
		}
	}()
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 200; i++ {
				router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
			}
		}()
	}
	time.Sleep(3 * time.Second)
	close(stop)
	finished := make(chan struct{})
	go func() { wg.Wait(); close(finished) }()
	select {
	case <-finished:
	case <-time.After(20 * time.Second):
		t.Fatal("deadlock")
	}
	os.WriteFile(file, data, 0o644)
	router.Reload()
	if got := testutil.ToFloat64(fibersBusy.WithLabelValues("default", "stress-1111")); got != 0 {
		t.Errorf("busy = %v after stress", got)
	}
}

func countOf(body, revision, code string) float64 {
	prefix := `lampyr_router_request_duration_seconds_count{code="` + code + `",namespace="default",revision="` + revision + `"} `
	for _, line := range strings.Split(body, "\n") {
		if strings.HasPrefix(line, prefix) {
			var v float64
			fmt.Sscan(strings.TrimPrefix(line, prefix), &v)
			return v
		}
	}
	return 0
}

// A WebSocket the client closes is not a request either.
func TestRouterMetricsSkipWebSocketClosedByClient(t *testing.T) {
	router, client, route, _ := namedRouter(t, "wsc-6666", "30s", project.ExecutionFresh)
	upgrader := websocket.Upgrader{}
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := upgrader.Upgrade(w, r, nil)
		if err != nil {
			return
		}
		defer c.Close()
		for {
			if _, _, err := c.ReadMessage(); err != nil {
				return
			}
		}
	})
	server := serveRouter(router, route)
	defer server.Close()
	c, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(100 * time.Millisecond)
	c.Close()
	time.Sleep(500 * time.Millisecond)
	if strings.Contains(scrape(), `lampyr_router_request_duration_seconds_count{code=`) &&
		strings.Contains(lines(scrape(), "wsc-6666"), "request_duration") {
		t.Fatalf("a WebSocket session was observed as a request:\n%s", lines(scrape(), "wsc-6666"))
	}
}
