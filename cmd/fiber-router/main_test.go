// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"bytes"
	"context"
	"fmt"
	"net"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestWriteProbeResponse(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.URL.RequestURI() != "/readyz?version=test" {
			t.Fatalf("unexpected probe path: %s", request.URL.RequestURI())
		}
		response.WriteHeader(http.StatusServiceUnavailable)
		_, _ = response.Write([]byte("pending"))
	}))
	defer server.Close()

	_, portText, err := net.SplitHostPort(strings.TrimPrefix(server.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	port, err := net.LookupPort("tcp", portText)
	if err != nil {
		t.Fatal(err)
	}
	var output bytes.Buffer
	if err := writeProbeResponse(port, "/readyz?version=test", &output); err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(output.String(), "HTTP/1.1 503 Service Unavailable\r\n") {
		t.Fatalf("unexpected response: %q", output.String())
	}
	if !strings.HasSuffix(output.String(), "\r\n\r\npending") {
		t.Fatalf("missing response body: %q", output.String())
	}
}

func TestWriteProbeResponseRejectsInvalidInput(t *testing.T) {
	for _, test := range []struct {
		port int
		path string
	}{
		{0, "/readyz"},
		{65536, "/readyz"},
		{8080, "readyz"},
		{8080, "/readyz\nother"},
	} {
		if err := writeProbeResponse(test.port, test.path, &bytes.Buffer{}); err == nil {
			t.Fatalf("accepted port %d path %q", test.port, test.path)
		}
	}
}

// order records what a shutdown did, in order.
type order struct {
	mu     sync.Mutex
	events []string
}

func (o *order) add(event string) {
	o.mu.Lock()
	defer o.mu.Unlock()
	o.events = append(o.events, event)
}

func (o *order) list() []string {
	o.mu.Lock()
	defer o.mu.Unlock()
	return slices.Clone(o.events)
}

type fakeRouter struct{ order *order }

func (r fakeRouter) Run(ctx context.Context) {
	<-ctx.Done()
	r.order.add("run stopped")
}

func (r fakeRouter) Close() error {
	r.order.add("router closed")
	return nil
}

// The router closes only after the requests in flight drained and its
// reload loop stopped, so neither uses a closed router.
func TestServeClosesTheRouterAfterTheDrainAndTheLoop(t *testing.T) {
	events := &order{}
	started, finish := make(chan struct{}), make(chan struct{})
	server := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		close(started)
		<-finish
		events.add("request done")
	})}
	inner, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	listener := &closingListener{Listener: inner, closed: make(chan struct{})}
	ctx, cancel := context.WithCancel(context.Background())
	served := make(chan error)
	go func() { served <- serve(ctx, listener, server, fakeRouter{events}) }()
	requested := make(chan error, 1)
	go func() {
		response, err := http.Get("http://" + listener.Addr().String() + "/")
		if err == nil {
			response.Body.Close()
			if response.StatusCode != http.StatusOK {
				err = fmt.Errorf("status %s", response.Status)
			}
		}
		requested <- err
	}()
	<-started
	cancel()
	// Shutdown closes the listener first, and Serve returns as soon as it
	// does; a serve that went on without waiting for the drain would stop
	// the loop and close the router from here, which the order below
	// catches if this check runs first.
	<-listener.closed
	if got := events.list(); len(got) != 0 {
		t.Fatalf("during the drain %v, want the router untouched", got)
	}
	select {
	case err := <-served:
		t.Fatalf("serve returned %v during the drain", err)
	default:
	}
	close(finish)
	if err := <-served; err != nil {
		t.Fatal(err)
	}
	if err := <-requested; err != nil {
		t.Fatalf("the drained request failed: %v", err)
	}
	if got, want := events.list(), []string{"request done", "run stopped", "router closed"}; !slices.Equal(got, want) {
		t.Fatalf("shutdown order %v, want %v", got, want)
	}
}

// closingListener tells when Shutdown has closed it.
type closingListener struct {
	net.Listener
	once   sync.Once
	closed chan struct{}
}

func (listener *closingListener) Close() error {
	err := listener.Listener.Close()
	listener.once.Do(func() { close(listener.closed) })
	return err
}

// releasingRouter closes, as Router.Close does, only once every request
// has released its fiber, here once every handler has ended.
type releasingRouter struct {
	fakeRouter
	releases *sync.WaitGroup
}

func (r releasingRouter) Close() error {
	r.releases.Wait()
	return r.fakeRouter.Close()
}

// At the drain deadline a stream and a hijacked connection, which Shutdown
// does not wait for, see their request contexts end, so the router's
// requests release their fibers. Serve can return before their handlers
// have: once Shutdown gives up at the deadline, nothing waits for them but
// Router.Close, which waits for every release, as this fake does.
func TestServeEndsLongRequestsAtTheDrainDeadline(t *testing.T) {
	drain := drainTimeout
	drainTimeout = 200 * time.Millisecond
	t.Cleanup(func() { drainTimeout = drain })
	events := &order{}
	var running, releases sync.WaitGroup
	running.Add(2)
	releases.Add(2)
	server := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		if request.URL.Path == "/websocket" {
			conn, _, err := w.(http.Hijacker).Hijack()
			if err != nil {
				t.Error(err)
				return
			}
			defer conn.Close()
		} else {
			w.WriteHeader(http.StatusOK)
			w.(http.Flusher).Flush()
		}
		running.Done()
		<-request.Context().Done()
		events.add(request.URL.Path + " ended")
		releases.Done()
	})}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	served := make(chan error)
	go func() { served <- serve(ctx, listener, server, releasingRouter{fakeRouter{events}, &releases}) }()
	for _, path := range []string{"/stream", "/websocket"} {
		conn, err := net.Dial("tcp", listener.Addr().String())
		if err != nil {
			t.Fatal(err)
		}
		defer conn.Close()
		conn.Write([]byte("GET " + path + " HTTP/1.1\r\nHost: router\r\n\r\n"))
	}
	running.Wait()
	cancel()
	select {
	case err := <-served:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("serve did not return after the drain deadline")
	}
	got := events.list()
	stream, websocket := slices.Index(got, "/stream ended"), slices.Index(got, "/websocket ended")
	stopped, closed := slices.Index(got, "run stopped"), slices.Index(got, "router closed")
	if len(got) != 4 || stream < 0 || websocket < 0 || stopped < 0 || closed != 3 {
		t.Fatalf("shutdown order %v, want both requests ended and the loop stopped before the router closed", got)
	}
}

// A listener that fails with no shutdown still ends the requests in
// flight, or Router.Close would wait for their fibers forever.
func TestServeEndsRequestsWhenTheListenerFails(t *testing.T) {
	events := &order{}
	var releases sync.WaitGroup
	releases.Add(1)
	started := make(chan struct{})
	server := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		w.WriteHeader(http.StatusOK)
		w.(http.Flusher).Flush()
		close(started)
		<-request.Context().Done()
		events.add("request ended")
		releases.Done()
	})}
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	served := make(chan error, 1)
	go func() { served <- serve(ctx, listener, server, releasingRouter{fakeRouter{events}, &releases}) }()
	conn, err := net.Dial("tcp", listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()
	conn.Write([]byte("GET /stream HTTP/1.1\r\nHost: router\r\n\r\n"))
	<-started
	listener.Close()
	select {
	case err := <-served:
		if err == nil {
			t.Fatal("serve returned nil, want the listener's error")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("serve did not return after the listener failed")
	}
	// Serve stops the loop while the handler may still be seeing its
	// context end, so only Router.Close's wait for the release puts the
	// request ahead of anything.
	got := events.list()
	ended, stopped := slices.Index(got, "request ended"), slices.Index(got, "run stopped")
	if len(got) != 3 || ended < 0 || stopped < 0 || slices.Index(got, "router closed") != 2 {
		t.Fatalf("shutdown order %v, want the request ended and the loop stopped before the router closed", got)
	}
}
