// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
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

func scrape() string {
	recorder := httptest.NewRecorder()
	MetricsHandler().ServeHTTP(recorder, httptest.NewRequest("GET", "/metrics", nil))
	return recorder.Body.String()
}

func lines(body, needle string) string {
	var out []string
	for _, line := range strings.Split(body, "\n") {
		if strings.Contains(line, needle) && !strings.HasPrefix(line, "#") && (strings.Contains(line, "_count") || strings.Contains(line, "busy")) {
			out = append(out, line)
		}
	}
	return strings.Join(out, "\n")
}

// A request in flight while its Revision is removed must not recreate the
// series forgetRevision deleted.
func TestRouterMetricsStayForgottenAfterInFlightRequest(t *testing.T) {
	router, client, route, _ := namedRouter(t, "inflight-1004", "30s", project.ExecutionFresh)
	entered := make(chan struct{})
	release := make(chan struct{})
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		close(entered)
		<-release
		w.WriteHeader(200)
	})
	done := make(chan int)
	go func() {
		response := httptest.NewRecorder()
		router.ServeHTTP(response, revisionRequest("GET", "/", route))
		done <- response.Code
	}()
	<-entered
	entries, _ := os.ReadDir(router.routesDir)
	for _, entry := range entries {
		os.Remove(filepath.Join(router.routesDir, entry.Name()))
	}
	if err := router.Reload(); err != nil {
		t.Fatal(err)
	}
	if strings.Contains(scrape(), route.Revision) {
		t.Fatal("series survived forgetRevision")
	}
	close(release)
	code := <-done
	body := scrape()
	t.Logf("request code %d; series after request finished:\n%s", code, lines(body, route.Revision))
	if strings.Contains(body, route.Revision) {
		t.Errorf("removed Revision %s has series again", route.Revision)
	}
}

// A client that disconnects after the headers were sent is a 499, against a
// real server where ReverseProxy aborts the handler mid-body.
func TestRouterMetricsRecordMidStreamDisconnectAs499(t *testing.T) {
	router, client, route, _ := namedRouter(t, "midstream-1003", "30s", project.ExecutionFresh)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(200)
		w.(http.Flusher).Flush()
		for {
			select {
			case <-r.Context().Done():
				return
			case <-time.After(20 * time.Millisecond):
				if _, err := w.Write([]byte("tick\n")); err != nil {
					return
				}
				w.(http.Flusher).Flush()
			}
		}
	})
	handled := make(chan struct{})
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		defer close(handled)
		r.Header.Set("Knative-Serving-Namespace", route.Namespace)
		r.Header.Set("Knative-Serving-Revision", route.Revision)
		router.ServeHTTP(w, r)
	}))
	defer server.Close()
	conn, err := net.Dial("tcp", strings.TrimPrefix(server.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	fmt.Fprintf(conn, "GET /stream HTTP/1.1\r\nHost: x\r\n\r\n")
	reader := bufio.NewReader(conn)
	status, _ := reader.ReadString('\n')
	t.Logf("client saw %q", strings.TrimSpace(status))
	time.Sleep(100 * time.Millisecond)
	conn.Close()
	select {
	case <-handled:
	case <-time.After(5 * time.Second):
		t.Fatal("handler did not finish")
	}
	time.Sleep(50 * time.Millisecond)
	body := scrape()
	t.Logf("series:\n%s", lines(body, "request_duration"))
	if !strings.Contains(body, `code="499",namespace="default",revision="midstream-1003"`) {
		t.Errorf("mid-stream client disconnect not recorded as 499")
	}
}

// A client that goes away while the router is still acquiring a Fiber is a
// 499 and a canceled acquisition, not a 503 and an error.
func TestRouterMetricsRecordCancelDuringAcquireAs499(t *testing.T) {
	routes := t.TempDir()
	route := RevisionRoute{Kind: "revision", Namespace: "default", Revision: "acq-1111",
		Execution: project.ExecutionPolicy{Mode: project.ExecutionFresh}, Timeout: "30s", Routed: true}
	data, _ := json.Marshal(route)
	os.WriteFile(filepath.Join(routes, "r.json"), data, 0o644)
	router, err := NewRouter(testHomeResolver{client: blockingFiberClient{}}, routes)
	if err != nil {
		t.Fatal(err)
	}
	defer router.Close()
	t.Cleanup(func() { forgetRevision(route.Namespace, route.Revision) })
	ctx, cancel := context.WithCancel(context.Background())
	go func() { time.Sleep(100 * time.Millisecond); cancel() }()
	router.ServeHTTP(httptest.NewRecorder(), revisionRequest("GET", "/", route).WithContext(ctx))
	body := scrape()
	t.Logf("series:\n%s", lines(body, "acq-1111"))
	if !strings.Contains(body, `code="499",namespace="default",revision="acq-1111"`) {
		t.Errorf("client cancel during acquire is not 499")
	}
	if !strings.Contains(body, `kind="canceled",namespace="default",revision="acq-1111"`) {
		t.Errorf("client cancel during acquire is not a canceled acquisition")
	}
}

func TestStatusLabelBucketsUnknownCodes(t *testing.T) {
	for code, want := range map[int]string{200: "200", 499: "499", 504: "504", 299: "other", 999: "other"} {
		if got := statusLabel(code); got != want {
			t.Errorf("statusLabel(%d) = %q, want %q", code, got, want)
		}
	}
}
