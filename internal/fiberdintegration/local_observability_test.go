// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
	"encoding/csv"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/hyperlight-dev/lampyr/internal/project"
	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/propagation"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"
	"go.opentelemetry.io/otel/trace"
)

func TestLocalObservabilityPreservesIncomingTraceAcrossBinding(t *testing.T) {
	previous, propagator := otel.GetTracerProvider(), otel.GetTextMapPropagator()
	spans := tracetest.NewSpanRecorder()
	provider := sdktrace.NewTracerProvider(sdktrace.WithSpanProcessor(spans))
	otel.SetTracerProvider(provider)
	otel.SetTextMapPropagator(propagation.TraceContext{})
	t.Cleanup(func() {
		otel.SetTracerProvider(previous)
		otel.SetTextMapPropagator(propagator)
		_ = provider.Shutdown(context.Background())
	})
	backend, backClient, backRoute := routerFixture(t, project.ExecutionFresh)
	backClient.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		fmt.Fprint(w, request.Header.Get("Traceparent"))
	})
	binding := httptest.NewServer(localHandler(backend, backRoute.Revision, "/_lampyr/private"))
	defer binding.Close()
	frontend, frontClient, frontRoute := routerFixture(t, project.ExecutionFresh)
	frontClient.handler = http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		call, err := http.NewRequestWithContext(request.Context(), http.MethodGet, binding.URL+"/_lampyr/private/", nil)
		if err != nil {
			t.Error(err)
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		call.Header.Set("Traceparent", request.Header.Get("Traceparent"))
		response, err := http.DefaultClient.Do(call)
		if err != nil {
			t.Error(err)
			w.WriteHeader(http.StatusBadGateway)
			return
		}
		defer response.Body.Close()
		w.WriteHeader(response.StatusCode)
		_, _ = io.Copy(w, response.Body)
	})
	handler := localIngressHandler(frontend, map[string]string{"app.default.localhost": frontRoute.Revision},
		"8080", &localObservability{})
	request := httptest.NewRequest(http.MethodGet, "http://app.default.localhost:8080/", nil)
	const incomingID = "0123456789abcdef0123456789abcdef"
	request.Header.Set("Traceparent", "00-"+incomingID+"-0123456789abcdef-01")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK || !strings.Contains(response.Body.String(), incomingID) {
		t.Errorf("binding lost incoming trace: status=%d body=%s", response.Code, response.Body.String())
	}
	acquisitions, servers := 0, 0
	for _, span := range spans.Ended() {
		if span.SpanContext().TraceID().String() != incomingID {
			t.Errorf("%s started a separate trace: %s", span.Name(), span.SpanContext().TraceID())
		}
		if span.Name() == "lampyr.fiber.acquire" {
			acquisitions++
		}
		if span.SpanKind() == trace.SpanKindServer {
			servers++
		}
	}
	if acquisitions != 2 || servers != 2 {
		t.Errorf("missing ingress or binding spans: acquisitions=%d servers=%d", acquisitions, servers)
	}
}

func TestLocalObservabilityMountSourceUsesDockerCSV(t *testing.T) {
	for _, directory := range []string{"/work/team,inc", `/work/team"quoted`, `/work/team,"quoted"`} {
		t.Run(directory, func(t *testing.T) {
			session := &localObservability{
				directory: directory,
				docker: func(_ context.Context, args ...string) (string, error) {
					if args[0] == "create" {
						mount := args[slices.Index(args, "--mount")+1]
						fields, err := csv.NewReader(strings.NewReader(mount)).Read()
						if err != nil {
							return "", fmt.Errorf("Docker mount CSV: %w", err)
						}
						want := "source=" + filepath.Join(directory, "jaeger.yaml")
						if len(fields) != 4 || !slices.Contains(fields, want) {
							return "", fmt.Errorf("Docker mount source split: %q, want %q", fields, want)
						}
					}
					return "127.0.0.1:16686", nil
				},
			}
			if _, err := session.container(context.Background(), "jaeger", localJaegerImage,
				"jaeger.yaml", []string{"16686"}); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestLocalObservabilityRotationDoesNotInheritDaemonDriver(t *testing.T) {
	for _, kind := range []string{"jaeger", "prometheus"} {
		t.Run(kind, func(t *testing.T) {
			session := &localObservability{
				directory: "/workspace",
				docker: func(_ context.Context, args ...string) (string, error) {
					if args[0] == "create" {
						driver := "journald"
						for index, arg := range args {
							if arg == "--log-driver" && index+1 < len(args) {
								driver = args[index+1]
							} else if value, ok := strings.CutPrefix(arg, "--log-driver="); ok {
								driver = value
							}
						}
						if driver != "json-file" && driver != "local" {
							return "", fmt.Errorf("logging driver %s does not support max-size/max-file", driver)
						}
					}
					return "127.0.0.1:19090", nil
				},
			}
			if _, err := session.container(context.Background(), kind, "test-image", "config", []string{"9090"}); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestLocalObservabilityPrometheusCompactionFitsTmpfs(t *testing.T) {
	ready := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))
	defer ready.Close()
	docker := func(_ context.Context, args ...string) (string, error) {
		if args[0] == "create" && slices.Contains(args, localPrometheusImage) {
			// Prometheus preallocates a 512 MiB block chunk segment by default.
			segmentBytes := int64(512 * 1024 * 1024)
			for _, arg := range args {
				if value, ok := strings.CutPrefix(arg, "--storage.tsdb.max-block-chunk-segment-size="); ok {
					var err error
					segmentBytes, err = strconv.ParseInt(strings.TrimSuffix(value, "MB"), 10, 64)
					if err != nil {
						return "", err
					}
					segmentBytes *= 1000 * 1000
				}
			}
			if segmentBytes > 128*1024*1024-96*1000*1000 {
				return "", fmt.Errorf("compaction segment preallocation %d exceeds tmpfs space after retained data", segmentBytes)
			}
		}
		return strings.TrimPrefix(ready.URL, "http://"), nil
	}
	session, err := startLocalObservability(context.Background(), t.TempDir(), docker)
	if err != nil {
		t.Fatal(err)
	}
	if err := session.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestLocalObservabilityLinksStayInServiceIndex(t *testing.T) {
	router, client, route := routerFixture(t, project.ExecutionFresh)
	requests := 0
	for _, enabled := range []bool{false, true} {
		var telemetry *localObservability
		if enabled {
			telemetry = &localObservability{Prometheus: "http://127.0.0.1:19090", Jaeger: "http://127.0.0.1:16686"}
		}
		handler := localIngressHandler(router, map[string]string{"app.default.localhost": route.Revision}, "8080", telemetry)
		for _, test := range []struct {
			url    string
			status int
		}{
			{"http://localhost:8080/", 200},
			{"http://127.0.0.1:8080/", 200},
			{"http://localhost:8080/metrics", 404},
			{"http://unknown.default.localhost:8080/", 404},
		} {
			response := httptest.NewRecorder()
			handler.ServeHTTP(response, httptest.NewRequest("GET", test.url, nil))
			body := response.Body.String()
			if response.Code != test.status || !strings.Contains(body, "http://app.default.localhost:8080/") {
				t.Fatalf("enabled=%v URL=%s status=%d body=%s", enabled, test.url, response.Code, body)
			}
			for _, link := range []string{"http://127.0.0.1:19090/query", "http://127.0.0.1:16686/search?service=lampyr-local"} {
				if strings.Contains(body, link) != enabled {
					t.Fatalf("enabled=%v, unexpected observability link %s: %s", enabled, link, body)
				}
			}
		}
		if client.next != requests {
			t.Fatal("index or unknown path acquired a fiber")
		}
		response := httptest.NewRecorder()
		handler.ServeHTTP(response, httptest.NewRequest("GET", "http://app.default.localhost:8080/metrics", nil))
		if response.Code != 200 || strings.Contains(response.Body.String(), "Observability") {
			t.Fatalf("application /metrics was intercepted: %d %s", response.Code, response.Body.String())
		}
		requests++
	}
}

func TestLocalScrapeListenerRequiresSessionToken(t *testing.T) {
	handler := localScrapeHandler("session-secret")
	for _, test := range []struct {
		path   string
		auth   string
		status int
	}{
		{"/metrics", "", 401},
		{"/metrics", "Bearer wrong", 401},
		{"/metrics", "Bearer session-secret", 200},
		{"/", "Bearer session-secret", 404},
	} {
		request := httptest.NewRequest("GET", test.path, nil)
		request.Header.Set("Authorization", test.auth)
		response := httptest.NewRecorder()
		handler.ServeHTTP(response, request)
		if response.Code != test.status {
			t.Fatalf("%s %q = %d", test.path, test.auth, response.Code)
		}
		if test.status == 200 && (!strings.Contains(response.Body.String(), "go_goroutines") ||
			strings.Contains(response.Header().Get("Content-Type"), "text/html")) {
			t.Fatalf("scrape did not return Prometheus exposition: %s", response.Body.String())
		}
	}
}

func TestLocalObservabilityOwnsOnlyItsContainersAndCleansStartupFailures(t *testing.T) {
	ready := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
	}))
	defer ready.Close()
	address := strings.TrimPrefix(ready.URL, "http://")
	for _, fail := range []string{"", "create", "start", "port"} {
		t.Run("fail-"+fail, func(t *testing.T) {
			parent := t.TempDir()
			var created, removed []string
			var scrapeAddress string
			previous := otel.GetTracerProvider()
			docker := func(_ context.Context, args ...string) (string, error) {
				if args[0] == "create" {
					configs, err := filepath.Glob(filepath.Join(parent, "observability-*", "*"))
					if err != nil {
						t.Fatal(err)
					}
					for _, config := range configs {
						info, err := os.Stat(config)
						if err != nil || info.Mode().Perm()&0o004 == 0 {
							t.Fatalf("container user cannot read configuration %s: %v", config, err)
						}
					}
					name := args[slices.Index(args, "--name")+1]
					created = append(created, name)
					for index, arg := range args {
						if arg == "--publish" && !strings.HasPrefix(args[index+1], "127.0.0.1::") {
							t.Fatalf("public container port: %v", args)
						}
					}
					if !slices.Contains(args, "--read-only") || !slices.Contains(args, "--cap-drop=ALL") {
						t.Fatalf("missing container restrictions: %v", args)
					}
					if strings.Contains(name, "-prometheus-") {
						configs, err := filepath.Glob(filepath.Join(parent, "observability-*", "prometheus.yml"))
						if err != nil || len(configs) != 1 {
							t.Fatalf("missing Prometheus config: %v %v", configs, err)
						}
						config, err := os.ReadFile(configs[0])
						if err != nil {
							t.Fatal(err)
						}
						for _, line := range strings.Split(string(config), "\n") {
							if strings.Contains(line, "host.docker.internal:") {
								port := strings.Split(strings.Split(line, ":")[1], "\"")[0]
								scrapeAddress = net.JoinHostPort("127.0.0.1", port)
							}
						}
					}
				}
				if args[0] == fail {
					return "", errors.New("injected " + fail + " failure")
				}
				if args[0] == "rm" {
					if len(args) != 3 || args[1] != "--force" || !slices.Contains(created, args[2]) {
						t.Fatalf("cleanup touched an unowned container: %v", args)
					}
					removed = append(removed, args[2])
				}
				return address, nil
			}
			session, err := startLocalObservability(context.Background(), parent, docker)
			if fail == "" {
				if err != nil {
					t.Fatal(err)
				}
				if session.Prometheus != ready.URL || session.Jaeger != ready.URL || otel.GetTracerProvider() == previous {
					t.Fatal("local endpoints or tracer were not configured")
				}
				if err := session.Close(); err != nil {
					t.Fatal(err)
				}
				if otel.GetTracerProvider() != previous {
					t.Fatal("tracer provider was not restored")
				}
			} else if err == nil || !strings.Contains(err.Error(), "injected "+fail) {
				t.Fatalf("missing startup failure: %v", err)
			}
			if len(created) == 0 || len(removed) != len(created) {
				t.Fatalf("created=%v removed=%v", created, removed)
			}
			files, err := os.ReadDir(parent)
			if err != nil || len(files) != 0 {
				t.Fatalf("startup left configuration behind: %v %v", files, err)
			}
			if scrapeAddress != "" {
				connection, err := net.Dial("tcp", scrapeAddress)
				if err == nil {
					connection.Close()
					t.Fatal("cleanup left the scrape listener running")
				}
			}
		})
	}
}

func TestLocalObservabilityContainerConfigsReadableWithPrivateUmask(t *testing.T) {
	if os.Getenv("LAMPYR_TEST_PRIVATE_UMASK") == "1" {
		TestLocalObservabilityOwnsOnlyItsContainersAndCleansStartupFailures(t)
		return
	}
	command := exec.Command("/bin/sh", "-c", `umask 077; exec "$@"`, "sh",
		os.Args[0], "-test.run=^TestLocalObservabilityContainerConfigsReadableWithPrivateUmask$")
	command.Env = append(os.Environ(), "LAMPYR_TEST_PRIVATE_UMASK=1")
	if output, err := command.CombinedOutput(); err != nil {
		t.Fatalf("restrictive umask: %v\n%s", err, output)
	}
}

type stalledLocalTraceExporter struct{}

func (stalledLocalTraceExporter) ExportSpans(context.Context, []sdktrace.ReadOnlySpan) error {
	return nil
}

func (stalledLocalTraceExporter) Shutdown(ctx context.Context) error {
	<-ctx.Done()
	return ctx.Err()
}

func TestLocalObservabilityTraceFlushTimeoutStillRemovesContainers(t *testing.T) {
	var removed []string
	session := &localObservability{
		directory: t.TempDir(), containers: []string{"owned-jaeger", "owned-prometheus"},
		tracer:   sdktrace.NewTracerProvider(sdktrace.WithSyncer(stalledLocalTraceExporter{})),
		previous: otel.GetTracerProvider(), propagator: otel.GetTextMapPropagator(),
		docker: func(ctx context.Context, args ...string) (string, error) {
			if err := ctx.Err(); err != nil {
				return "", err
			}
			removed = append(removed, args[2])
			return "", nil
		},
	}
	started := time.Now()
	if err := session.Close(); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("trace flush error was lost: %v", err)
	}
	if len(removed) != 2 {
		t.Fatalf("trace flush timeout prevented container cleanup: %v (after %s)", removed, time.Since(started))
	}
}

func TestLocalObservabilityReadinessRespectsCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if err := localWaitHTTP(ctx, "http://127.0.0.1:1/status"); !errors.Is(err, context.Canceled) {
		t.Fatalf("readiness did not stop with the session: %v", err)
	}
}

func TestLocalObservabilityKeepsDockerDesktopHostDNS(t *testing.T) {
	for _, platform := range []string{"linux", "darwin"} {
		t.Run(platform, func(t *testing.T) {
			var createArgs []string
			session := &localObservability{
				directory: t.TempDir(), platform: platform,
				docker: func(_ context.Context, args ...string) (string, error) {
					if args[0] == "create" {
						createArgs = args
					}
					return "127.0.0.1:19090", nil
				},
			}
			if _, err := session.container(context.Background(), "prometheus", localPrometheusImage,
				"prometheus.yml", []string{"9090"}); err != nil {
				t.Fatal(err)
			}
			hostGateway := slices.Contains(createArgs, "--add-host=host.docker.internal:host-gateway")
			if hostGateway != (platform == "linux") {
				t.Fatalf("%s host routing overrides Docker Desktop's host DNS: %v", platform, createArgs)
			}
		})
	}
}
