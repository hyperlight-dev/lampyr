// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"bufio"
	"context"
	"errors"
	"net"
	"net/http"
	"strconv"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

// Router metrics describe what Lampyr does differently from a container
// platform: how many Fibers are serving requests, how long it takes to get
// one, and how requests end. Labels come only from Revisions the router has
// a route for, and a Revision's series are deleted when its route goes.
var (
	registry = prometheus.NewRegistry()

	fibersBusy = prometheus.NewGaugeVec(prometheus.GaugeOpts{
		Name: "lampyr_router_fibers_busy",
		Help: "Fibers serving a request right now, by Revision. Idle pooled reuse Fibers are not counted.",
	}, []string{"namespace", "revision"})

	fiberAcquireSeconds = prometheus.NewHistogramVec(prometheus.HistogramOpts{
		Name:    "lampyr_router_fiber_acquire_seconds",
		Help:    "Time to clone or attach a Fiber before proxying, by Revision and kind (error, or canceled when the client went away, if none was acquired).",
		Buckets: []float64{.005, .01, .025, .05, .075, .1, .125, .15, .2, .25, .3, .4, .5, .75, 1, 2, 3, 5, 10, 30},
	}, []string{"namespace", "revision", "kind"})

	requestSeconds = prometheus.NewHistogramVec(prometheus.HistogramOpts{
		Name:    "lampyr_router_request_duration_seconds",
		Help:    "Request duration through the router, including Fiber acquisition. WebSocket sessions (101) are not observed; 499 means the client went away.",
		Buckets: []float64{.01, .025, .05, .1, .15, .2, .3, .5, .75, 1, 1.5, 2, 2.25, 2.5, 3, 4, 5, 10, 30},
	}, []string{"namespace", "revision", "code"})

	shedTotal = prometheus.NewCounterVec(prometheus.CounterOpts{
		Name: "lampyr_router_shed_total",
		Help: "Requests the Revision's home refused: at fibers.max, too busy restoring to start a Fiber within the request's deadline, or under memory or task-limit (pids) pressure, or still over its clone rate limit at the acquire deadline.",
	}, []string{"namespace", "revision"})

	cloneRetriesTotal = prometheus.NewCounterVec(prometheus.CounterOpts{
		Name: "lampyr_router_clone_retries_total",
		Help: "Clones the router tried again within the request's acquire deadline, by reason: rate_limit after waiting out the home's clone rate limit, restores_busy after waiting for the home's helper to restore fewer Fibers, home_unreachable after asking for the grant's homes again when no home received the Clone. A request that still gets no Fiber is counted as it fails, as a shed or an error.",
	}, []string{"namespace", "revision", "reason"})
)

func init() {
	registry.MustRegister(fibersBusy, fiberAcquireSeconds, requestSeconds, shedTotal, cloneRetriesTotal,
		prometheus.NewGoCollector(), prometheus.NewProcessCollector(prometheus.ProcessCollectorOpts{}))
}

// MetricsHandler serves router metrics. It belongs on its own listener so a
// Fiber's application paths never collide with it. The registry is
// process-wide: one fiber-router process runs one Router.
func MetricsHandler() http.Handler {
	return promhttp.HandlerFor(registry, promhttp.HandlerOpts{})
}

// statusRecorder keeps the response code for metrics. Unwrap lets
// http.ResponseController reach the underlying writer, so the reverse proxy
// can still flush streams and hijack WebSocket upgrades.
type statusRecorder struct {
	http.ResponseWriter
	code int
	// failed is set when the router saw the request fail: no Fiber, a
	// proxy error, or a response copy that broke after the headers.
	failed bool
}

func (recorder *statusRecorder) WriteHeader(code int) {
	// Informational responses (100 Continue, 103 Early Hints) precede the
	// real status, except 101, which ends HTTP on the connection.
	if recorder.code == 0 && (code >= 200 || code == http.StatusSwitchingProtocols) {
		recorder.code = code
	}
	recorder.ResponseWriter.WriteHeader(code)
}

func (recorder *statusRecorder) Write(data []byte) (int, error) {
	if recorder.code == 0 {
		recorder.code = http.StatusOK
	}
	return recorder.ResponseWriter.Write(data)
}

func (recorder *statusRecorder) Unwrap() http.ResponseWriter { return recorder.ResponseWriter }

// Hijack marks the request as a protocol switch (WebSocket), which is not
// observed as a request: the session's length isn't request latency.
func (recorder *statusRecorder) Hijack() (net.Conn, *bufio.ReadWriter, error) {
	connection, buffer, err := http.NewResponseController(recorder.ResponseWriter).Hijack()
	// Only a hijack that succeeded becomes an upgrade; on failure (an HTTP/2
	// connection, for one) the proxy's error response is recorded instead.
	if err == nil && recorder.code == 0 {
		recorder.code = http.StatusSwitchingProtocols
	}
	return connection, buffer, err
}

// statusClientClosed marks requests whose client went away, as nginx does.
const statusClientClosed = 499

// requestStatus is the status to record. A request that completed keeps its
// status, even when the client closes its connection right after. Only a
// failure the router saw, or a request that never got a status, becomes
// 499 (client went away) or 504 (Revision timeout); a response copy that
// broke otherwise is a 502.
func requestStatus(recorder *statusRecorder, client, revision context.Context) int {
	if recorder.code == http.StatusSwitchingProtocols {
		return recorder.code
	}
	if !recorder.failed && recorder.code != 0 {
		return recorder.code
	}
	switch {
	case client.Err() != nil:
		return statusClientClosed
	case revision != nil && errors.Is(revision.Err(), context.DeadlineExceeded):
		return http.StatusGatewayTimeout
	case recorder.code == 0 || recorder.code < 400:
		return http.StatusBadGateway
	default:
		return recorder.code
	}
}

// statusLabel keeps the code label bounded: the status comes from the
// tenant's application, so codes HTTP doesn't define become "other".
func statusLabel(code int) string {
	if code == statusClientClosed || http.StatusText(code) != "" {
		return strconv.Itoa(code)
	}
	return "other"
}

func observeRequest(namespace, revision string, code int, start time.Time) {
	// A hijacked WebSocket connection never writes a status through us, and
	// a session's length says nothing about request latency.
	if code == 0 || code == http.StatusSwitchingProtocols {
		return
	}
	requestSeconds.WithLabelValues(namespace, revision, statusLabel(code)).Observe(time.Since(start).Seconds())
}

// revisionMetrics records a Revision's series only while the route they
// were taken for still exists. A request still in flight when its Revision
// is removed would otherwise recreate series that nothing deletes again.
type revisionMetrics struct {
	router     *Router
	key        string
	generation uint64
}

func (router *Router) metricsFor(namespace, revision string) revisionMetrics {
	key := namespace + "/" + revision
	router.mu.RLock()
	defer router.mu.RUnlock()
	return revisionMetrics{router: router, key: key, generation: router.generations[key]}
}

// record runs observe under the router's read lock, which Reload's
// forgetRevision cannot interleave with.
func (metrics revisionMetrics) record(observe func()) {
	metrics.router.mu.RLock()
	defer metrics.router.mu.RUnlock()
	if metrics.generation != 0 && metrics.router.generations[metrics.key] == metrics.generation {
		observe()
	}
}

// forgetRevision drops a removed Revision's series, so label cardinality
// follows the routes the router has, not every Revision it ever served.
func forgetRevision(namespace, revision string) {
	labels := prometheus.Labels{"namespace": namespace, "revision": revision}
	fibersBusy.DeletePartialMatch(labels)
	fiberAcquireSeconds.DeletePartialMatch(labels)
	requestSeconds.DeletePartialMatch(labels)
	shedTotal.DeletePartialMatch(labels)
	cloneRetriesTotal.DeletePartialMatch(labels)
}
