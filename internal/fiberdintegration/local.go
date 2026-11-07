// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fiberdintegration

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"errors"
	"fmt"
	"html/template"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	consumer "github.com/Azure/fiberd/sdk/go/client"
	"github.com/hyperlight-dev/lampyr/internal/project"
)

type LocalService struct {
	Name      string                  `json:"name"`
	Snapshot  string                  `json:"snapshot"`
	Command   string                  `json:"command"`
	Execution project.ExecutionPolicy `json:"execution"`
	Bindings  map[string]string       `json:"bindings"`
}

type LocalConfig struct {
	SchemaVersion   int            `json:"schemaVersion"`
	Binary          string         `json:"binary"`
	Listen          string         `json:"listen"`
	ExposedServices []string       `json:"exposedServices"`
	Services        []LocalService `json:"services"`
	Metrics         bool           `json:"metrics"`
}

type localMetrics struct {
	mu                               sync.Mutex
	output                           io.Writer
	requests, errors, create, attach int
	duration, clone                  time.Duration
}

func newLocalMetrics(output io.Writer) *localMetrics {
	return &localMetrics{output: output}
}

func (metrics *localMetrics) Observe(metric RequestMetric) {
	metrics.mu.Lock()
	defer metrics.mu.Unlock()
	metrics.requests++
	if metric.Status >= 400 {
		metrics.errors++
	}
	switch metric.FiberKind {
	case "CREATE":
		metrics.create++
	case "ATTACH":
		metrics.attach++
	}
	metrics.duration += metric.Duration
	metrics.clone += metric.CloneDuration
	fmt.Fprintf(
		metrics.output,
		"metric service=%s method=%s path=%q status=%d duration_ms=%d clone_ms=%d fiber_id=%q fiber=%s\n",
		metric.Service, metric.Method, metric.Path, metric.Status,
		metric.Duration.Milliseconds(), metric.CloneDuration.Milliseconds(),
		metric.FiberID, metric.FiberKind,
	)
}

func (metrics *localMetrics) Summary() {
	metrics.mu.Lock()
	defer metrics.mu.Unlock()
	var duration, clone int64
	if metrics.requests > 0 {
		duration = metrics.duration.Milliseconds() / int64(metrics.requests)
		clone = metrics.clone.Milliseconds() / int64(metrics.requests)
	}
	fmt.Fprintf(
		metrics.output,
		"metrics summary requests=%d errors=%d create=%d attach=%d avg_duration_ms=%d avg_clone_ms=%d\n",
		metrics.requests, metrics.errors, metrics.create, metrics.attach, duration, clone,
	)
}

type localFiber struct {
	value     consumer.Fiber
	session   string
	cmd       *exec.Cmd
	done      chan struct{}
	ready     chan struct{}
	startErr  error
	isReady   bool
	start     atomic.Uint32
	readyOnce sync.Once
	doneOnce  sync.Once
	stopping  atomic.Bool
}

const (
	localFiberStarting uint32 = iota
	localFiberReady
	localFiberExited
)

type localHome struct {
	mu       sync.Mutex
	binary   string
	service  LocalService
	bindings map[string]string
	fibers   map[string]*localFiber
	sequence uint64
	closed   bool
	hostIP   string
}

type localHomes map[string]*localHome

var localPorts = struct {
	sync.Mutex
	claimed map[int]bool
}{claimed: map[int]bool{}}

func reserveLocalPort() (string, string, func(), error) {
	for {
		listener, err := net.Listen("tcp4", "127.0.0.1:0")
		if err != nil {
			return "", "", nil, err
		}
		address := listener.Addr().String()
		port := listener.Addr().(*net.TCPAddr).Port
		localPorts.Lock()
		if !localPorts.claimed[port] {
			localPorts.claimed[port] = true
			localPorts.Unlock()
			if err := listener.Close(); err != nil {
				localPorts.Lock()
				delete(localPorts.claimed, port)
				localPorts.Unlock()
				return "", "", nil, err
			}
			return address, strconv.Itoa(port), func() {
				localPorts.Lock()
				delete(localPorts.claimed, port)
				localPorts.Unlock()
			}, nil
		}
		localPorts.Unlock()
		if err := listener.Close(); err != nil {
			return "", "", nil, err
		}
	}
}

func (homes localHomes) Resolve(_ context.Context, namespace, revision string) (HomeGrant, error) {
	home, ok := homes[revision]
	if namespace != "default" || !ok {
		return HomeGrant{}, fmt.Errorf("no local revision %s/%s", namespace, revision)
	}
	return HomeGrant{UID: "default--" + revision, MaxFibers: 8, Client: home}, nil
}

// Forget has nothing to drop: each local service has its one home, which
// no controller hands out and none can replace.
func (localHomes) Forget(string, string, string) {}

// Retain keeps every local home, since ly dev routes each of its services
// for as long as it runs.
func (localHomes) Retain(map[string]bool) {}

func (homes localHomes) Close() error {
	var failures []error
	for _, home := range homes {
		home.mu.Lock()
		home.closed = true
		for id := range home.fibers {
			failures = append(failures, home.release(id))
		}
		home.mu.Unlock()
	}
	return errors.Join(failures...)
}

func (home *localHome) Clone(ctx context.Context, _, session string, _ time.Duration, _ []byte) (consumer.Fiber, error) {
	address, port, releasePort, err := reserveLocalPort()
	if err != nil {
		return consumer.Fiber{}, err
	}
	defer releasePort()
	bindings, err := project.BindingArgs(home.service.Bindings, "default", home.bindings)
	if err != nil {
		return consumer.Fiber{}, err
	}
	var id string
	var fiber *localFiber
	for {
		home.mu.Lock()
		if home.closed {
			home.mu.Unlock()
			return consumer.Fiber{}, fmt.Errorf("local home is closed")
		}
		if err := ctx.Err(); err != nil {
			home.mu.Unlock()
			return consumer.Fiber{}, err
		}
		var waiting *localFiber
		for currentID, current := range home.fibers {
			select {
			case <-current.done:
				delete(home.fibers, currentID)
			default:
				if session != "" && current.session == session {
					waiting = current
				}
			}
		}
		if waiting != nil {
			if waiting.ready == nil {
				value := waiting.value
				value.Kind = consumer.Attach
				home.mu.Unlock()
				return value, nil
			}
			ready := waiting.ready
			home.mu.Unlock()
			select {
			case <-ready:
				home.mu.Lock()
				startErr := waiting.startErr
				current := home.fibers[waiting.value.ID] == waiting
				value := waiting.value
				home.mu.Unlock()
				if startErr != nil {
					return consumer.Fiber{}, startErr
				}
				if current {
					value.Kind = consumer.Attach
					return value, nil
				}
				continue
			case <-ctx.Done():
				return consumer.Fiber{}, ctx.Err()
			}
		}
		if len(home.fibers) >= 8 {
			home.mu.Unlock()
			return consumer.Fiber{}, &consumer.Shed{RetryAfter: time.Second}
		}
		home.sequence++
		id = fmt.Sprintf("%s-%d", home.service.Name, home.sequence)
		fiber = &localFiber{
			value: consumer.Fiber{
				ID: id, Endpoint: "tcp://" + address, Kind: consumer.Create,
			},
			session: session, done: make(chan struct{}), ready: make(chan struct{}),
		}
		home.fibers[id] = fiber
		home.mu.Unlock()
		break
	}
	args := []string{
		"snapshot", "run", home.service.Snapshot,
		"--env", "PORT=" + port,
		"--env", "LAMPYR_FIBER_ID=" + id,
		"--env", "LAMPYR_LISTEN_HOST=127.0.0.1",
		"--net-allow", "127.0.0.1",
		"--port", port, "--guest-exec", home.service.Command,
	}
	cmd := exec.Command(home.binary, append(args, bindings...)...)
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	home.mu.Lock()
	if home.closed || home.fibers[id] != fiber {
		home.failStart(id, fiber, fmt.Errorf("local home is closed"))
		home.mu.Unlock()
		return consumer.Fiber{}, fmt.Errorf("local home is closed")
	}
	if err := cmd.Start(); err != nil {
		failure := fmt.Errorf("start native fiber: %w", err)
		home.failStart(id, fiber, failure)
		home.mu.Unlock()
		return consumer.Fiber{}, failure
	}
	fiber.cmd = cmd
	home.mu.Unlock()
	go func() {
		waitErr := cmd.Wait()
		if waitErr != nil && !fiber.stopping.Load() {
			log.Printf("native fiber %s exited: %v", id, waitErr)
		}
		exitedBeforeReady := fiber.start.CompareAndSwap(localFiberStarting, localFiberExited)
		fiber.doneOnce.Do(func() { close(fiber.done) })
		home.mu.Lock()
		if exitedBeforeReady {
			fiber.startErr = fmt.Errorf("native fiber %s exited before readiness", id)
			fiber.readyOnce.Do(func() { close(fiber.ready) })
		}
		home.mu.Unlock()
	}()
	ticker := time.NewTicker(10 * time.Millisecond)
	defer ticker.Stop()
	for {
		connection, err := net.DialTimeout("tcp", address, 20*time.Millisecond)
		if err == nil {
			connection.Close()
			select {
			case <-fiber.done:
				home.mu.Lock()
				delete(home.fibers, id)
				home.mu.Unlock()
				return consumer.Fiber{}, fmt.Errorf("native fiber %s exited during readiness", id)
			default:
			}
			owned, err := processOwnsLocalListener(ctx, cmd.Process.Pid, port)
			if err != nil {
				cleanup := home.Release(context.Background(), id, true)
				return consumer.Fiber{}, errors.Join(fmt.Errorf("inspect native fiber listener: %w", err), cleanup)
			}
			if !owned {
				goto wait
			}
			// A server must honor LAMPYR_LISTEN_HOST. Refuse a
			// wildcard listener rather than exposing a lifecycle bypass.
			if exposed, err := net.DialTimeout("tcp", net.JoinHostPort(home.hostIP, port), 20*time.Millisecond); err == nil {
				exposed.Close()
				cleanup := home.Release(context.Background(), id, true)
				return consumer.Fiber{}, errors.Join(fmt.Errorf("%s must bind LAMPYR_LISTEN_HOST, not all host interfaces", home.service.Name), cleanup)
			}
			if ctx.Err() == nil {
				home.mu.Lock()
				if home.closed || home.fibers[id] != fiber {
					home.mu.Unlock()
					cleanup := home.Release(context.Background(), id, true)
					return consumer.Fiber{}, errors.Join(fmt.Errorf("local home is closed"), cleanup)
				}
				if !fiber.start.CompareAndSwap(localFiberStarting, localFiberReady) {
					failure := fiber.startErr
					if failure == nil {
						failure = fmt.Errorf("native fiber %s exited during readiness", id)
					}
					delete(home.fibers, id)
					home.mu.Unlock()
					return consumer.Fiber{}, failure
				}
				fiber.isReady = true
				fiber.readyOnce.Do(func() { close(fiber.ready) })
				value := fiber.value
				home.mu.Unlock()
				return value, nil
			}
		}
	wait:
		select {
		case <-ctx.Done():
			cleanup := home.Release(context.Background(), id, true)
			return consumer.Fiber{}, errors.Join(fmt.Errorf("native fiber readiness: %w", ctx.Err()), cleanup)
		case <-fiber.done:
			home.mu.Lock()
			delete(home.fibers, id)
			home.mu.Unlock()
			return consumer.Fiber{}, fmt.Errorf("native fiber %s exited before readiness", id)
		case <-ticker.C:
		}
	}
}

// home.mu must be held.
func (home *localHome) failStart(id string, fiber *localFiber, failure error) {
	if home.fibers[id] != fiber {
		return
	}
	fiber.start.CompareAndSwap(localFiberStarting, localFiberExited)
	fiber.startErr = failure
	fiber.readyOnce.Do(func() { close(fiber.ready) })
	fiber.doneOnce.Do(func() { close(fiber.done) })
	delete(home.fibers, id)
}

// The home owns every native process, including idle reused fibers. Wait before
// forgetting it so its VM and listener cannot outlive Release or shutdown.
func (home *localHome) release(id string) error {
	fiber := home.fibers[id]
	if fiber == nil {
		return nil
	}
	if !fiber.isReady {
		fiber.start.CompareAndSwap(localFiberStarting, localFiberExited)
		fiber.startErr = fmt.Errorf("native fiber %s stopped before readiness", id)
		if fiber.ready != nil {
			fiber.readyOnce.Do(func() { close(fiber.ready) })
		}
	}
	if fiber.cmd == nil {
		fiber.doneOnce.Do(func() { close(fiber.done) })
		delete(home.fibers, id)
		return nil
	}
	select {
	case <-fiber.done:
	default:
		fiber.stopping.Store(true)
		if err := fiber.cmd.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
			return err
		}
		<-fiber.done
	}
	delete(home.fibers, id)
	return nil
}

func (home *localHome) Release(_ context.Context, id string, _ bool) error {
	home.mu.Lock()
	defer home.mu.Unlock()
	return home.release(id)
}

// RunLocal replaces discovery/admission only. Requests, streams, cancellation,
// reuse pools and release all pass through the deployed Router.
func RunLocal(ctx context.Context, config LocalConfig) (returnErr error) {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	if config.SchemaVersion != 3 {
		return fmt.Errorf("unsupported local plan schema; install or rebuild the matching ly and ly-dev")
	}
	if config.Binary == "" || len(config.Services) == 0 {
		return fmt.Errorf("native binary and services are required")
	}
	host, _, err := net.SplitHostPort(config.Listen)
	if err != nil || host != "127.0.0.1" {
		return fmt.Errorf("local ingress must listen on 127.0.0.1")
	}
	routesDir, err := os.MkdirTemp("", "lampyr-local-routes-")
	if err != nil {
		return err
	}
	defer func() { returnErr = errors.Join(returnErr, os.RemoveAll(routesDir)) }()
	homes := localHomes{}
	defer func() { returnErr = errors.Join(returnErr, homes.Close()) }()
	endpoints := map[string]string{}
	bindingHost, err := localBindingHost()
	if err != nil {
		return err
	}
	tokens := map[string]string{}
	var listeners []net.Listener
	defer func() {
		for _, listener := range listeners {
			listener.Close()
		}
	}()
	for _, service := range config.Services {
		if !serviceNameLocal(service.Name) || service.Command == "" {
			return fmt.Errorf("invalid local service %q", service.Name)
		}
		if _, exists := homes[service.Name]; exists {
			return fmt.Errorf("duplicate local service %s", service.Name)
		}
		if info, err := os.Stat(service.Snapshot); err != nil || !info.IsDir() {
			return fmt.Errorf("local snapshot for %s is missing", service.Name)
		}
		if _, err := (project.Manifest{Execution: &service.Execution}).EffectiveExecution(); err != nil {
			return err
		}
		listener, err := net.Listen("tcp4", net.JoinHostPort(bindingHost, "0"))
		if err != nil {
			return err
		}
		listeners = append(listeners, listener)
		token := rand.Text()
		tokens[service.Name] = "/_lampyr/" + token
		endpoints[service.Name] = "http://" + listener.Addr().String() + tokens[service.Name]
		homes[service.Name] = &localHome{binary: config.Binary, service: service, bindings: endpoints, fibers: map[string]*localFiber{}, hostIP: bindingHost}
		route := RevisionRoute{
			Kind: "revision", Namespace: "default", Revision: service.Name,
			Execution: service.Execution, Timeout: "5m", Routed: true,
		}
		if err := route.Validate(); err != nil {
			return err
		}
		data, err := json.Marshal(route)
		if err != nil {
			return err
		}
		if err := os.WriteFile(filepath.Join(routesDir, service.Name+".json"), data, 0o600); err != nil {
			return err
		}
	}
	exposed := map[string]string{}
	for _, name := range config.ExposedServices {
		if homes[name] == nil {
			return fmt.Errorf("exposed service %s is not a local service", name)
		}
		hostname := name + ".default.localhost"
		if _, exists := exposed[hostname]; exists {
			return fmt.Errorf("duplicate exposed service %s", name)
		}
		exposed[hostname] = name
	}
	for _, service := range config.Services {
		if _, err := project.BindingArgs(service.Bindings, "default", endpoints); err != nil {
			return err
		}
	}
	router, err := NewRouter(homes, routesDir)
	if err != nil {
		return err
	}
	routerClosed := false
	defer func() {
		if !routerClosed {
			returnErr = errors.Join(returnErr, router.Close())
		}
	}()
	if config.Metrics {
		metrics := newLocalMetrics(os.Stdout)
		router.Observe(metrics.Observe)
		defer metrics.Summary()
	}
	if len(exposed) > 0 {
		ingress, err := net.Listen("tcp4", config.Listen)
		if err != nil {
			return err
		}
		listeners = append(listeners, ingress)
		for _, name := range config.ExposedServices {
			_, port, _ := net.SplitHostPort(ingress.Addr().String())
			fmt.Printf("running %s at http://%s.default.localhost:%s\n", name, name, port)
			fmt.Printf("  loopback fallback: curl -H 'Host: %s.default.localhost' http://127.0.0.1:%s/\n", name, port)
		}
	}
	var servers []*http.Server
	failures := make(chan error, len(listeners))
	for index, listener := range listeners {
		_, port, _ := net.SplitHostPort(listener.Addr().String())
		handler := localIngressHandler(router, exposed, port)
		if index < len(config.Services) {
			name := config.Services[index].Name
			handler = localHandler(router, name, tokens[name])
		}
		server := &http.Server{
			ReadHeaderTimeout: 10 * time.Second,
			Handler:           handler,
		}
		servers = append(servers, server)
		go func() { failures <- server.Serve(listener) }()
	}
	routerDone := make(chan struct{})
	go func() { defer close(routerDone); router.Run(ctx) }()
	fmt.Printf("%d local services; native snapshots; max 8 fibers/service\n", len(config.Services))
	select {
	case <-ctx.Done():
	case err := <-failures:
		returnErr = err
	}
	router.StopRequests()
	shutdownCtx, stopShutdown := context.WithTimeout(context.Background(), 5*time.Second)
	var shutdown sync.WaitGroup
	for _, server := range servers {
		shutdown.Add(1)
		go func() {
			defer shutdown.Done()
			_ = server.Shutdown(shutdownCtx)
		}()
	}
	shutdown.Wait()
	drained := router.WaitRequests(shutdownCtx)
	stopShutdown()
	if !drained {
		for _, server := range servers {
			returnErr = errors.Join(returnErr, server.Close())
		}
	}
	returnErr = errors.Join(returnErr, router.Close())
	routerClosed = true
	router.WaitRequests(context.Background())
	cancel()
	<-routerDone
	return returnErr
}

func localHandler(router *Router, name, capability string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		if capability != "" {
			switch {
			case request.URL.Path == capability:
				request.URL.Path = "/"
			case strings.HasPrefix(request.URL.Path, capability+"/"):
				request.URL.Path = strings.TrimPrefix(request.URL.Path, capability)
			default:
				http.NotFound(w, request)
				return
			}
			if request.URL.RawPath != "" {
				switch {
				case request.URL.RawPath == capability:
					request.URL.RawPath = "/"
				case strings.HasPrefix(request.URL.RawPath, capability+"/"):
					request.URL.RawPath = strings.TrimPrefix(request.URL.RawPath, capability)
				default:
					request.URL.RawPath = ""
				}
			}
		}
		// Host headers cannot select another service through this endpoint.
		request.Host = name
		if capability == "" {
			localForward(router, name, w, request)
			return
		}
		localForward(router, name, &localBindingResponseWriter{
			ResponseWriter: w,
			capability:     capability,
		}, request)
	})
}

type localBindingResponseWriter struct {
	http.ResponseWriter
	capability string
	rewritten  bool
}

func (writer *localBindingResponseWriter) Unwrap() http.ResponseWriter {
	return writer.ResponseWriter
}

func (writer *localBindingResponseWriter) WriteHeader(status int) {
	if status >= 200 {
		writer.rewriteLocation()
	}
	writer.ResponseWriter.WriteHeader(status)
}

func (writer *localBindingResponseWriter) Write(body []byte) (int, error) {
	writer.rewriteLocation()
	return writer.ResponseWriter.Write(body)
}

func (writer *localBindingResponseWriter) rewriteLocation() {
	if writer.rewritten {
		return
	}
	writer.rewritten = true
	location := writer.Header().Get("Location")
	if strings.HasPrefix(location, "/") && !strings.HasPrefix(location, "//") {
		writer.Header().Set("Location", writer.capability+location)
	}
}

func localIngressHandler(router *Router, exposed map[string]string, port string) http.Handler {
	type serviceLink struct {
		Name string
		URL  string
	}
	hosts := make([]string, 0, len(exposed))
	for host := range exposed {
		hosts = append(hosts, host)
	}
	sort.Strings(hosts)
	links := make([]serviceLink, 0, len(hosts))
	for _, host := range hosts {
		links = append(links, serviceLink{
			Name: exposed[host],
			URL:  "http://" + net.JoinHostPort(host, port) + "/",
		})
	}
	var page strings.Builder
	pageErr := localIndexTemplate.Execute(&page, links)
	return http.HandlerFunc(func(w http.ResponseWriter, request *http.Request) {
		host := request.Host
		if strings.Contains(host, ":") {
			var port string
			var err error
			host, port, err = net.SplitHostPort(host)
			number, parseErr := strconv.ParseUint(port, 10, 16)
			if err != nil || parseErr != nil || number == 0 {
				http.NotFound(w, request)
				return
			}
		}
		host = strings.ToLower(strings.TrimSuffix(host, "."))
		name, ok := exposed[host]
		if !ok {
			if pageErr != nil {
				log.Printf("render local service index: %v", pageErr)
				http.Error(w, "could not render local service index", http.StatusInternalServerError)
				return
			}
			status := http.StatusNotFound
			if (host == "localhost" || host == "127.0.0.1" || host == "::1") &&
				request.URL.Path == "/" &&
				(request.Method == http.MethodGet || request.Method == http.MethodHead) {
				status = http.StatusOK
			}
			w.Header().Set("Content-Type", "text/html; charset=utf-8")
			w.Header().Set("X-Content-Type-Options", "nosniff")
			w.WriteHeader(status)
			fmt.Fprint(w, page.String())
			return
		}
		localForward(router, name, w, request)
	})
}

var localIndexTemplate = template.Must(template.New("services").Parse(`<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Lampyr local services</title></head>
<body><h1>Lampyr local services</h1><p>Choose an exposed service:</p>
<ul>{{range .}}<li><a href="{{.URL}}">{{.Name}}</a></li>{{else}}<li>No services are exposed.</li>{{end}}</ul>
</body></html>
`))

func localForward(router *Router, name string, w http.ResponseWriter, request *http.Request) {
	request.Header.Del("K-Original-Host")
	request.Header.Set("Knative-Serving-Namespace", "default")
	request.Header.Set("Knative-Serving-Revision", name)
	router.ServeHTTP(w, request)
}

func localBindingHost() (string, error) {
	addresses, err := net.InterfaceAddrs()
	if err != nil {
		return "", err
	}
	for _, address := range addresses {
		ip, _, err := net.ParseCIDR(address.String())
		if err == nil && ip.To4() != nil && ip.IsPrivate() && !ip.IsLoopback() {
			return ip.String(), nil
		}
	}
	return "", fmt.Errorf("native bindings require a private non-loopback IPv4 interface: Hyperlight's allowlist forbids loopback; unrestricted networking will not be substituted")
}

func serviceNameLocal(name string) bool {
	_, err := project.BindingArgs(map[string]string{"service": name}, "default", nil)
	return err == nil
}
