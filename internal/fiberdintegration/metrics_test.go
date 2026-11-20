// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/hyperlight-dev/lampyr/internal/project"
	"github.com/prometheus/client_golang/prometheus/testutil"
)

func TestRouterMetricsCountFibersAndRequests(t *testing.T) {
	router, _, route, _ := namedRouter(t, "count-1001", "30s", project.ExecutionFresh)
	labels := []string{route.Namespace, route.Revision}
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/", route))
	if response.Code != 200 {
		t.Fatalf("request = %d", response.Code)
	}
	if got := testutil.ToFloat64(fibersBusy.WithLabelValues(labels...)); got != 0 {
		t.Fatalf("active fibers after the request = %v", got)
	}
	recorder := httptest.NewRecorder()
	MetricsHandler().ServeHTTP(recorder, httptest.NewRequest("GET", "/metrics", nil))
	body := recorder.Body.String()
	for _, want := range []string{
		`lampyr_router_request_duration_seconds_count{code="200",namespace="default",revision="count-1001"} 1`,
		`lampyr_router_fiber_acquire_seconds_count{kind="CREATE",namespace="default",revision="count-1001"} 1`,
		`lampyr_router_fibers_busy{namespace="default",revision="count-1001"} 0`,
	} {
		if !strings.Contains(body, want) {
			t.Fatalf("metrics are missing %q", want)
		}
	}
}

func TestRouterMetricsTrackRequestStages(t *testing.T) {
	router, client, route, _ := namedRouter(t, "stages-1001", "30s", project.ExecutionFresh)
	entered := make(chan struct{})
	finish := make(chan struct{})
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		close(entered)
		<-finish
		fmt.Fprint(w, "done")
	})
	done := make(chan struct{})
	go func() {
		router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
		close(done)
	}()
	<-entered
	if got := testutil.ToFloat64(requestsInFlight.WithLabelValues(route.Namespace, route.Revision, "acquire")); got != 0 {
		t.Fatalf("acquiring requests after proxy began = %v", got)
	}
	if got := testutil.ToFloat64(requestsInFlight.WithLabelValues(route.Namespace, route.Revision, "proxy")); got != 1 {
		t.Fatalf("proxying requests = %v, want 1", got)
	}
	close(finish)
	<-done
	if got := testutil.ToFloat64(requestsInFlight.WithLabelValues(route.Namespace, route.Revision, "proxy")); got != 0 {
		t.Fatalf("proxying requests after completion = %v", got)
	}
}

func TestRouterMetricsTrackRequestAcquisition(t *testing.T) {
	routes := t.TempDir()
	route := RevisionRoute{Kind: "revision", Namespace: "default", Revision: "acquiring-1001",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh}, Timeout: "30s", Routed: true}
	data, _ := json.Marshal(route)
	if err := os.WriteFile(filepath.Join(routes, "route.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	router, err := NewRouter(testHomeResolver{client: blockingFiberClient{}}, routes)
	if err != nil {
		t.Fatal(err)
	}
	defer router.Close()
	t.Cleanup(func() { forgetRevision(route.Namespace, route.Revision) })
	ctx, cancel := context.WithCancel(t.Context())
	done := make(chan struct{})
	go func() {
		router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route).WithContext(ctx))
		close(done)
	}()
	labels := []string{route.Namespace, route.Revision, "acquire"}
	deadline := time.Now().Add(time.Second)
	for testutil.ToFloat64(requestsInFlight.WithLabelValues(labels...)) != 1 && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if got := testutil.ToFloat64(requestsInFlight.WithLabelValues(labels...)); got != 1 {
		t.Fatalf("acquiring requests = %v, want 1", got)
	}
	cancel()
	<-done
	if got := testutil.ToFloat64(requestsInFlight.WithLabelValues(labels...)); got != 0 {
		t.Fatalf("acquiring requests after cancellation = %v", got)
	}
}

func TestRouterMetricsTrackReuseSlots(t *testing.T) {
	router, client, route, _ := namedRouter(t, "reuse-slots-1001", "30s", project.ExecutionReuse)
	entered := make(chan struct{})
	finish := make(chan struct{})
	calls := 0
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		calls++
		if calls == 2 {
			close(entered)
			<-finish
		}
		fmt.Fprint(w, "done")
	})
	router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
	labels := []string{route.Namespace, route.Revision}
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "idle")...)); got != 1 {
		t.Fatalf("idle reuse slots after request = %v, want 1", got)
	}
	if got := testutil.ToFloat64(reuseSlotLimit.WithLabelValues(labels...)); got != 2 {
		t.Fatalf("reuse slot limit = %v, want 2", got)
	}

	done := make(chan struct{})
	go func() {
		router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
		close(done)
	}()
	<-entered
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "busy")...)); got != 1 {
		t.Fatalf("busy reuse slots = %v, want 1", got)
	}
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "idle")...)); got != 0 {
		t.Fatalf("idle reuse slots while busy = %v", got)
	}
	close(finish)
	<-done
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "idle")...)); got != 1 {
		t.Fatalf("idle reuse slots after completion = %v, want 1", got)
	}

	router.mu.RLock()
	pool := router.pools[route.Namespace+"--"+route.Revision]
	router.mu.RUnlock()
	router.endPool(pool, false, time.Now().Add(time.Minute))
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "idle")...)); got != 0 {
		t.Fatalf("idle reuse slots after timeout = %v", got)
	}
}

func TestRetiredReusePoolCannotOverwriteReplacementMetrics(t *testing.T) {
	router, client, route, file := namedRouter(t, "reuse-replaced-1001", "30s", project.ExecutionReuse)
	oldEntered := make(chan struct{})
	oldFinish := make(chan struct{})
	calls := 0
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		calls++
		if calls == 1 {
			close(oldEntered)
			<-oldFinish
		}
		fmt.Fprint(w, "done")
	})
	oldDone := make(chan struct{})
	go func() {
		router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
		close(oldDone)
	}()
	<-oldEntered

	route.Routed = false
	data, _ := json.Marshal(route)
	if err := os.WriteFile(file, data, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	labels := []string{route.Namespace, route.Revision}
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "busy")...)); got != 0 {
		t.Fatalf("busy slots after route retired = %v", got)
	}

	route.Routed = true
	data, _ = json.Marshal(route)
	if err := os.WriteFile(file, data, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "idle")...)); got != 1 {
		t.Fatalf("replacement pool idle slots = %v, want 1", got)
	}

	close(oldFinish)
	<-oldDone
	if got := testutil.ToFloat64(reuseSlots.WithLabelValues(append(labels, "idle")...)); got != 1 {
		t.Fatalf("retired pool overwrote replacement idle slots: %v", got)
	}
}

func TestRouterMetricsIgnoreUnknownRevisions(t *testing.T) {
	router, _, route := routerFixture(t, project.ExecutionFresh)
	route.Revision = "unrouted-9999"
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/", route))
	recorder := httptest.NewRecorder()
	MetricsHandler().ServeHTTP(recorder, httptest.NewRequest("GET", "/metrics", nil))
	if strings.Contains(recorder.Body.String(), "unrouted-9999") {
		t.Fatal("an unknown Revision created metric labels")
	}
}

// The status recorder must not hide the underlying connection: WebSocket
// upgrades through the router hijack it.
func TestRouterMetricsKeepWebSocketUpgrades(t *testing.T) {
	router, client, route := routerFixture(t, project.ExecutionFresh)
	upgrader := websocket.Upgrader{}
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		connection, err := upgrader.Upgrade(w, request, nil)
		if err != nil {
			return
		}
		defer connection.Close()
		kind, message, err := connection.ReadMessage()
		if err == nil {
			connection.WriteMessage(kind, message)
		}
	})
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		request.Header.Set("Knative-Serving-Namespace", route.Namespace)
		request.Header.Set("Knative-Serving-Revision", route.Revision)
		router.ServeHTTP(w, request)
	}))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatalf("upgrade through the router: %v", err)
	}
	if got := testutil.ToFloat64(websocketSessions.WithLabelValues(route.Namespace, route.Revision)); got != 1 {
		t.Fatalf("active WebSocket sessions = %v, want 1", got)
	}
	if err := connection.WriteMessage(websocket.TextMessage, []byte("ping")); err != nil {
		t.Fatal(err)
	}
	if _, message, err := connection.ReadMessage(); err != nil || string(message) != "ping" {
		t.Fatalf("echo = %q, %v", message, err)
	}
	connection.Close()
	deadline := time.Now().Add(time.Second)
	duration := `lampyr_router_websocket_session_duration_seconds_count{namespace="default",outcome="closed",revision="` + route.Revision + `"} 1`
	for !strings.Contains(scrape(), duration) && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if got := testutil.ToFloat64(websocketSessions.WithLabelValues(route.Namespace, route.Revision)); got != 0 {
		t.Fatalf("active WebSocket sessions after close = %v", got)
	}
	body := scrape()
	if !strings.Contains(body, duration) {
		t.Fatalf("closed WebSocket session was not observed:\n%s", lines(body, route.Revision))
	}
}

func TestStatusRecorderSkipsInformationalResponses(t *testing.T) {
	recorder := &statusRecorder{ResponseWriter: httptest.NewRecorder()}
	recorder.WriteHeader(http.StatusEarlyHints)
	recorder.WriteHeader(http.StatusCreated)
	if recorder.code != http.StatusCreated {
		t.Fatalf("recorded %d, want the final status", recorder.code)
	}
}

func TestRouterMetricsForgetRemovedRevisions(t *testing.T) {
	router, _, route := routerFixture(t, project.ExecutionFresh)
	router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route))
	entries, err := os.ReadDir(router.routesDir)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if err := os.Remove(filepath.Join(router.routesDir, entry.Name())); err != nil {
			t.Fatal(err)
		}
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	recorder := httptest.NewRecorder()
	MetricsHandler().ServeHTTP(recorder, httptest.NewRequest("GET", "/metrics", nil))
	if strings.Contains(recorder.Body.String(), route.Revision) {
		t.Fatal("a removed Revision kept its series")
	}
}

func TestRouterMetricsRecordClientDisconnectAs499(t *testing.T) {
	router, client, route, _ := namedRouter(t, "cancel-1002", "30s", project.ExecutionFresh)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		<-request.Context().Done()
	})
	ctx, cancel := context.WithCancel(t.Context())
	request := revisionRequest("GET", "/", route).WithContext(ctx)
	go func() {
		time.Sleep(200 * time.Millisecond)
		cancel()
	}()
	router.ServeHTTP(httptest.NewRecorder(), request)
	recorder := httptest.NewRecorder()
	MetricsHandler().ServeHTTP(recorder, httptest.NewRequest("GET", "/metrics", nil))
	if !strings.Contains(recorder.Body.String(), `lampyr_router_request_duration_seconds_count{code="499",namespace="default",revision="cancel-1002"}`) {
		t.Fatal("a client disconnect was not recorded as 499")
	}
}
