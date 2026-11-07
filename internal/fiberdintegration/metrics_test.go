// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
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
	defer connection.Close()
	if err := connection.WriteMessage(websocket.TextMessage, []byte("ping")); err != nil {
		t.Fatal(err)
	}
	if _, message, err := connection.ReadMessage(); err != nil || string(message) != "ping" {
		t.Fatalf("echo = %q, %v", message, err)
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
