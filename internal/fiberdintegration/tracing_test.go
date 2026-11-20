// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/hyperlight-dev/lampyr/internal/project"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/propagation"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"
)

func TestRouterTracesFiberAcquisitionAndPropagatesContext(t *testing.T) {
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
