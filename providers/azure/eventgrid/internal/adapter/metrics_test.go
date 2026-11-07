// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"net/http/httptest"
	"strings"
	"testing"
)

func TestMetricsExposeOperationalCountersWithoutPayloadData(t *testing.T) {
	metrics := NewMetrics()
	metrics.receiveSuccess.Add(2)
	metrics.sinkFailure.Add(1)
	metrics.activeDeliveries.Store(4)
	metrics.configuredSources.Store(5)
	metrics.deliveryLatencyMillis.Store(9250)
	metrics.deliveryLatencyCount.Store(2)

	response := httptest.NewRecorder()
	metrics.ServeHTTP(response, httptest.NewRequest("GET", "/metrics", nil))
	body := response.Body.String()
	for _, expected := range []string{
		`lampyr_eventgrid_receive_total{outcome="success"} 2`,
		`lampyr_eventgrid_sink_delivery_total{outcome="failure"} 1`,
		"lampyr_eventgrid_active_deliveries 4",
		"lampyr_eventgrid_configured_sources 5",
		"lampyr_eventgrid_delivery_latency_seconds_sum 9.250",
		"lampyr_eventgrid_delivery_latency_seconds_count 2",
	} {
		if !strings.Contains(body, expected) {
			t.Fatalf("metrics body does not contain %q:\n%s", expected, body)
		}
	}
	for _, forbidden := range []string{"lockToken", "access_token", "specversion"} {
		if strings.Contains(body, forbidden) {
			t.Fatalf("metrics body contains sensitive or payload field %q", forbidden)
		}
	}
}
