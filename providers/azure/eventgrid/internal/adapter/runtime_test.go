// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"testing"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/dynamic/fake"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/config"
)

func TestMarkRuntimeStatusTracksCurrentDigest(t *testing.T) {
	adapterConfig := runtimeConfig("source-uid")
	name := config.NameForUID(adapterConfig.Source.UID)
	configMap := testConfigMap(t, adapterConfig, "digest")
	client := fake.NewSimpleDynamicClientWithCustomListKinds(
		runtime.NewScheme(),
		map[schema.GroupVersionResource]string{config.ConfigGVR: "ConfigMapList"},
		configMap,
	)
	adapterRuntime := NewRuntime(client, testProcessor{})
	if err := adapterRuntime.markRuntimeStatus(
		context.Background(),
		"digest",
		adapterConfig,
		true,
		"",
		"",
	); err != nil {
		t.Fatal(err)
	}

	updated, err := client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Get(context.Background(), name, metav1.GetOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if updated.GetAnnotations()[config.AnnotationRuntimeReady] != "true" {
		t.Fatalf("annotations = %#v", updated.GetAnnotations())
	}
	if err := adapterRuntime.markRuntimeStatus(
		context.Background(),
		"stale-digest",
		adapterConfig,
		false,
		"SinkUnavailable",
		"stale worker",
	); err != nil {
		t.Fatal(err)
	}
	updated, err = client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Get(context.Background(), name, metav1.GetOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if updated.GetAnnotations()[config.AnnotationRuntimeReady] != "true" {
		t.Fatal("stale worker overwrote current runtime readiness")
	}
}

func TestParseConfigMapRejectsTamperedOrUnboundConfiguration(t *testing.T) {
	adapterConfig := runtimeConfig("source-uid")
	configMap := testConfigMap(t, adapterConfig, "tampered")
	if _, _, err := parseConfigMap(configMap); err == nil {
		t.Fatal("tampered configuration digest was accepted")
	}
	configMap.SetNamespace("default")
	if _, _, err := parseConfigMap(configMap); err == nil {
		t.Fatal("configuration outside protected namespace was accepted")
	}
}

func TestSyncAcknowledgesStopRequestAfterWebhookDrain(t *testing.T) {
	adapterConfig := runtimeConfig("source-uid")
	data, err := json.Marshal(adapterConfig)
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256(data)
	digest := hex.EncodeToString(sum[:])
	configMap := testConfigMap(t, adapterConfig, digest)
	annotations := configMap.GetAnnotations()
	annotations[config.AnnotationStopRequested] = "true"
	configMap.SetAnnotations(annotations)
	client := fake.NewSimpleDynamicClientWithCustomListKinds(
		runtime.NewScheme(),
		map[schema.GroupVersionResource]string{config.ConfigGVR: "ConfigMapList"},
		configMap,
	)
	adapterRuntime := NewRuntime(client, testProcessor{})
	if err := adapterRuntime.sync(context.Background()); err != nil {
		t.Fatal(err)
	}
	updated, err := client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Get(context.Background(), configMap.GetName(), metav1.GetOptions{})
	if err != nil {
		t.Fatal(err)
	}
	if updated.GetAnnotations()[config.AnnotationStopped] != "true" {
		t.Fatalf("stop was not acknowledged: %#v", updated.GetAnnotations())
	}
}

func testConfigMap(
	t *testing.T,
	adapterConfig config.AdapterConfig,
	digest string,
) *unstructured.Unstructured {
	t.Helper()
	data, err := json.Marshal(adapterConfig)
	if err != nil {
		t.Fatal(err)
	}
	return &unstructured.Unstructured{Object: map[string]any{
		"apiVersion": "v1",
		"kind":       "ConfigMap",
		"metadata": map[string]any{
			"name":      config.NameForUID(adapterConfig.Source.UID),
			"namespace": config.SystemNamespace,
			"labels": map[string]any{
				config.LabelAdapterConfig:   "true",
				config.LabelSourceUID:       adapterConfig.Source.UID,
				config.LabelSourceNamespace: adapterConfig.Source.Namespace,
				config.LabelSourceName:      adapterConfig.Source.Name,
			},
			"annotations": map[string]any{
				config.AnnotationConfigDigest: digest,
			},
		},
		"data": map[string]any{config.DataKey: string(data)},
	}}
}

func runtimeConfig(uid string) config.AdapterConfig {
	return config.AdapterConfig{
		Version: config.Version,
		Source: config.SourceReference{
			APIVersion: "sources.lampyr.dev/v1alpha1",
			Kind:       "AzureEventGridSource",
			Namespace:  "default",
			Name:       uid,
			UID:        uid,
		},
		Generation: 1,
		Webhook: &config.WebhookReference{
			Path:                    config.WebhookPath(uid),
			MaxConcurrentDeliveries: 1,
		},
		SinkURI: "http://sink",
	}
}

type testProcessor struct{}

func (testProcessor) ProcessWebhook(context.Context, config.AdapterConfig, []byte) error {
	return nil
}

func (testProcessor) Metrics() *Metrics {
	return NewMetrics()
}
