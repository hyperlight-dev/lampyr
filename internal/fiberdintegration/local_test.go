// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"bytes"
	"context"
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
	"github.com/hyperlight-dev/lampyr/internal/project"
)

func TestLocalBindingRequiresCapabilityBeforeAcquiringFiber(t *testing.T) {
	router, client, route := routerFixture(t, project.ExecutionFresh)
	handler := localHandler(router, route.Revision, "/_lampyr/private")
	for _, path := range []string{"/test", "/_lampyr/wrong/test", "/_lampyr/private-other/test"} {
		response := httptest.NewRecorder()
		handler.ServeHTTP(response, httptest.NewRequest("GET", "http://localhost"+path, nil))
		if response.Code != 404 {
			t.Fatalf("unauthorized binding request = %d", response.Code)
		}
	}
	if client.next != 0 {
		t.Fatal("unauthorized request acquired a fiber")
	}
	for _, path := range []string{"/_lampyr/private", "/_lampyr/private/test"} {
		response := httptest.NewRecorder()
		handler.ServeHTTP(response, httptest.NewRequest("GET", "http://unknown"+path, nil))
		if response.Code != 200 {
			t.Fatalf("authorized binding %q = %d", path, response.Code)
		}
	}
	if client.next != 2 || len(client.released) != 2 {
		t.Fatal("authorized bindings did not use the routed lifecycle")
	}

	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		http.Redirect(w, request, "/target", http.StatusFound)
	})
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest("GET", "http://unknown/_lampyr/private/start", nil))
	if location := response.Header().Get("Location"); location != "/_lampyr/private/target" {
		t.Fatalf("binding redirect location = %q", location)
	}
	if location := response.Result().Header.Get("Location"); location != "/_lampyr/private/target" {
		t.Fatalf("binding redirect result location = %q", location)
	}

	client.handler = http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Location", "https://example.com/target")
		w.WriteHeader(http.StatusFound)
	})
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, httptest.NewRequest("GET", "http://unknown/_lampyr/private/start", nil))
	if location := response.Header().Get("Location"); location != "https://example.com/target" {
		t.Fatalf("external redirect location = %q", location)
	}
}

func TestLocalIngressOnlyExposesDeclaredHostnames(t *testing.T) {
	router, client, route := routerFixture(t, project.ExecutionFresh)
	handler := localIngressHandler(router, map[string]string{"demo.default.localhost": route.Revision}, "8080")
	for _, host := range []string{"localhost:8080", "127.0.0.1:8080", "internal.default.localhost", "demo.default.localhost.invalid", "[::1]:8080", "demo.default.localhost:bad"} {
		response := httptest.NewRecorder()
		request := httptest.NewRequest("GET", "/hello", nil)
		request.Host = host
		handler.ServeHTTP(response, request)
		if response.Code != http.StatusNotFound || client.next != 0 {
			t.Fatalf("undeclared host %q = %d, clones=%d", host, response.Code, client.next)
		}
	}
	for _, host := range []string{"demo.default.localhost:8080", "DEMO.default.localhost.:8080"} {
		response := httptest.NewRecorder()
		request := httptest.NewRequest("GET", "/hello?q=test", nil)
		request.Host = host
		request.Header.Set("Knative-Serving-Namespace", "unregistered")
		request.Header.Set("Knative-Serving-Revision", "unregistered")
		request.Header.Set("K-Original-Host", "internal.default.localhost")
		handler.ServeHTTP(response, request)
		if response.Code != http.StatusOK || response.Header().Get("X-Test-Host") != host ||
			response.Header().Get("X-Test-Revision") != "" || response.Header().Get("X-Test-Original-Host") != "" {
			t.Fatalf("host %q selection or host preservation failed: %d %#v", host, response.Code, response.Header())
		}
	}
}

func TestLocalServiceIndexListsOnlyExposedServicesWithoutAcquiringFibers(t *testing.T) {
	router, client, route := routerFixture(t, project.ExecutionFresh)
	server := httptest.NewServer(localIngressHandler(router, map[string]string{
		"demo.default.localhost": route.Revision,
	}, "8080"))
	defer server.Close()
	for _, host := range []string{"localhost:8080", "127.0.0.1:8080", "unknown.default.localhost:9999"} {
		request, err := http.NewRequest("GET", server.URL, nil)
		if err != nil {
			t.Fatal(err)
		}
		request.Host = host
		response, err := http.DefaultClient.Do(request)
		if err != nil {
			t.Fatal(err)
		}
		body, err := io.ReadAll(response.Body)
		response.Body.Close()
		expected := http.StatusOK
		if strings.HasPrefix(host, "unknown.") {
			expected = http.StatusNotFound
		}
		if err != nil || response.StatusCode != expected ||
			!strings.Contains(string(body), `href="http://demo.default.localhost:8080/"`) ||
			strings.Contains(string(body), "internal.default.localhost") ||
			strings.Contains(string(body), ":9999") || client.next != 0 {
			t.Fatalf("index for %q: status=%d body=%s err=%v clones=%d", host, response.StatusCode, body, err, client.next)
		}
	}
}

func TestLocalHomeEnforcesCapacityWithoutBlockingSessionAttach(t *testing.T) {
	home := &localHome{fibers: map[string]*localFiber{}}
	for index := range 8 {
		id := fmt.Sprint(index)
		home.fibers[id] = &localFiber{
			value:   consumer.Fiber{ID: id},
			session: "session-" + id,
			done:    make(chan struct{}),
		}
	}
	_, err := home.Clone(t.Context(), "", "", time.Second, nil)
	var shed *consumer.Shed
	if !errors.As(err, &shed) || shed.RetryAfter != time.Second {
		t.Fatalf("full local home did not shed: %v", err)
	}
	fiber, err := home.Clone(t.Context(), "", "session-0", time.Second, nil)
	if err != nil || fiber.ID != "0" || fiber.Kind != consumer.Attach {
		t.Fatalf("existing live session did not attach at capacity: %v, %v", fiber, err)
	}
	ctx, cancel := context.WithCancel(t.Context())
	cancel()
	if _, err := home.Clone(ctx, "", "session-0", time.Second, nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancelled acquisition proceeded: %v", err)
	}
}

func TestLocalHomeStartsColdFibersConcurrently(t *testing.T) {
	syncDir := t.TempDir()
	t.Setenv("LAMPYR_LOCAL_HELPER_SYNC", syncDir)
	wrapper := filepath.Join(t.TempDir(), "hluk")
	script := fmt.Sprintf("#!/bin/sh\nexec %s -test.run=^TestLocalFiberProcess$ -- \"$@\"\n", strconv.Quote(os.Args[0]))
	if err := os.WriteFile(wrapper, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	snapshot := filepath.Join(t.TempDir(), "snapshot")
	if err := os.Mkdir(snapshot, 0o700); err != nil {
		t.Fatal(err)
	}
	home := &localHome{
		binary: wrapper, service: LocalService{Name: "service", Snapshot: snapshot, Command: "/app"},
		bindings: map[string]string{}, fibers: map[string]*localFiber{}, hostIP: "192.168.1.1",
	}
	t.Cleanup(func() {
		home.mu.Lock()
		defer home.mu.Unlock()
		for id := range home.fibers {
			if err := home.release(id); err != nil {
				t.Error(err)
			}
		}
	})
	ctx, cancel := context.WithTimeout(t.Context(), 3*time.Second)
	defer cancel()
	results := make(chan error, 2)
	var started sync.WaitGroup
	for range 2 {
		started.Add(1)
		go func() {
			defer started.Done()
			_, err := home.Clone(ctx, "", "", time.Second, nil)
			results <- err
		}()
	}
	started.Wait()
	close(results)
	for err := range results {
		if err != nil {
			t.Fatalf("parallel clone: %v", err)
		}
	}
}

func TestLocalHomeWakesSessionWaitersWhenStartupExits(t *testing.T) {
	syncDir := t.TempDir()
	t.Setenv("LAMPYR_LOCAL_HELPER_SYNC", syncDir)
	t.Setenv("LAMPYR_LOCAL_HELPER_EXIT", "1")
	wrapper := filepath.Join(t.TempDir(), "hluk")
	script := fmt.Sprintf("#!/bin/sh\nexec %s -test.run=^TestLocalFiberProcess$ -- \"$@\"\n", strconv.Quote(os.Args[0]))
	if err := os.WriteFile(wrapper, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	snapshot := filepath.Join(t.TempDir(), "snapshot")
	if err := os.Mkdir(snapshot, 0o700); err != nil {
		t.Fatal(err)
	}
	home := &localHome{
		binary: wrapper, service: LocalService{Name: "service", Snapshot: snapshot, Command: "/app"},
		bindings: map[string]string{}, fibers: map[string]*localFiber{}, hostIP: "192.168.1.1",
	}
	t.Cleanup(func() {
		home.mu.Lock()
		defer home.mu.Unlock()
		for id := range home.fibers {
			if err := home.release(id); err != nil {
				t.Error(err)
			}
		}
	})
	ctx, cancel := context.WithTimeout(t.Context(), 2*time.Second)
	defer cancel()
	results := make(chan error, 2)
	go func() {
		_, err := home.Clone(ctx, "", "session", time.Second, nil)
		results <- err
	}()
	for {
		entries, _ := os.ReadDir(syncDir)
		if len(entries) > 0 {
			break
		}
		select {
		case <-ctx.Done():
			t.Fatal("first local Fiber did not start")
		case <-time.After(time.Millisecond):
		}
	}
	go func() {
		_, err := home.Clone(ctx, "", "session", time.Second, nil)
		results <- err
	}()
	time.Sleep(20 * time.Millisecond)
	if err := os.WriteFile(filepath.Join(syncDir, "exit"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	for range 2 {
		err := <-results
		if err == nil || errors.Is(err, context.DeadlineExceeded) {
			t.Fatalf("startup exit result = %v", err)
		}
	}
}

func TestLocalHomeDoesNotAcceptACompetingListener(t *testing.T) {
	syncDir := t.TempDir()
	t.Setenv("LAMPYR_LOCAL_HELPER_SYNC", syncDir)
	t.Setenv("LAMPYR_LOCAL_HELPER_COMPETE", "1")
	wrapper := filepath.Join(t.TempDir(), "hluk")
	script := fmt.Sprintf("#!/bin/sh\nexec %s -test.run=^TestLocalFiberProcess$ -- \"$@\"\n", strconv.Quote(os.Args[0]))
	if err := os.WriteFile(wrapper, []byte(script), 0o700); err != nil {
		t.Fatal(err)
	}
	snapshot := filepath.Join(t.TempDir(), "snapshot")
	if err := os.Mkdir(snapshot, 0o700); err != nil {
		t.Fatal(err)
	}
	home := &localHome{
		binary: wrapper, service: LocalService{Name: "service", Snapshot: snapshot, Command: "/app"},
		bindings: map[string]string{}, fibers: map[string]*localFiber{}, hostIP: "192.168.1.1",
	}
	ctx, cancel := context.WithTimeout(t.Context(), 2*time.Second)
	defer cancel()
	result := make(chan error, 1)
	go func() {
		_, err := home.Clone(ctx, "", "", time.Second, nil)
		result <- err
	}()
	var port string
	for port == "" {
		entries, _ := os.ReadDir(syncDir)
		if len(entries) > 0 {
			port = entries[0].Name()
			break
		}
		select {
		case <-ctx.Done():
			t.Fatal("local Fiber did not publish its port")
		case <-time.After(time.Millisecond):
		}
	}
	listener, err := net.Listen("tcp4", net.JoinHostPort("127.0.0.1", port))
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	if err := os.WriteFile(filepath.Join(syncDir, "competing"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := <-result; err == nil {
		t.Fatal("local Fiber accepted a competing listener as ready")
	}
}

func TestLocalFiberProcess(t *testing.T) {
	syncDir := os.Getenv("LAMPYR_LOCAL_HELPER_SYNC")
	if syncDir == "" {
		return
	}
	var port string
	for index, argument := range os.Args {
		if argument == "--port" && index+1 < len(os.Args) {
			port = os.Args[index+1]
		}
	}
	if port == "" {
		os.Exit(2)
	}
	if err := os.WriteFile(filepath.Join(syncDir, port), nil, 0o600); err != nil {
		os.Exit(3)
	}
	if os.Getenv("LAMPYR_LOCAL_HELPER_COMPETE") != "" {
		for {
			if _, err := os.Stat(filepath.Join(syncDir, "competing")); err == nil {
				select {}
			}
			time.Sleep(time.Millisecond)
		}
	}
	if os.Getenv("LAMPYR_LOCAL_HELPER_EXIT") != "" {
		for {
			if _, err := os.Stat(filepath.Join(syncDir, "exit")); err == nil {
				os.Exit(6)
			}
			time.Sleep(time.Millisecond)
		}
	}
	deadline := time.Now().Add(2 * time.Second)
	for {
		entries, err := os.ReadDir(syncDir)
		if err == nil && len(entries) >= 2 {
			break
		}
		if time.Now().After(deadline) {
			os.Exit(4)
		}
		time.Sleep(time.Millisecond)
	}
	listener, err := net.Listen("tcp", net.JoinHostPort("127.0.0.1", port))
	if err != nil {
		os.Exit(5)
	}
	defer listener.Close()
	for {
		connection, err := listener.Accept()
		if err != nil {
			return
		}
		connection.Close()
	}
}

func TestLocalMetricsPrintCallsAndExitSummary(t *testing.T) {
	var output bytes.Buffer
	metrics := newLocalMetrics(&output)
	metrics.Observe(RequestMetric{
		Service: "api", Method: "GET", Path: "/one", Status: 200,
		Duration: 20 * time.Millisecond, CloneDuration: 10 * time.Millisecond,
		FiberID: "api-1", FiberKind: "CREATE",
	})
	metrics.Observe(RequestMetric{
		Service: "api", Method: "GET", Path: "/two", Status: 500,
		Duration: 40 * time.Millisecond, CloneDuration: 0,
		FiberID: "api-1", FiberKind: "ATTACH",
	})
	metrics.Summary()
	text := output.String()
	for _, expected := range []string{
		`metric service=api method=GET path="/one" status=200 duration_ms=20 clone_ms=10 fiber_id="api-1" fiber=CREATE`,
		`metric service=api method=GET path="/two" status=500 duration_ms=40 clone_ms=0 fiber_id="api-1" fiber=ATTACH`,
		`metrics summary requests=2 errors=1 create=1 attach=1 avg_duration_ms=30 avg_clone_ms=5`,
	} {
		if !strings.Contains(text, expected) {
			t.Fatalf("metrics output missing %q:\n%s", expected, text)
		}
	}
}
