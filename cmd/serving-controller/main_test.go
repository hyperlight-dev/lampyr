// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"context"
	"reflect"
	"runtime"
	"strings"
	"testing"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	discoveryfake "k8s.io/client-go/discovery/fake"
	"k8s.io/client-go/kubernetes/fake"
	clienttesting "k8s.io/client-go/testing"
	"knative.dev/pkg/system"
	_ "knative.dev/pkg/system/testing"
)

func TestControllerComposition(t *testing.T) {
	var names []string
	for _, constructor := range controllers {
		function := runtime.FuncForPC(reflect.ValueOf(constructor).Pointer())
		if function == nil {
			t.Fatal("controller constructor has no function metadata")
		}
		names = append(names, function.Name())
	}

	required := []string{
		"pkg/reconciler/configuration.NewController",
		"pkg/reconciler/labeler.NewController",
		"internal/servingrevision.NewController",
		"pkg/reconciler/route.NewController",
		"pkg/reconciler/service.NewController",
		"pkg/reconciler/gc.NewController",
		"pkg/reconciler/nscert.NewController",
		"pkg/reconciler/domainmapping.NewController",
	}
	for _, suffix := range required {
		if !containsSuffix(names, suffix) {
			t.Errorf("controller composition is missing %q: %v", suffix, names)
		}
	}
	for _, forbidden := range []string{
		"pkg/reconciler/revision.NewController",
		"pkg/reconciler/serverlessservice.NewController",
	} {
		if containsSuffix(names, forbidden) {
			t.Errorf("controller composition includes forbidden %q: %v", forbidden, names)
		}
	}
}

func containsSuffix(values []string, suffix string) bool {
	for _, value := range values {
		if strings.HasSuffix(value, suffix) {
			return true
		}
	}
	return false
}

func TestCertManagerControllerRequirementFollowsNetworkConfig(t *testing.T) {
	client := fake.NewSimpleClientset(&corev1.ConfigMap{
		ObjectMeta: metav1.ObjectMeta{
			Name:      "config-network",
			Namespace: system.Namespace(),
		},
		Data: map[string]string{"external-domain-tls": "Enabled"},
	})
	required, err := shouldEnableNetCertManagerController(context.Background(), client)
	if err != nil {
		t.Fatal(err)
	}
	if !required {
		t.Fatal("external-domain-tls did not enable the cert-manager controller")
	}
}

func TestCertManagerCRDRequirements(t *testing.T) {
	discovery := &discoveryfake.FakeDiscovery{Fake: &clienttesting.Fake{}}
	discovery.Resources = []*metav1.APIResourceList{
		{
			GroupVersion: "cert-manager.io/v1",
			APIResources: []metav1.APIResource{
				{Name: "certificaterequests"},
				{Name: "certificates"},
				{Name: "clusterissuers"},
				{Name: "issuers"},
			},
		},
		{
			GroupVersion: "acme.cert-manager.io/v1",
			APIResources: []metav1.APIResource{{Name: "challenges"}},
		},
	}
	if err := certManagerCRDsExist(discovery); err != nil {
		t.Fatal(err)
	}
	discovery.Resources[0].APIResources = discovery.Resources[0].APIResources[:3]
	if err := certManagerCRDsExist(discovery); err == nil {
		t.Fatal("missing cert-manager CRD was accepted")
	}
}
