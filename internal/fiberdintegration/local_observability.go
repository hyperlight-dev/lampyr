// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
	"crypto/rand"
	"crypto/subtle"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"time"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/exporters/otlp/otlptrace/otlptracegrpc"
	"go.opentelemetry.io/otel/propagation"
	"go.opentelemetry.io/otel/sdk/resource"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	semconv "go.opentelemetry.io/otel/semconv/v1.37.0"
	"go.opentelemetry.io/otel/trace"
)

const localPrometheusImage = "prom/prometheus:v3.16.0@sha256:e04987641b57704f408e31f6373ba4138ecf27cb4a165d24aabb6c38fddd0f06"
const localJaegerImage = "jaegertracing/jaeger:2.23.0@sha256:3823829974ccfc2485d2e48b165579091f32295022ae80ca49704e2f4834116e"

type localObservability struct {
	Prometheus string
	Jaeger     string
	directory  string
	containers []string
	docker     func(context.Context, ...string) (string, error)
	metrics    *http.Server
	tracer     *sdktrace.TracerProvider
	previous   trace.TracerProvider
	propagator propagation.TextMapPropagator
	platform   string
}

func localDockerCommand(ctx context.Context, args ...string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 5*time.Minute)
	defer cancel()
	output, err := exec.CommandContext(ctx, "docker", args...).CombinedOutput()
	if err != nil {
		return "", fmt.Errorf("docker %s: %w: %s", args[0], err, strings.TrimSpace(string(output)))
	}
	return strings.TrimSpace(string(output)), nil
}

func startLocalObservability(ctx context.Context, parent string, docker func(context.Context, ...string) (string, error)) (_ *localObservability, returnErr error) {
	session := &localObservability{docker: docker, platform: runtime.GOOS}
	defer func() {
		if returnErr != nil {
			returnErr = errors.Join(returnErr, session.Close())
		}
	}()
	directory, err := os.MkdirTemp(parent, "observability-")
	if err != nil {
		return nil, err
	}
	session.directory, err = filepath.Abs(directory)
	if err != nil {
		return nil, err
	}
	// Docker Desktop reaches the host through host.docker.internal; Linux gets
	// the same name through host-gateway. This listener never serves app routes.
	listener, err := net.Listen("tcp4", "0.0.0.0:0")
	if err != nil {
		return nil, err
	}
	token := rand.Text()
	session.metrics = &http.Server{
		Handler: localScrapeHandler(token), ReadHeaderTimeout: 5 * time.Second,
	}
	go func() { _ = session.metrics.Serve(listener) }()
	metricsPort := listener.Addr().(*net.TCPAddr).Port
	prometheusConfig := fmt.Sprintf(`global:
  scrape_interval: 1s
scrape_configs:
  - job_name: lampyr-local
    authorization:
      credentials: %s
    static_configs:
      - targets: ["host.docker.internal:%d"]
`, token, metricsPort)
	for name, contents := range map[string]string{"prometheus.yml": prometheusConfig, "jaeger.yaml": localJaegerConfig} {
		path := filepath.Join(session.directory, name)
		if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
			return nil, err
		}
		if err := os.Chmod(path, 0o644); err != nil {
			return nil, err
		}
	}
	jaeger, err := session.container(ctx, "jaeger", localJaegerImage, "jaeger.yaml",
		[]string{"16686", "4317", "13133"}, "--config=/etc/lampyr/config")
	if err != nil {
		return nil, err
	}
	session.Jaeger = "http://" + jaeger["16686"]
	prometheus, err := session.container(ctx, "prometheus", localPrometheusImage, "prometheus.yml",
		[]string{"9090"}, "--config.file=/etc/lampyr/config", "--storage.tsdb.path=/prometheus",
		"--storage.tsdb.retention.time=1h", "--storage.tsdb.retention.size=96MB")
	if err != nil {
		return nil, err
	}
	session.Prometheus = "http://" + prometheus["9090"]
	for _, endpoint := range []string{"http://" + jaeger["13133"] + "/status", session.Prometheus + "/-/ready"} {
		if err := localWaitHTTP(ctx, endpoint); err != nil {
			return nil, err
		}
	}
	exporter, err := otlptracegrpc.New(ctx,
		otlptracegrpc.WithEndpoint(jaeger["4317"]), otlptracegrpc.WithInsecure())
	if err != nil {
		return nil, err
	}
	session.tracer = sdktrace.NewTracerProvider(
		sdktrace.WithBatcher(exporter),
		sdktrace.WithResource(resource.NewWithAttributes(semconv.SchemaURL, semconv.ServiceName("lampyr-local"))),
	)
	session.previous = otel.GetTracerProvider()
	session.propagator = otel.GetTextMapPropagator()
	otel.SetTracerProvider(session.tracer)
	otel.SetTextMapPropagator(propagation.NewCompositeTextMapPropagator(propagation.TraceContext{}, propagation.Baggage{}))
	return session, nil
}

func (session *localObservability) container(ctx context.Context, kind, image, config string, ports []string, command ...string) (map[string]string, error) {
	name := "lampyr-dev-" + kind + "-" + strings.ToLower(rand.Text())
	session.containers = append(session.containers, name)
	args := []string{"create", "--name", name, "--pull=missing",
		"--label", "lampyr.dev/local-observability=true",
		"--read-only", "--cap-drop=ALL", "--security-opt=no-new-privileges",
		"--user=10001:10001", "--memory=512m", "--pids-limit=128",
		"--log-opt=max-size=1m", "--log-opt=max-file=1",
		"--mount", "type=bind,source=" + filepath.Join(session.directory, config) + ",target=/etc/lampyr/config,readonly"}
	for _, port := range ports {
		args = append(args, "--publish", "127.0.0.1::"+port)
	}
	if kind == "prometheus" {
		if session.platform == "linux" {
			args = append(args, "--add-host=host.docker.internal:host-gateway")
		}
		args = append(args, "--tmpfs=/prometheus:rw,noexec,nosuid,size=128m,uid=10001,gid=10001")
	}
	args = append(args, image)
	args = append(args, command...)
	if _, err := session.docker(ctx, args...); err != nil {
		return nil, err
	}
	if _, err := session.docker(ctx, "start", name); err != nil {
		return nil, err
	}
	endpoints := map[string]string{}
	for _, port := range ports {
		address, err := session.docker(ctx, "port", name, port+"/tcp")
		if err != nil {
			return nil, err
		}
		host, number, err := net.SplitHostPort(address)
		value, parseErr := strconv.ParseUint(number, 10, 16)
		if err != nil || parseErr != nil || value == 0 || host != "127.0.0.1" {
			return nil, fmt.Errorf("docker returned a non-loopback local %s endpoint", kind)
		}
		endpoints[port] = address
	}
	return endpoints, nil
}

func localScrapeHandler(token string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		if subtle.ConstantTimeCompare([]byte(request.Header.Get("Authorization")), []byte("Bearer "+token)) != 1 {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		if request.URL.Path != "/metrics" {
			http.NotFound(w, request)
			return
		}
		MetricsHandler().ServeHTTP(w, request)
	})
}

func localWaitHTTP(ctx context.Context, endpoint string) error {
	ctx, cancel := context.WithTimeout(ctx, 45*time.Second)
	defer cancel()
	client := &http.Client{Timeout: time.Second}
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()
	for {
		request, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
		if err != nil {
			return err
		}
		response, err := client.Do(request)
		if err == nil {
			response.Body.Close()
			if response.StatusCode == http.StatusOK {
				return nil
			}
		}
		select {
		case <-ctx.Done():
			return fmt.Errorf("local observability did not become ready: %w", ctx.Err())
		case <-ticker.C:
		}
	}
}

func (session *localObservability) Close() error {
	var result error
	// Flush after requests drain and before stopping the trace backend.
	if session.tracer != nil {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		result = errors.Join(result, session.tracer.Shutdown(ctx))
		cancel()
		if otel.GetTracerProvider() == session.tracer {
			otel.SetTracerProvider(session.previous)
			otel.SetTextMapPropagator(session.propagator)
		}
	}
	if session.metrics != nil {
		result = errors.Join(result, session.metrics.Close())
	}
	for index := len(session.containers) - 1; index >= 0; index-- {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_, err := session.docker(ctx, "rm", "--force", session.containers[index])
		cancel()
		if err != nil && !strings.Contains(err.Error(), "No such container") {
			result = errors.Join(result, err)
		}
	}
	return errors.Join(result, os.RemoveAll(session.directory))
}

const localJaegerConfig = `extensions:
  healthcheckv2:
    use_v2: true
    http:
      endpoint: 0.0.0.0:13133
  jaeger_query:
    storage:
      traces: memory
  jaeger_storage:
    backends:
      memory:
        memory:
          max_traces: 10000
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
processors:
  batch: {}
exporters:
  jaeger_storage_exporter:
    trace_storage: memory
service:
  extensions: [healthcheckv2, jaeger_query, jaeger_storage]
  pipelines:
    traces:
      receivers: [otlp]
      processors: [batch]
      exporters: [jaeger_storage_exporter]
`
