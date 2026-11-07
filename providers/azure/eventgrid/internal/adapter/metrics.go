// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"fmt"
	"net/http"
	"sync/atomic"
)

type Metrics struct {
	receiveSuccess        atomic.Uint64
	receiveFailure        atomic.Uint64
	sinkSuccess           atomic.Uint64
	sinkFailure           atomic.Uint64
	activeDeliveries      atomic.Int64
	configuredSources     atomic.Int64
	configurationErrors   atomic.Uint64
	deliveryLatencyMillis atomic.Uint64
	deliveryLatencyCount  atomic.Uint64
}

func NewMetrics() *Metrics {
	return &Metrics{}
}

func (metrics *Metrics) ServeHTTP(response http.ResponseWriter, _ *http.Request) {
	response.Header().Set("Content-Type", "text/plain; version=0.0.4")
	writeCounter(response, "lampyr_eventgrid_receive_total", "success", metrics.receiveSuccess.Load())
	writeCounter(response, "lampyr_eventgrid_receive_total", "failure", metrics.receiveFailure.Load())
	writeCounter(response, "lampyr_eventgrid_sink_delivery_total", "success", metrics.sinkSuccess.Load())
	writeCounter(response, "lampyr_eventgrid_sink_delivery_total", "failure", metrics.sinkFailure.Load())
	fmt.Fprintf(response, "lampyr_eventgrid_active_deliveries %d\n", metrics.activeDeliveries.Load())
	fmt.Fprintf(response, "lampyr_eventgrid_configured_sources %d\n", metrics.configuredSources.Load())
	fmt.Fprintf(response, "lampyr_eventgrid_configuration_errors_total %d\n", metrics.configurationErrors.Load())
	fmt.Fprintf(
		response,
		"lampyr_eventgrid_delivery_latency_seconds_sum %.3f\n",
		float64(metrics.deliveryLatencyMillis.Load())/1000,
	)
	fmt.Fprintf(
		response,
		"lampyr_eventgrid_delivery_latency_seconds_count %d\n",
		metrics.deliveryLatencyCount.Load(),
	)
}

func writeCounter(response http.ResponseWriter, name, outcome string, value uint64) {
	fmt.Fprintf(response, "%s{outcome=%q} %d\n", name, outcome, value)
}
