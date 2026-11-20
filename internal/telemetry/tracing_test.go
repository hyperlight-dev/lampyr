// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package telemetry

import "testing"

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
