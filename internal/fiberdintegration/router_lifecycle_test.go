// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/hyperlight-dev/lampyr/internal/project"
)

func waitReleased(t *testing.T, client *testFiberClient) {
	t.Helper()
	deadline := time.Now().Add(time.Second)
	for {
		client.mu.Lock()
		released := len(client.released)
		client.mu.Unlock()
		if released == 1 {
			return
		}
		if time.Now().After(deadline) {
			t.Fatal("completed or cancelled request retained its fiber")
		}
		time.Sleep(time.Millisecond)
	}
}

func TestRouterObservesCompletedRequestMetrics(t *testing.T) {
	router, _, revision := routerFixture(t, project.ExecutionFresh)
	observed := make(chan RequestMetric, 1)
	router.Observe(func(metric RequestMetric) { observed <- metric })
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test?q=yes", revision))
	select {
	case metric := <-observed:
		if metric.Service != revision.Revision || metric.Method != "GET" ||
			metric.Path != "/test?q=yes" || metric.Status != 200 ||
			metric.Duration <= 0 || metric.CloneDuration < 0 ||
			metric.FiberID == "" || metric.FiberKind != "CREATE" {
			t.Fatalf("request metric = %+v", metric)
		}
	case <-time.After(time.Second):
		t.Fatal("completed request metric was not observed")
	}
}

func TestRouterMeasuresFailedFiberAcquisition(t *testing.T) {
	routes := t.TempDir()
	revision := RevisionRoute{
		Kind: "revision", Namespace: "default", Revision: "demo-1234",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh},
		Timeout:   "10ms", Routed: true,
	}
	data, err := json.Marshal(revision)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(routes, "revision.json"), data, 0o600); err != nil {
		t.Fatal(err)
	}
	router, err := NewRouter(testHomeResolver{client: blockingFiberClient{}}, routes)
	if err != nil {
		t.Fatal(err)
	}
	defer router.Close()
	observed := make(chan RequestMetric, 1)
	router.Observe(func(metric RequestMetric) { observed <- metric })
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	metric := <-observed
	if metric.Status != http.StatusGatewayTimeout || metric.CloneDuration <= 0 ||
		metric.FiberID != "" || metric.FiberKind != "" {
		t.Fatalf("failed acquisition metric = %+v", metric)
	}
}

func TestRouterObservesWebSocketUpgradeStatus(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		connection, buffered, err := w.(http.Hijacker).Hijack()
		if err != nil {
			t.Error(err)
			return
		}
		defer connection.Close()
		fmt.Fprint(buffered, "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
		buffered.Flush()
	})
	observed := make(chan RequestMetric, 1)
	router.Observe(func(metric RequestMetric) { observed <- metric })
	server := httptest.NewServer(router.Handler())
	defer server.Close()
	connection, err := net.Dial("tcp", strings.TrimPrefix(server.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	fmt.Fprintf(connection,
		"GET /socket HTTP/1.1\r\nHost: fiber-router\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nKnative-Serving-Namespace: %s\r\nKnative-Serving-Revision: %s\r\n\r\n",
		revision.Namespace, revision.Revision)
	status, err := bufio.NewReader(connection).ReadString('\n')
	connection.Close()
	if err != nil || !strings.Contains(status, "101 Switching Protocols") {
		t.Fatalf("upgrade response = %q, %v", status, err)
	}
	select {
	case metric := <-observed:
		if metric.Status != http.StatusSwitchingProtocols || metric.Path != "/socket" {
			t.Fatalf("upgrade metric = %+v", metric)
		}
	case <-time.After(time.Second):
		t.Fatal("upgrade metric was not observed")
	}
}

func TestRouterRefusesRequestsAfterMetricsDrainStarts(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	router.StopRequests()
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	router.WaitRequests(t.Context())
	if response.Code != http.StatusServiceUnavailable || client.next != 0 {
		t.Fatalf("draining router request = %d, clones=%d", response.Code, client.next)
	}
}

func TestRouterGracefulDrainWaitsForActiveRequests(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	started := make(chan struct{})
	finish := make(chan struct{})
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		close(started)
		<-finish
		fmt.Fprint(w, "done")
	})
	response := httptest.NewRecorder()
	done := make(chan struct{})
	go func() {
		defer close(done)
		router.ServeHTTP(response, revisionRequest("GET", "/slow", revision))
	}()
	<-started
	router.StopRequests()
	wait, cancel := context.WithTimeout(t.Context(), 20*time.Millisecond)
	defer cancel()
	if router.WaitRequests(wait) {
		t.Fatal("router drained before active request completed")
	}
	close(finish)
	if !router.WaitRequests(t.Context()) {
		t.Fatal("router did not drain after active request completed")
	}
	<-done
	if response.Code != http.StatusOK || response.Body.String() != "done" {
		t.Fatalf("drained response = %d %q", response.Code, response.Body.String())
	}
}

func TestProxyPreservesMissingContentTypeWithoutSniffingHTML(t *testing.T) {
	router, client, route := routerFixture(t, project.ExecutionFresh)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header()["Content-Type"] = nil
		w.Header().Set("X-Content-Type-Options", "nosniff")
		w.WriteHeader(http.StatusOK)
		w.(http.Flusher).Flush()
		fmt.Fprint(w, "<html><script>alert(1)</script>")
	})
	server := httptest.NewServer(localIngressHandler(router, map[string]string{
		"demo.default.localhost": route.Revision,
	}, "8080"))
	defer server.Close()
	request, err := http.NewRequest("GET", server.URL, nil)
	if err != nil {
		t.Fatal(err)
	}
	request.Host = "demo.default.localhost"
	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil || string(body) != "<html><script>alert(1)</script>" {
		t.Fatalf("proxy body = %q, error = %v", body, err)
	}
	if contentType := response.Header.Get("Content-Type"); contentType != "" {
		t.Fatalf("proxy added Content-Type %q to an untyped response", contentType)
	}
}

func TestFreshReleaseOnApplicationAndTransportErrors(t *testing.T) {
	for _, disconnect := range []bool{false, true} {
		t.Run(fmt.Sprint(disconnect), func(t *testing.T) {
			router, client, route := routerFixture(t, project.ExecutionFresh)
			client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
				if disconnect {
					connection, _, err := w.(http.Hijacker).Hijack()
					if err != nil {
						t.Error(err)
						return
					}
					connection.Close()
					return
				}
				http.Error(w, "application error", 500)
			})
			response := httptest.NewRecorder()
			router.ServeHTTP(response, revisionRequest("GET", "/", route))
			want := 500
			if disconnect {
				want = 502
			}
			if response.Code != want || len(client.released) != 1 {
				t.Fatalf("error lifecycle = %d, released=%v", response.Code, client.released)
			}
		})
	}
}

func TestFreshStreamRetainsFiberUntilCompletionOrCancellation(t *testing.T) {
	for _, cancelStream := range []bool{false, true} {
		t.Run(fmt.Sprint(cancelStream), func(t *testing.T) {
			router, client, route := routerFixture(t, project.ExecutionFresh)
			end := make(chan struct{})
			client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
				fmt.Fprint(w, "start\n")
				w.(http.Flusher).Flush()
				select {
				case <-request.Context().Done():
				case <-end:
					fmt.Fprint(w, "end\n")
				}
			})
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
				request.Header.Set("Knative-Serving-Namespace", route.Namespace)
				request.Header.Set("Knative-Serving-Revision", route.Revision)
				router.ServeHTTP(w, request)
			}))
			defer server.Close()
			ctx, cancel := context.WithCancel(t.Context())
			defer cancel()
			request, _ := http.NewRequestWithContext(ctx, "GET", server.URL, nil)
			response, err := http.DefaultClient.Do(request)
			if err != nil {
				t.Fatal(err)
			}
			start := make([]byte, len("start\n"))
			if _, err := io.ReadFull(response.Body, start); err != nil {
				t.Fatal(err)
			}
			client.mu.Lock()
			released := len(client.released)
			client.mu.Unlock()
			if released != 0 {
				t.Fatal("stream released before completion")
			}
			if cancelStream {
				cancel()
			} else {
				close(end)
				body, err := io.ReadAll(response.Body)
				if err != nil || string(body) != "end\n" {
					t.Fatalf("stream completion = %q, %v", body, err)
				}
			}
			response.Body.Close()
			waitReleased(t, client)
		})
	}
}

func TestLocalBindingPreservesEscapedPathsAndQuery(t *testing.T) {
	router, client, route := routerFixture(t, project.ExecutionFresh)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		fmt.Fprint(w, request.URL.RequestURI())
	})
	response := httptest.NewRecorder()
	request := httptest.NewRequest("GET", "http://unknown/_lampyr/token/a%2Fb?q=hello%20world", nil)
	localHandler(router, route.Revision, "/_lampyr/token").ServeHTTP(response, request)
	if response.Body.String() != "/a%2Fb?q=hello%20world" {
		t.Fatalf("binding rewrote escaped request path: %q", response.Body.String())
	}
}
