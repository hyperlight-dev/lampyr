// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/dynamic"
	corev1client "k8s.io/client-go/kubernetes/typed/core/v1"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/adapter"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/health"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/kubeconfig"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

func main() {
	healthAddress := flag.String(
		"health-address",
		":8080",
		"health and readiness listen address",
	)
	maxInFlight := flag.Int(
		"max-in-flight",
		64,
		"maximum deliveries in flight across all sources",
	)
	webhookAddress := flag.String(
		"webhook-address",
		"",
		"Event Grid webhook HTTP listen address behind Azure Front Door",
	)
	webhookAuthKeyFile := flag.String(
		"webhook-auth-key-file",
		"",
		"path to the shared webhook authentication key",
	)
	frontDoorID := flag.String(
		"azure-front-door-id",
		"",
		"required X-Azure-FDID value for public webhook requests",
	)
	flag.Parse()

	ctx, stop := signal.NotifyContext(
		context.Background(),
		syscall.SIGINT,
		syscall.SIGTERM,
	)
	defer stop()
	if err := run(ctx, options{
		healthAddress:      *healthAddress,
		maxInFlight:        *maxInFlight,
		webhookAddress:     *webhookAddress,
		webhookAuthKeyFile: *webhookAuthKeyFile,
		frontDoorID:        *frontDoorID,
	}); err != nil {
		log.Fatal(err)
	}
}

type options struct {
	healthAddress      string
	maxInFlight        int
	webhookAddress     string
	webhookAuthKeyFile string
	frontDoorID        string
}

func run(ctx context.Context, options options) error {
	runCtx, cancelRun := context.WithCancel(ctx)
	defer cancelRun()
	webhookEnabled := options.webhookAddress != "" ||
		options.webhookAuthKeyFile != ""
	var webhookKeys *adapter.RotatingWebhookKeys
	if webhookEnabled {
		if options.webhookAddress == "" || options.webhookAuthKeyFile == "" {
			return fmt.Errorf(
				"webhook address and authentication key file must be configured together",
			)
		}
		if _, err := waitForWebhookAuthKey(
			runCtx,
			options.webhookAuthKeyFile,
			250*time.Millisecond,
		); err != nil {
			return err
		}
		webhookKeys = adapter.NewRotatingWebhookKeys(
			func() ([]byte, error) {
				return readWebhookAuthKey(options.webhookAuthKeyFile)
			},
		)
		if _, err := webhookKeys.Refresh(); err != nil {
			return err
		}
	}
	restConfig, err := kubeconfig.Load()
	if err != nil {
		return err
	}
	dynamicClient, err := dynamic.NewForConfig(restConfig)
	if err != nil {
		return err
	}
	var keyAcknowledgements corev1client.ConfigMapInterface
	if webhookEnabled {
		coreClient, err := corev1client.NewForConfig(restConfig)
		if err != nil {
			return err
		}
		keyAcknowledgements = coreClient.ConfigMaps(webhook.SecretNamespace)
	}
	httpClient := &http.Client{Timeout: 30 * time.Second}
	processor := adapter.NewProcessor(adapter.NewHTTPSink(httpClient))
	processor.SetMaxInFlight(options.maxInFlight)
	adapterRuntime := adapter.NewRuntime(dynamicClient, processor)

	healthHandler := health.NewHandler(adapterRuntime.Ready)
	privateHandler := http.NewServeMux()
	privateHandler.Handle("/metrics", processor.Metrics())
	privateHandler.Handle("/", healthHandler)
	healthServer := &http.Server{
		Addr:              options.healthAddress,
		Handler:           privateHandler,
		ReadHeaderTimeout: 5 * time.Second,
	}

	serverErrors := make(chan error, 2)
	runtimeDone := make(chan error, 1)
	if webhookEnabled {
		go func() {
			if err := acknowledgeWebhookKeys(
				runCtx,
				keyAcknowledgements,
				webhookKeys,
				250*time.Millisecond,
			); err != nil {
				serverErrors <- err
			}
		}()
	}
	go serveHTTP(healthServer, serverErrors)
	go func() {
		runtimeDone <- adapterRuntime.Run(runCtx)
	}()

	var webhookServer *http.Server
	if webhookEnabled {
		publicHandler := http.NewServeMux()
		publicHandler.Handle("/healthz", healthHandler)
		publicHandler.Handle(
			"/",
			adapter.NewWebhookHandlerWithKeys(
				adapterRuntime,
				webhookKeys,
				options.frontDoorID,
			),
		)
		webhookServer = &http.Server{
			Addr:              options.webhookAddress,
			Handler:           publicHandler,
			ReadHeaderTimeout: 5 * time.Second,
			ReadTimeout:       30 * time.Second,
		}
		go serveHTTP(webhookServer, serverErrors)
	}

	var runErr error
	runtimeFinished := false
	select {
	case <-ctx.Done():
	case runErr = <-serverErrors:
	case runErr = <-runtimeDone:
		runtimeFinished = true
	}
	cancelRun()
	shutdownCtx, cancel := context.WithTimeout(
		context.Background(),
		14*time.Second,
	)
	defer cancel()
	shutdownErr := healthServer.Shutdown(shutdownCtx)
	if webhookServer != nil {
		shutdownErr = errors.Join(
			shutdownErr,
			webhookServer.Shutdown(shutdownCtx),
		)
	}
	if !runtimeFinished {
		select {
		case runtimeErr := <-runtimeDone:
			runErr = errors.Join(runErr, runtimeErr)
		case <-shutdownCtx.Done():
			runErr = errors.Join(
				runErr,
				fmt.Errorf("timed out waiting for adapter workers to stop"),
			)
		}
	}
	return errors.Join(runErr, shutdownErr)
}

func readWebhookAuthKey(path string) ([]byte, error) {
	authKey, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read webhook authentication key: %w", err)
	}
	if len(authKey) < 32 {
		return nil, fmt.Errorf(
			"webhook authentication key must contain at least 32 bytes",
		)
	}
	return authKey, nil
}

func waitForWebhookAuthKey(
	ctx context.Context,
	path string,
	interval time.Duration,
) ([]byte, error) {
	for {
		authKey, err := readWebhookAuthKey(path)
		if err == nil {
			return authKey, nil
		}
		timer := time.NewTimer(interval)
		select {
		case <-ctx.Done():
			timer.Stop()
			return nil, fmt.Errorf(
				"wait for webhook authentication key: %w",
				ctx.Err(),
			)
		case <-timer.C:
		}
	}
}

func acknowledgeWebhookKeys(
	ctx context.Context,
	configMaps corev1client.ConfigMapInterface,
	keys *adapter.RotatingWebhookKeys,
	interval time.Duration,
) error {
	lastDigest := ""
	for {
		digest, err := keys.Refresh()
		if err != nil {
			return err
		}
		if digest != lastDigest {
			if err := publishWebhookKeyAcknowledgement(
				ctx,
				configMaps,
				digest,
			); err != nil {
				return err
			}
			lastDigest = digest
		}
		timer := time.NewTimer(interval)
		select {
		case <-ctx.Done():
			timer.Stop()
			return nil
		case <-timer.C:
		}
	}
}

func publishWebhookKeyAcknowledgement(
	ctx context.Context,
	configMaps corev1client.ConfigMapInterface,
	digest string,
) error {
	current, err := configMaps.Get(
		ctx,
		webhook.KeyAcknowledgementConfigMap,
		metav1.GetOptions{},
	)
	if apierrors.IsNotFound(err) {
		_, err = configMaps.Create(
			ctx,
			&corev1.ConfigMap{
				ObjectMeta: metav1.ObjectMeta{
					Name:      webhook.KeyAcknowledgementConfigMap,
					Namespace: webhook.SecretNamespace,
				},
				Data: map[string]string{
					webhook.KeyDigestDataKey: digest,
				},
			},
			metav1.CreateOptions{},
		)
	} else if err == nil &&
		current.Data[webhook.KeyDigestDataKey] != digest {
		current = current.DeepCopy()
		if current.Data == nil {
			current.Data = make(map[string]string)
		}
		current.Data[webhook.KeyDigestDataKey] = digest
		_, err = configMaps.Update(ctx, current, metav1.UpdateOptions{})
	}
	if err != nil {
		return fmt.Errorf(
			"publish adapter webhook key acknowledgement: %w",
			err,
		)
	}
	return nil
}

func serveHTTP(server *http.Server, errorsChannel chan<- error) {
	err := server.ListenAndServe()
	if err != nil && !errors.Is(err, http.ErrServerClosed) {
		errorsChannel <- err
	}
}
