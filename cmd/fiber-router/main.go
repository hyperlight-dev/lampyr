// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/hyperlight-dev/lampyr/internal/fiberdintegration"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/rest"
)

// fiberd-k8s-controller's issuer, and where k8s/fiber-router.yaml mounts
// the router's client certificate, whose key every grant is bound to, and
// the fiberd CA.
const (
	issuerURL      = "https://fiberd-k8s-controller.fiberd-system.svc:8443"
	callerCertFile = "/var/run/fiberd/tls/tls.crt"
	callerKeyFile  = "/var/run/fiberd/tls/tls.key"
	fiberdCAFile   = "/var/run/fiberd/tls/ca.crt"
)

// apiClient reads CapacityGrants with the Pod's service account. It does
// not throttle: the router reads a grant at most once per refresh of it.
func apiClient() (dynamic.Interface, error) {
	config, err := rest.InClusterConfig()
	if err != nil {
		return nil, err
	}
	config.Timeout = 30 * time.Second
	config.QPS = -1
	return dynamic.NewForConfig(config)
}

func main() {
	listen := flag.String("listen", ":8080", "HTTP route address")
	routes := flag.String("routes-dir", "/var/lib/lampyr/routes", "projected route configuration")
	metrics := flag.String("metrics-listen", ":9090", "Prometheus metrics address; empty disables it")
	acquireTimeout := flag.Duration("acquire-timeout", fiberdintegration.DefaultAcquireTimeout, "how long a request waits for its grant's home, and then for the clone")
	probe := flag.String("probe", "", "GET a path from the running local router")
	probePort := flag.Int("probe-port", 8080, "local router port for --probe")
	flag.Parse()
	if *probe != "" {
		if err := writeProbeResponse(*probePort, *probe, os.Stdout); err != nil {
			log.Fatal(err)
		}
		return
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	client, err := apiClient()
	if err != nil {
		log.Fatal(err)
	}
	router, err := fiberdintegration.NewRouter(&fiberdintegration.CallerGrants{
		API: client, Issuer: issuerURL, CertFile: callerCertFile, KeyFile: callerKeyFile, CAFile: fiberdCAFile,
	}, *routes)
	if err != nil {
		log.Fatal(err)
	}
	if *acquireTimeout <= 0 {
		router.Close()
		log.Fatal("-acquire-timeout must be positive")
	}
	router.AcquireTimeout = *acquireTimeout
	if *metrics != "" {
		metricsServer := &http.Server{Addr: *metrics, Handler: fiberdintegration.MetricsHandler(), ReadHeaderTimeout: 5 * time.Second}
		go func() {
			if err := metricsServer.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
				log.Printf("metrics listener: %v", err)
			}
		}()
		go func() {
			<-ctx.Done()
			metricsServer.Close()
		}()
	}
	// Kourier speaks h2c to the fiber-router-h2c Service, for gRPC fibers;
	// routes through the fiber-router Service stay HTTP/1.1.
	protocols := new(http.Protocols)
	protocols.SetHTTP1(true)
	protocols.SetUnencryptedHTTP2(true)
	server := &http.Server{Handler: router.Handler(), ReadHeaderTimeout: 5 * time.Second, Protocols: protocols}
	listener, err := net.Listen("tcp", *listen)
	if err != nil {
		router.Close()
		log.Fatal(err)
	}
	log.Printf("Lampyr protocol consumer on %s", *listen)
	if err := serve(ctx, listener, server, router); err != nil {
		log.Println(err)
		os.Exit(1)
	}
}

// drainTimeout is how long a shutdown lets requests, streams and WebSockets
// finish, within the Pod's terminationGracePeriodSeconds of 30 so their
// fibers are still released before the kubelet kills the router.
var drainTimeout = 20 * time.Second

type routerLoop interface {
	Run(context.Context)
	Close() error
}

// serve runs router and server until ctx ends or the listener fails. Serve
// returns as soon as Shutdown starts, so it waits for the drain and then for
// the router's reload loop to stop before closing the router, which no
// request or sweep may use once it is closed. At the drain deadline, or at
// once if the listener failed, every request's context ends, which Shutdown
// alone does not do for streams and hijacked WebSockets, so their fibers are
// released before Close returns.
func serve(ctx context.Context, listener net.Listener, server *http.Server, router routerLoop) error {
	base, endRequests := context.WithCancel(context.Background())
	defer endRequests()
	server.BaseContext = func(net.Listener) context.Context { return base }
	loop, stopLoop := context.WithCancel(context.Background())
	looped := make(chan struct{})
	go func() {
		defer close(looped)
		router.Run(loop)
	}()
	drained := make(chan struct{})
	failed := make(chan struct{})
	go func() {
		defer close(drained)
		select {
		case <-ctx.Done():
		case <-failed:
			// Nothing is left to drain, and waiting on ctx would outlive
			// serve.
			return
		}
		time.AfterFunc(drainTimeout, endRequests)
		closeCtx, cancel := context.WithTimeout(context.Background(), drainTimeout)
		defer cancel()
		if err := server.Shutdown(closeCtx); err != nil {
			log.Printf("router shutdown: %v", err)
			endRequests()
			server.Close()
		}
	}()
	err := server.Serve(listener)
	if errors.Is(err, http.ErrServerClosed) {
		err = nil
	} else {
		// The listener failed with no shutdown to drain, so nothing else
		// would end the requests in flight, and Router.Close waits for
		// each of their fibers' release.
		close(failed)
		endRequests()
		server.Close()
	}
	<-drained
	stopLoop()
	<-looped
	if closeErr := router.Close(); err == nil {
		err = closeErr
	}
	return err
}

func writeProbeResponse(port int, path string, output io.Writer) error {
	if port < 1 || port > 65535 {
		return fmt.Errorf("probe port must be between 1 and 65535")
	}
	if !strings.HasPrefix(path, "/") || strings.ContainsAny(path, "\r\n") {
		return fmt.Errorf("probe path must be an absolute HTTP path")
	}
	response, err := (&http.Client{Timeout: 5 * time.Second}).Get(
		fmt.Sprintf("http://127.0.0.1:%d%s", port, path),
	)
	if err != nil {
		return fmt.Errorf("probe local router: %w", err)
	}
	defer response.Body.Close()
	var encoded bytes.Buffer
	if err := response.Write(&encoded); err != nil {
		return fmt.Errorf("encode probe response: %w", err)
	}
	if _, err := io.Copy(output, &encoded); err != nil {
		return fmt.Errorf("write probe response: %w", err)
	}
	return nil
}
