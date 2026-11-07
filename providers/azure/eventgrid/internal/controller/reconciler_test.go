// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package controller

import (
	"context"
	"encoding/json"
	"errors"
	"slices"
	"strings"
	"testing"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/dynamic/fake"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/config"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/management"
)

func TestAzureReconcilePersistsAllocationBeforeProvisioning(t *testing.T) {
	source := azureTestSource()
	client := fake.NewSimpleDynamicClientWithCustomListKinds(runtime.NewScheme(), map[schema.GroupVersionResource]string{
		api.SourceGVR:    "AzureEventGridSourceList",
		config.ConfigGVR: "ConfigMapList",
	}, source)
	manager := &fakeManagementClient{}
	reconciler := NewReconciler(
		client,
		staticResolver("http://broker.demo.svc.cluster.local"),
	).WithManagementClient(manager)

	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	if len(manager.calls) != 0 {
		t.Fatalf("Azure resources were created before allocation status: %v", manager.calls)
	}
	updated, err := client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	allocationStatus, found, err := unstructured.NestedMap(
		updated.Object,
		"status",
		"azureResources",
	)
	if err != nil || !found {
		t.Fatalf("allocation status = %#v, found=%t, err=%v", allocationStatus, found, err)
	}

	reconciler = NewReconciler(
		client,
		staticResolver("http://broker.demo.svc.cluster.local"),
	).WithManagementClient(manager)
	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	allocation, err := management.Allocate(sourceFromTestObject(t, updated))
	if err != nil {
		t.Fatal(err)
	}
	configMap, err := client.Resource(config.ConfigGVR).Namespace(config.SystemNamespace).Get(
		context.Background(),
		config.NameForUID(allocation.SourceUID),
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	rawConfig, _, _ := unstructured.NestedString(configMap.Object, "data", config.DataKey)
	var adapterConfig config.AdapterConfig
	if err := json.Unmarshal([]byte(rawConfig), &adapterConfig); err != nil {
		t.Fatal(err)
	}
	if adapterConfig.Webhook == nil ||
		adapterConfig.Webhook.Path != config.WebhookPath(allocation.SourceUID) {
		t.Fatalf("Azure webhook config = %#v", adapterConfig.Webhook)
	}
	markAdapterConfigReady(t, client, configMap)
	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	if wantCalls := []string{"forwarding"}; !slices.Equal(manager.calls, wantCalls) {
		t.Fatalf("management calls = %v, want %v", manager.calls, wantCalls)
	}
}

func TestAzureReconcileRejectsConflictingAllocation(t *testing.T) {
	source := azureTestSource()
	source.Object["status"] = map[string]any{
		"azureResources": map[string]any{
			"sourceUid":                  string(source.GetUID()),
			"publisherResourceId":        "/subscriptions/other/resource",
			"forwardingSubscriptionName": "unverified",
		},
	}
	source.SetFinalizers([]string{sourceFinalizer})
	client := fake.NewSimpleDynamicClientWithCustomListKinds(runtime.NewScheme(), map[schema.GroupVersionResource]string{
		api.SourceGVR:    "AzureEventGridSourceList",
		config.ConfigGVR: "ConfigMapList",
	}, source)
	manager := &fakeManagementClient{}
	reconciler := NewReconciler(
		client,
		staticResolver("http://broker.demo.svc.cluster.local"),
	).WithManagementClient(manager)

	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	if len(manager.calls) != 0 {
		t.Fatalf("conflicting resources were adopted: %v", manager.calls)
	}
	updated, err := client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	status, _, _ := unstructured.NestedMap(updated.Object, "status")
	assertCondition(t, status, ConditionAzureResourcesReady, metav1.ConditionFalse)
}

func TestAzureReconcileStopsBeforeForwardingOnPartialFailure(t *testing.T) {
	source := azureTestSource()
	client := fake.NewSimpleDynamicClientWithCustomListKinds(runtime.NewScheme(), map[schema.GroupVersionResource]string{
		api.SourceGVR:    "AzureEventGridSourceList",
		config.ConfigGVR: "ConfigMapList",
	}, source)
	manager := &fakeManagementClient{
		failAt: "forwarding",
	}

	reconciler := NewReconciler(
		client,
		staticResolver("http://broker.demo.svc.cluster.local"),
	).WithManagementClient(manager)
	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	updated, err := client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	configMap, err := client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Get(
		context.Background(),
		config.NameForUID(string(source.GetUID())),
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	markAdapterConfigReady(t, client, configMap)
	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	if want := []string{"forwarding"}; !slices.Equal(manager.calls, want) {
		t.Fatalf("management calls = %v, want %v", manager.calls, want)
	}
	updated, err = client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	status, _, _ := unstructured.NestedMap(updated.Object, "status")
	assertCondition(t, status, ConditionAzureResourcesReady, metav1.ConditionFalse)

	manager.calls = nil
	manager.failAt = ""
	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	if want := []string{"forwarding"}; !slices.Equal(manager.calls, want) {
		t.Fatalf("retry management calls = %v, want %v", manager.calls, want)
	}
}

func TestAzureProvisioningReason(t *testing.T) {
	tests := []struct {
		err  error
		want string
	}{
		{
			err: &management.AzureResponseError{
				StatusCode: 429,
				Code:       "TooManyRequests",
			},
			want: "AzureThrottled",
		},
		{
			err: &management.AzureResponseError{
				StatusCode: 403,
				Code:       "AuthorizationFailed",
			},
			want: "AzureAuthorizationFailed",
		},
		{
			err:  &management.AzureResponseError{StatusCode: 409},
			want: "AzureConflict",
		},
		{err: errors.New("other"), want: "Fallback"},
	}
	for _, test := range tests {
		if got := azureProvisioningReason("Fallback", test.err); got != test.want {
			t.Fatalf("reason = %q, want %q", got, test.want)
		}
	}
}

func TestAzureFinalizationStopsWebhookBeforeRemovingFinalizer(t *testing.T) {
	now := metav1.Now()
	source := azureTestSource()
	source.SetDeletionTimestamp(&now)
	source.SetFinalizers([]string{sourceFinalizer})
	allocation, err := management.Allocate(sourceFromTestObject(t, source))
	if err != nil {
		t.Fatal(err)
	}
	source.Object["status"] = map[string]any{
		"adapterConfig": map[string]any{
			"name":      config.NameForUID(allocation.SourceUID),
			"namespace": config.SystemNamespace,
			"version":   config.Version,
			"digest":    "digest",
		},
		"azureResources": map[string]any{
			"sourceUid":                  allocation.SourceUID,
			"publisherResourceId":        allocation.PublisherResourceID,
			"forwardingSubscriptionName": allocation.ForwardingSubscriptionName,
		},
	}
	configMap := &unstructured.Unstructured{Object: map[string]any{
		"apiVersion": "v1",
		"kind":       "ConfigMap",
		"metadata": map[string]any{
			"name":      config.NameForUID(allocation.SourceUID),
			"namespace": config.SystemNamespace,
			"labels": map[string]any{
				config.LabelSourceUID: allocation.SourceUID,
			},
		},
	}}
	client := fake.NewSimpleDynamicClientWithCustomListKinds(runtime.NewScheme(), map[schema.GroupVersionResource]string{
		api.SourceGVR:    "AzureEventGridSourceList",
		config.ConfigGVR: "ConfigMapList",
	}, source, configMap)
	manager := &fakeManagementClient{}
	reconciler := NewReconciler(client, staticResolver("http://unused")).
		WithManagementClient(manager)

	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	updated, err := client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, found, _ := unstructured.NestedMap(updated.Object, "status", "adapterConfig"); !found {
		t.Fatal("adapter configuration was removed before stop acknowledgement")
	}
	stopping, err := client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Get(
		context.Background(),
		config.NameForUID(allocation.SourceUID),
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	annotations := stopping.GetAnnotations()
	if annotations[config.AnnotationStopRequested] != "true" {
		t.Fatalf("adapter stop was not requested: %#v", annotations)
	}
	annotations[config.AnnotationStopped] = "true"
	stopping.SetAnnotations(annotations)
	if _, err := client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Update(context.Background(), stopping, metav1.UpdateOptions{}); err != nil {
		t.Fatal(err)
	}

	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	updated, err = client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	if _, found, _ := unstructured.NestedMap(updated.Object, "status", "adapterConfig"); found {
		t.Fatal("stopped adapter configuration remained in status")
	}

	if err := reconciler.Reconcile(context.Background(), "demo", "uploads"); err != nil {
		t.Fatal(err)
	}
	wantCalls := []string{
		"delete-forwarding",
		"delete-forwarding",
		"delete-forwarding",
	}
	if !slices.Equal(manager.calls, wantCalls) {
		t.Fatalf("cleanup calls = %v, want %v", manager.calls, wantCalls)
	}
	finalized, err := client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil && !apierrors.IsNotFound(err) {
		t.Fatal(err)
	}
	if err == nil && slices.Contains(finalized.GetFinalizers(), sourceFinalizer) {
		t.Fatalf("source finalizer remains: %v", finalized.GetFinalizers())
	}
}

func TestAzureFinalizationRetainsFinalizerOnCleanupFailure(t *testing.T) {
	now := metav1.Now()
	source := azureTestSource()
	source.SetDeletionTimestamp(&now)
	source.SetFinalizers([]string{sourceFinalizer})
	allocation, err := management.Allocate(sourceFromTestObject(t, source))
	if err != nil {
		t.Fatal(err)
	}
	source.Object["status"] = map[string]any{
		"azureResources": map[string]any{
			"sourceUid":                  allocation.SourceUID,
			"publisherResourceId":        allocation.PublisherResourceID,
			"forwardingSubscriptionName": allocation.ForwardingSubscriptionName,
		},
	}
	client := fake.NewSimpleDynamicClientWithCustomListKinds(runtime.NewScheme(), map[schema.GroupVersionResource]string{
		api.SourceGVR:    "AzureEventGridSourceList",
		config.ConfigGVR: "ConfigMapList",
	}, source)
	manager := &fakeManagementClient{failAt: "delete-forwarding"}
	reconciler := NewReconciler(client, staticResolver("http://unused")).
		WithManagementClient(manager)

	err = reconciler.Reconcile(context.Background(), "demo", "uploads")
	if err == nil || !strings.Contains(err.Error(), "delete forwarding subscription") {
		t.Fatalf("Reconcile() = %v, want forwarding deletion failure", err)
	}
	updated, err := client.Resource(api.SourceGVR).Namespace("demo").Get(
		context.Background(),
		"uploads",
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Contains(updated.GetFinalizers(), sourceFinalizer) {
		t.Fatalf("cleanup failure removed finalizer: %v", updated.GetFinalizers())
	}
}

func assertCondition(t *testing.T, status map[string]any, conditionType string, want metav1.ConditionStatus) {
	t.Helper()
	conditions, _, _ := unstructured.NestedSlice(status, "conditions")
	for _, item := range conditions {
		condition := item.(map[string]any)
		if condition["type"] == conditionType {
			if condition["status"] != string(want) {
				t.Fatalf("%s status = %#v, want %s", conditionType, condition["status"], want)
			}
			return
		}
	}
	t.Fatalf("condition %s not found in %#v", conditionType, conditions)
}

type staticResolver string

func (s staticResolver) Resolve(context.Context, string, api.Destination) (string, error) {
	return string(s), nil
}

func azureTestSource() *unstructured.Unstructured {
	source := &unstructured.Unstructured{Object: map[string]any{
		"apiVersion": api.GroupVersion,
		"kind":       api.Kind,
		"metadata": map[string]any{
			"name":       "uploads",
			"namespace":  "demo",
			"uid":        "11111111-2222-3333-4444-555555555555",
			"generation": int64(3),
		},
		"spec": map[string]any{
			"sourceResourceID": "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/demo/providers/Microsoft.Storage/storageAccounts/uploads",
			"sink":             map[string]any{"uri": "http://broker.demo.svc.cluster.local"},
		},
	}}
	return source
}

func sourceFromTestObject(t *testing.T, object *unstructured.Unstructured) *api.Source {
	t.Helper()
	source, err := sourceFromUnstructured(object)
	if err != nil {
		t.Fatal(err)
	}
	return source
}

func markAdapterConfigReady(
	t *testing.T,
	client dynamic.Interface,
	configMap *unstructured.Unstructured,
) {
	t.Helper()
	updated := configMap.DeepCopy()
	annotations := updated.GetAnnotations()
	if annotations == nil {
		annotations = make(map[string]string)
	}
	annotations[config.AnnotationLoadedDigest] =
		annotations[config.AnnotationConfigDigest]
	annotations[config.AnnotationRuntimeReady] = "true"
	updated.SetAnnotations(annotations)
	if _, err := client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Update(context.Background(), updated, metav1.UpdateOptions{}); err != nil {
		t.Fatal(err)
	}
}

type fakeManagementClient struct {
	calls  []string
	failAt string
}

func (client *fakeManagementClient) EnsureForwardingSubscription(
	context.Context,
	management.Allocation,
	*api.Source,
) error {
	client.calls = append(client.calls, "forwarding")
	if client.failAt == "forwarding" {
		return errors.New("forwarding failed")
	}
	return nil
}

func (client *fakeManagementClient) DeleteForwardingSubscription(
	context.Context,
	management.Allocation,
) error {
	client.calls = append(client.calls, "delete-forwarding")
	if client.failAt == "delete-forwarding" {
		return errors.New("delete forwarding failed")
	}
	return nil
}
