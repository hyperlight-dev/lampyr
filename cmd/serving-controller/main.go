// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"context"
	"flag"
	"fmt"
	"log"

	versioned "github.com/cert-manager/cert-manager/pkg/client/clientset/versioned"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/discovery"
	"k8s.io/client-go/kubernetes"
	netcfg "knative.dev/networking/pkg/config"
	"knative.dev/pkg/injection"
	"knative.dev/pkg/injection/sharedmain"
	"knative.dev/pkg/reconciler"
	"knative.dev/pkg/signals"
	"knative.dev/pkg/system"
	"knative.dev/serving/pkg/client/certmanager/injection/informers/acme/v1/challenge"
	v1certificate "knative.dev/serving/pkg/client/certmanager/injection/informers/certmanager/v1/certificate"
	"knative.dev/serving/pkg/client/certmanager/injection/informers/certmanager/v1/certificaterequest"
	"knative.dev/serving/pkg/client/certmanager/injection/informers/certmanager/v1/clusterissuer"
	"knative.dev/serving/pkg/client/certmanager/injection/informers/certmanager/v1/issuer"
	"knative.dev/serving/pkg/networking"
	"knative.dev/serving/pkg/reconciler/certificate"
	"knative.dev/serving/pkg/reconciler/configuration"
	"knative.dev/serving/pkg/reconciler/domainmapping"
	"knative.dev/serving/pkg/reconciler/gc"
	"knative.dev/serving/pkg/reconciler/labeler"
	"knative.dev/serving/pkg/reconciler/nscert"
	"knative.dev/serving/pkg/reconciler/route"
	"knative.dev/serving/pkg/reconciler/service"

	"github.com/hyperlight-dev/lampyr/internal/servingrevision"
)

var controllers = []injection.ControllerConstructor{
	configuration.NewController,
	labeler.NewController,
	servingrevision.NewController,
	route.NewController,
	service.NewController,
	gc.NewController,
	nscert.NewController,
	domainmapping.NewController,
}

func main() {
	flag.DurationVar(
		&reconciler.DefaultTimeout,
		"reconciliation-timeout",
		reconciler.DefaultTimeout,
		"maximum duration of one reconciliation",
	)
	ctx := signals.NewContext()
	config := injection.ParseAndGetRESTConfigOrDie()
	constructors := append([]injection.ControllerConstructor{}, controllers...)
	kube := kubernetes.NewForConfigOrDie(config)
	required, err := shouldEnableNetCertManagerController(ctx, kube)
	if err != nil {
		log.Fatal(err)
	}
	if required {
		certManager := versioned.NewForConfigOrDie(config)
		if err := certManagerCRDsExist(certManager.Discovery()); err != nil {
			log.Fatalf("Please install cert-manager: %v", err)
		}
		for _, informer := range []injection.InformerInjector{
			challenge.WithInformer,
			v1certificate.WithInformer,
			certificaterequest.WithInformer,
			clusterissuer.WithInformer,
			issuer.WithInformer,
		} {
			injection.Default.RegisterInformer(informer)
		}
		constructors = append(constructors, certificate.NewController)
	}
	sharedmain.MainWithConfig(ctx, "lampyr-serving-controller", config, constructors...)
}

func shouldEnableNetCertManagerController(
	ctx context.Context,
	client kubernetes.Interface,
) (bool, error) {
	config, err := client.CoreV1().ConfigMaps(system.Namespace()).Get(
		ctx,
		"config-network",
		metav1.GetOptions{},
	)
	if err != nil {
		return false, fmt.Errorf("read config-network: %w", err)
	}
	networkConfig, err := netcfg.NewConfigFromMap(config.Data)
	if err != nil {
		return false, fmt.Errorf("construct network config: %w", err)
	}
	return networking.IsNetCertManagerControllerRequired(networkConfig), nil
}

func certManagerCRDsExist(client discovery.DiscoveryInterface) error {
	if err := findCRDs(
		client,
		"cert-manager.io/v1",
		[]string{"certificaterequests", "certificates", "clusterissuers", "issuers"},
	); err != nil {
		return err
	}
	return findCRDs(client, "acme.cert-manager.io/v1", []string{"challenges"})
}

func findCRDs(
	client discovery.DiscoveryInterface,
	groupVersion string,
	required []string,
) error {
	resources, err := client.ServerResourcesForGroupVersion(groupVersion)
	if err != nil {
		return err
	}
	available := make(map[string]bool, len(resources.APIResources))
	for _, resource := range resources.APIResources {
		available[resource.Name] = true
	}
	for _, resource := range required {
		if !available[resource] {
			return fmt.Errorf("cert-manager CRD is missing: %s", resource)
		}
	}
	return nil
}
