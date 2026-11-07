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
	"os/signal"
	"syscall"
	"time"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/discovery"
	"k8s.io/client-go/discovery/cached/memory"
	"k8s.io/client-go/dynamic"
	corev1client "k8s.io/client-go/kubernetes/typed/core/v1"
	"k8s.io/client-go/restmapper"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/controller"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/destination"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/health"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/kubeconfig"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/management"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

func main() {
	healthAddress := flag.String("health-address", ":8080", "health and readiness listen address")
	azureSubscriptionID := flag.String("azure-subscription-id", "", "Azure subscription ID")
	azureWebhookBaseURL := flag.String(
		"azure-webhook-base-url",
		"",
		"public HTTPS base URL for Event Grid delivery",
	)
	azurePublisherResourceGroup := flag.String(
		"azure-publisher-resource-group",
		"",
		"only Azure resource group from which publishers may be selected",
	)
	flag.Parse()

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, controllerOptions{
		healthAddress:               *healthAddress,
		azureSubscriptionID:         *azureSubscriptionID,
		azurePublisherResourceGroup: *azurePublisherResourceGroup,
		azureWebhookBaseURL:         *azureWebhookBaseURL,
	}); err != nil {
		log.Fatal(err)
	}
}

type controllerOptions struct {
	healthAddress               string
	azureSubscriptionID         string
	azurePublisherResourceGroup string
	azureWebhookBaseURL         string
}

func run(ctx context.Context, options controllerOptions) error {
	restConfig, err := kubeconfig.Load()
	if err != nil {
		return err
	}
	dynamicClient, err := dynamic.NewForConfig(restConfig)
	if err != nil {
		return err
	}
	discoveryClient, err := discovery.NewDiscoveryClientForConfig(restConfig)
	if err != nil {
		return err
	}
	mapper := restmapper.NewDeferredDiscoveryRESTMapper(memory.NewMemCacheClient(discoveryClient))
	resolver := destination.NewResolver(dynamicClient, destination.RESTMapper{Mapper: mapper})
	reconciler := controller.NewReconciler(dynamicClient, resolver)
	if options.azureSubscriptionID != "" {
		coreClient, err := corev1client.NewForConfig(restConfig)
		if err != nil {
			return err
		}
		secrets := coreClient.Secrets(webhook.SecretNamespace)
		configMaps := coreClient.ConfigMaps(webhook.SecretNamespace)
		if _, err := webhook.EnsureKey(
			ctx,
			secrets,
		); err != nil {
			return err
		}
		manager, err := management.NewAzureClient(management.AzureOptions{
			SubscriptionID:         options.azureSubscriptionID,
			PublisherResourceGroup: options.azurePublisherResourceGroup,
			WebhookBaseURL:         options.azureWebhookBaseURL,
			WebhookAuthKeyLoader: func(ctx context.Context) ([]byte, error) {
				return acknowledgedWebhookKey(ctx, secrets, configMaps)
			},
		})
		if err != nil {
			return err
		}

		reconciler.WithManagementClient(manager)
	}
	sourceController := controller.New(dynamicClient, reconciler)

	state := &health.State{}
	server := &http.Server{
		Addr:              options.healthAddress,
		Handler:           health.NewHandler(state.Ready),
		ReadHeaderTimeout: 5 * time.Second,
	}
	errorsChannel := make(chan error, 2)
	go func() {
		err := server.ListenAndServe()
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			errorsChannel <- err
		}
	}()
	state.SetReady(true)
	go func() {
		errorsChannel <- sourceController.Run(ctx)
	}()

	select {
	case <-ctx.Done():
	case err := <-errorsChannel:
		if err != nil {
			return err
		}
	}
	state.SetReady(false)
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	return server.Shutdown(shutdownCtx)
}

func acknowledgedWebhookKey(
	ctx context.Context,
	secrets corev1client.SecretInterface,
	configMaps corev1client.ConfigMapInterface,
) ([]byte, error) {
	key, err := webhook.EnsureKey(ctx, secrets)
	if err != nil {
		return nil, err
	}
	acknowledgement, err := configMaps.Get(
		ctx,
		webhook.KeyAcknowledgementConfigMap,
		metav1.GetOptions{},
	)
	if err != nil {
		if apierrors.IsNotFound(err) {
			return nil, fmt.Errorf(
				"adapter has not acknowledged the projected webhook authentication key",
			)
		}
		return nil, fmt.Errorf(
			"read adapter webhook key acknowledgement: %w",
			err,
		)
	}
	if acknowledgement.Data[webhook.KeyDigestDataKey] != webhook.KeyDigest(key) {
		return nil, fmt.Errorf(
			"adapter has not acknowledged the current projected webhook authentication key",
		)
	}
	return key, nil
}
