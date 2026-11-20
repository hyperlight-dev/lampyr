// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package telemetry

import (
	"context"
	"testing"

	semconv "go.opentelemetry.io/otel/semconv/v1.37.0"
)

func TestTracingRequiresAnEndpoint(t *testing.T) {
	t.Setenv("OTEL_SDK_DISABLED", "")
	t.Setenv("OTEL_EXPORTER_OTLP_ENDPOINT", "")
	t.Setenv("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "")
	if tracingEnabled() {
		t.Fatal("tracing enabled without an exporter endpoint")
	}
	t.Setenv("OTEL_EXPORTER_OTLP_ENDPOINT", "http://tempo:4317")
	if !tracingEnabled() {
		t.Fatal("OTLP endpoint did not enable tracing")
	}
	t.Setenv("OTEL_SDK_DISABLED", "true")
	if tracingEnabled() {
		t.Fatal("OTEL_SDK_DISABLED did not disable tracing")
	}
}

func TestTracingResourceLetsEnvironmentOverrideDefaultServiceName(t *testing.T) {
	for _, test := range []struct {
		name               string
		serviceName        string
		resourceAttributes string
		want               string
	}{
		{"service name", "configured-router", "", "configured-router"},
		{"resource attributes", "", "service.name=attributed-router", "attributed-router"},
	} {
		t.Run(test.name, func(t *testing.T) {
			t.Setenv("OTEL_SERVICE_NAME", test.serviceName)
			t.Setenv("OTEL_RESOURCE_ATTRIBUTES", test.resourceAttributes)
			res, err := tracingResource(context.Background(), "lampyr-fiber-router")
			if err != nil {
				t.Fatal(err)
			}
			value, ok := res.Set().Value(semconv.ServiceNameKey)
			if !ok {
				t.Fatal("tracing resource has no service.name")
			}
			if got := value.AsString(); got != test.want {
				t.Fatalf("service.name = %q, want %q", got, test.want)
			}
		})
	}
}
