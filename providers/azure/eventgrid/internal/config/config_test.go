// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package config

import (
	"testing"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

func TestBuildWebhookUsesBoundedConcurrency(t *testing.T) {
	source := testSource()
	source.Spec.Delivery.MaxConcurrentDeliveries = 4
	adapterConfig, _, digest, err := BuildWebhook(
		source,
		"http://broker.default.svc.cluster.local",
	)
	if err != nil {
		t.Fatal(err)
	}
	if adapterConfig.Webhook.MaxConcurrentDeliveries != 4 {
		t.Fatalf("webhook controls = %#v", adapterConfig.Webhook)
	}
	if digest == "" {
		t.Fatal("configuration digest is empty")
	}
}

func TestBuildDigestChangesWithGenerationAndSink(t *testing.T) {
	source := testSource()
	_, _, first, err := BuildWebhook(
		source,
		"http://broker.default.svc.cluster.local",
	)
	if err != nil {
		t.Fatal(err)
	}
	source.Generation++
	_, _, second, err := BuildWebhook(
		source,
		"http://broker.default.svc.cluster.local",
	)
	if err != nil {
		t.Fatal(err)
	}
	if first == second {
		t.Fatal("generation change did not change configuration digest")
	}
	_, _, third, err := BuildWebhook(
		source,
		"http://service.default.svc.cluster.local",
	)
	if err != nil {
		t.Fatal(err)
	}
	if second == third {
		t.Fatal("sink change did not change configuration digest")
	}
}

func testSource() *api.Source {
	return &api.Source{
		ObjectMeta: metav1.ObjectMeta{
			Name:       "uploads",
			Namespace:  "default",
			UID:        types.UID("source-uid"),
			Generation: 1,
		},
		Spec: api.SourceSpec{
			SourceResourceID: "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/demo/providers/Microsoft.Storage/storageAccounts/uploads",
			Sink: api.Destination{
				URI: "http://broker.default.svc.cluster.local",
			},
		},
	}
}
