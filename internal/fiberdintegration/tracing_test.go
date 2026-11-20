// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"bufio"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/gorilla/websocket"
	"github.com/hyperlight-dev/lampyr/internal/project"
	"go.opentelemetry.io/contrib/instrumentation/net/http/otelhttp"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	otelcodes "go.opentelemetry.io/otel/codes"
	"go.opentelemetry.io/otel/propagation"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"
	semconv "go.opentelemetry.io/otel/semconv/v1.37.0"
	"go.opentelemetry.io/otel/trace"
)

func recordSpans(t *testing.T) *tracetest.SpanRecorder {
	previous := otel.GetTracerProvider()
	previousPropagation := otel.GetTextMapPropagator()
	recorder := tracetest.NewSpanRecorder()
	provider := sdktrace.NewTracerProvider(sdktrace.WithSpanProcessor(recorder))
	otel.SetTracerProvider(provider)
	otel.SetTextMapPropagator(propagation.TraceContext{})
	t.Cleanup(func() {
		_ = provider.Shutdown(t.Context())
		otel.SetTracerProvider(previous)
		otel.SetTextMapPropagator(previousPropagation)
	})
	return recorder
}

func waitForSpan(t *testing.T, recorder *tracetest.SpanRecorder, name string) sdktrace.ReadOnlySpan {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		for _, span := range recorder.Ended() {
			if span.Name() == name {
				return span
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	var started, ended []string
	for _, span := range recorder.Started() {
		started = append(started, span.Name())
	}
	for _, span := range recorder.Ended() {
		ended = append(ended, span.Name())
	}
	t.Fatalf("span %q did not end; started=%v ended=%v", name, started, ended)
	return nil
}

func waitForServerSpan(t *testing.T, recorder *tracetest.SpanRecorder) sdktrace.ReadOnlySpan {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		for _, span := range recorder.Ended() {
			if span.SpanKind() == trace.SpanKindServer {
				return span
			}
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("server span did not end")
	return nil
}

func tracedRouterServer(router *Router, revision RevisionRoute) *httptest.Server {
	handler := otelhttp.NewHandler(http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		request.Header.Set("Knative-Serving-Namespace", revision.Namespace)
		request.Header.Set("Knative-Serving-Revision", revision.Revision)
		router.ServeHTTP(w, request)
	}), "lampyr.router.request")
	return httptest.NewServer(handler)
}

func spanStatusCode(span sdktrace.ReadOnlySpan) int64 {
	for _, value := range span.Attributes() {
		if value.Key == semconv.HTTPResponseStatusCodeKey {
			return value.Value.AsInt64()
		}
	}
	return 0
}

func TestRouterTracesFiberAcquisitionAndPropagatesContext(t *testing.T) {
	recorder := recordSpans(t)

	router, client, revision := routerFixture(t, project.ExecutionFresh)
	traceparent := make(chan string, 1)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		traceparent <- request.Header.Get("traceparent")
		w.WriteHeader(http.StatusOK)
	})
	requestCtx, span := otel.Tracer("test").Start(t.Context(), "incoming")
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/", revision).WithContext(requestCtx))
	span.End()
	if response.Code != http.StatusOK {
		t.Fatalf("request = %d", response.Code)
	}
	if got := <-traceparent; got == "" {
		t.Fatal("proxy request did not carry trace context")
	}

	var acquire sdktrace.ReadOnlySpan
	for _, ended := range recorder.Ended() {
		if ended.Name() == "lampyr.fiber.acquire" {
			acquire = ended
			break
		}
	}
	if acquire == nil {
		t.Fatal("Fiber acquisition span was not recorded")
	}
	attributes := map[attribute.Key]attribute.Value{}
	for _, value := range acquire.Attributes() {
		attributes[value.Key] = value.Value
	}
	for key, want := range map[attribute.Key]string{
		"lampyr.namespace":      revision.Namespace,
		"lampyr.revision":       revision.Revision,
		"lampyr.execution.mode": project.ExecutionFresh,
		"lampyr.fiber.kind":     "CREATE",
	} {
		if got := attributes[key].AsString(); got != want {
			t.Errorf("%s = %q, want %q", key, got, want)
		}
	}
	if attributes["lampyr.fiber.id"].AsString() == "" {
		t.Error("Fiber acquisition span has no Fiber ID")
	}
}

func TestRouterTracesWebSocketUpgradeAs101(t *testing.T) {
	recorder := recordSpans(t)
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	upgrader := websocket.Upgrader{}
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		connection, err := upgrader.Upgrade(w, request, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer connection.Close()
		_, _, _ = connection.ReadMessage()
	})
	server := tracedRouterServer(router, revision)
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial(
		"ws"+strings.TrimPrefix(server.URL, "http"), nil,
	)
	if err != nil {
		t.Fatal(err)
	}
	if err := connection.WriteMessage(websocket.TextMessage, []byte("done")); err != nil {
		t.Fatal(err)
	}
	if err := connection.Close(); err != nil {
		t.Fatal(err)
	}
	span := waitForServerSpan(t, recorder)
	if got := spanStatusCode(span); got != http.StatusSwitchingProtocols {
		t.Fatalf("HTTP response status = %d, want 101", got)
	}
	if got := span.Status().Code; got != otelcodes.Unset {
		t.Fatalf("span status = %v, want unset", got)
	}
}

func TestRouterTracesResponseCopyAbortAs502(t *testing.T) {
	recorder := recordSpans(t)
	router, client, revision := routerFixture(t, project.ExecutionFresh)
	client.handler = http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Length", "100")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("short"))
		w.(http.Flusher).Flush()
		connection, _, err := http.NewResponseController(w).Hijack()
		if err != nil {
			t.Error(err)
			return
		}
		_ = connection.Close()
	})
	server := tracedRouterServer(router, revision)
	defer server.Close()
	connection, err := net.Dial("tcp", strings.TrimPrefix(server.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	_, _ = fmt.Fprintf(connection, "GET / HTTP/1.1\r\nHost: router\r\n\r\n")
	response, err := http.ReadResponse(bufio.NewReader(connection), nil)
	if err == nil {
		_, _ = io.ReadAll(response.Body)
		_ = response.Body.Close()
	}
	_ = connection.Close()
	span := waitForServerSpan(t, recorder)
	if got := spanStatusCode(span); got != http.StatusBadGateway {
		t.Fatalf("HTTP response status = %d, want 502", got)
	}
	if got := span.Status().Code; got != otelcodes.Error {
		t.Fatalf("span status = %v, want error", got)
	}
}

func TestAbortLeavesUpgradedSpanForWebSocketFinalizer(t *testing.T) {
	recorder := recordSpans(t)
	ctx, span := otel.Tracer("test").Start(
		t.Context(),
		"upgraded",
		trace.WithSpanKind(trace.SpanKindServer),
	)
	status := &statusRecorder{code: http.StatusSwitchingProtocols, switched: true}
	finishAbortServerSpan(span, status, ctx, ctx)
	if ended := recorder.Ended(); len(ended) != 0 {
		t.Fatalf("abort finalizer ended an upgraded span before WebSocket finalization: %v", ended)
	}
	span.End()
}
