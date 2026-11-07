// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package destination

import (
	"context"
	"testing"

	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/dynamic/fake"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

func TestResolveDestination(t *testing.T) {
	scheme := runtime.NewScheme()
	broker := &unstructured.Unstructured{Object: map[string]any{
		"apiVersion": "eventing.knative.dev/v1",
		"kind":       "Broker",
		"metadata": map[string]any{
			"name":      "default",
			"namespace": "demo",
		},
		"status": map[string]any{
			"address": map[string]any{"url": "http://broker-ingress.knative-eventing.svc.cluster.local/demo/default"},
		},
	}}
	client := fake.NewSimpleDynamicClient(scheme, broker)
	mapper := staticMapper{
		resource: schema.GroupVersionResource{Group: "eventing.knative.dev", Version: "v1", Resource: "brokers"},
	}
	resolver := NewResolver(client, mapper)

	got, err := resolver.Resolve(context.Background(), "demo", api.Destination{
		Ref: &api.KReference{
			APIVersion: "eventing.knative.dev/v1",
			Kind:       "Broker",
			Name:       "default",
		},
	})
	if err != nil {
		t.Fatalf("Resolve() = %v", err)
	}
	if want := "http://broker-ingress.knative-eventing.svc.cluster.local/demo/default"; got != want {
		t.Fatalf("Resolve() = %q, want %q", got, want)
	}

	got, err = resolver.Resolve(context.Background(), "demo", api.Destination{
		URI: "http://receiver.demo.svc.cluster.local/events",
	})
	if err != nil {
		t.Fatalf("Resolve(uri) = %v", err)
	}
	if got != "http://receiver.demo.svc.cluster.local/events" {
		t.Fatalf("Resolve(uri) = %q", got)
	}
}

func TestResolveDestinationRequiresReadyAddress(t *testing.T) {
	client := fake.NewSimpleDynamicClient(runtime.NewScheme(), &unstructured.Unstructured{Object: map[string]any{
		"apiVersion": "serving.knative.dev/v1",
		"kind":       "Service",
		"metadata": map[string]any{
			"name":      "receiver",
			"namespace": "demo",
		},
	}})
	mapper := staticMapper{
		resource: schema.GroupVersionResource{Group: "serving.knative.dev", Version: "v1", Resource: "services"},
	}

	resolver := NewResolver(client, mapper)

	_, err := resolver.Resolve(context.Background(), "demo", api.Destination{
		Ref: &api.KReference{APIVersion: "serving.knative.dev/v1", Kind: "Service", Name: "receiver"},
	})
	if err == nil {
		t.Fatal("Resolve() unexpectedly succeeded")
	}
}

func TestResolveDestinationRejectsCrossNamespaceReference(t *testing.T) {
	resolver := NewResolver(
		fake.NewSimpleDynamicClient(runtime.NewScheme()),
		staticMapper{
			resource: schema.GroupVersionResource{
				Group:    "eventing.knative.dev",
				Version:  "v1",
				Resource: "brokers",
			},
		},
	)
	_, err := resolver.Resolve(
		context.Background(),
		"demo",
		api.Destination{Ref: &api.KReference{
			APIVersion: "eventing.knative.dev/v1",
			Kind:       "Broker",
			Name:       "default",
			Namespace:  "other",
		}},
	)
	if err == nil {
		t.Fatal("cross-namespace reference was accepted")
	}
}

type staticMapper struct {
	resource schema.GroupVersionResource
}

func (s staticMapper) ResourceFor(string, string) (schema.GroupVersionResource, error) {
	return s.resource, nil
}
