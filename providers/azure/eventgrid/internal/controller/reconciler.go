// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package controller

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"slices"
	"strings"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/client-go/dynamic"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/config"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/management"
)

type DestinationResolver interface {
	Resolve(context.Context, string, api.Destination) (string, error)
}

type Reconciler struct {
	client   dynamic.Interface
	resolver DestinationResolver
	manager  management.Client
}

func (reconciler *Reconciler) WithManagementClient(
	manager management.Client,
) *Reconciler {
	reconciler.manager = manager
	return reconciler
}

const sourceFinalizer = "sources.lampyr.dev/azure-event-grid"

func NewReconciler(
	client dynamic.Interface,
	resolver DestinationResolver,
) *Reconciler {
	return &Reconciler{
		client:   client,
		resolver: resolver,
	}
}

func (reconciler *Reconciler) Reconcile(ctx context.Context, namespace, name string) error {
	resource := reconciler.client.Resource(api.SourceGVR).Namespace(namespace)
	object, err := resource.Get(ctx, name, metav1.GetOptions{})
	if apierrors.IsNotFound(err) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("get source: %w", err)
	}
	source, err := sourceFromUnstructured(object)
	if err != nil {
		return fmt.Errorf("decode source: %w", err)
	}
	if source.DeletionTimestamp != nil {
		return reconciler.finalizeSource(ctx, object, source)
	}
	object, source, err = reconciler.ensureFinalizer(ctx, object, source)
	if err != nil {
		return err
	}
	if err := api.ValidateSource(source); err != nil {
		status := buildStatus(source, "", nil,
			conditionValue{ConditionSinkResolved, metav1.ConditionFalse, "InvalidSpec", err.Error()},
			conditionValue{ConditionNetworkReady, metav1.ConditionFalse, "InvalidSpec", "Source validation must succeed before selecting a backend."},
			conditionValue{ConditionAzureResourcesReady, metav1.ConditionFalse, "InvalidSpec", "Source validation must succeed before provisioning resources."},
			conditionValue{ConditionConfigurationReady, metav1.ConditionFalse, "InvalidSpec", err.Error()},
			conditionValue{ConditionAdapterReady, metav1.ConditionFalse, "ConfigurationUnavailable", "The adapter has no valid source configuration."},
			conditionValue{ConditionReady, metav1.ConditionFalse, "InvalidSpec", "Correct the source specification."},
		)
		return reconciler.updateStatus(ctx, object, source, status)
	}

	sinkURI, err := reconciler.resolver.Resolve(ctx, source.Namespace, source.Spec.Sink)
	if err != nil {
		status := buildStatus(source, "", nil,
			conditionValue{ConditionSinkResolved, metav1.ConditionFalse, "SinkNotFound", err.Error()},
			conditionValue{ConditionNetworkReady, metav1.ConditionUnknown, "ResolutionPending", "Backend readiness is evaluated after the sink resolves."},
			conditionValue{ConditionAzureResourcesReady, metav1.ConditionUnknown, "ResolutionPending", "Resource readiness is evaluated after the sink resolves."},
			conditionValue{ConditionConfigurationReady, metav1.ConditionFalse, "SinkUnresolved", "A resolved sink URI is required for adapter configuration."},
			conditionValue{ConditionAdapterReady, metav1.ConditionFalse, "ConfigurationUnavailable", "The adapter has no source configuration."},
			conditionValue{ConditionReady, metav1.ConditionFalse, "SinkUnresolved", "Resolve the referenced Knative destination."},
		)
		if statusErr := reconciler.updateStatus(ctx, object, source, status); statusErr != nil {
			return errors.Join(err, statusErr)
		}
		return nil
	}

	if reconciler.manager == nil {
		status := buildStatus(source, sinkURI, nil,
			conditionValue{ConditionSinkResolved, metav1.ConditionTrue, "SinkResolved", "The sink address was resolved."},
			conditionValue{ConditionNetworkReady, metav1.ConditionFalse, "AzureBackendUnavailable", "No Event Grid backend is configured for this cluster."},
			conditionValue{ConditionAzureResourcesReady, metav1.ConditionFalse, "AzureBackendUnavailable", "No Event Grid backend is configured: install the Azure cluster with ly cluster install --provider azure --eventing."},
			conditionValue{ConditionConfigurationReady, metav1.ConditionFalse, "BackendUnavailable", "Adapter configuration is not published without the Azure Event Grid backend."},
			conditionValue{ConditionAdapterReady, metav1.ConditionFalse, "ConfigurationUnavailable", "The adapter has no source configuration."},
			conditionValue{ConditionReady, metav1.ConditionFalse, "BackendUnavailable", "Install the provider-specific Eventing source platform."},
		)
		return reconciler.updateStatus(ctx, object, source, status)
	}
	return reconciler.reconcileAzure(ctx, object, source, sinkURI)
}

func (reconciler *Reconciler) finalizeSource(
	ctx context.Context,
	object *unstructured.Unstructured,
	source *api.Source,
) error {
	if source.Status.AzureResources == nil {
		stopped, err := reconciler.stopAdapterConfig(ctx, source)
		if err != nil {
			return err
		}
		if !stopped {
			status := source.Status
			status.Conditions = append(
				[]metav1.Condition(nil),
				source.Status.Conditions...,
			)
			setFinalizingCondition(
				source,
				&status,
				"AdapterStopping",
				"Waiting for the adapter to stop receives and settle in-flight events.",
			)
			return reconciler.updateStatus(ctx, object, source, status)
		}
		return reconciler.removeFinalizer(ctx, object)
	}
	if reconciler.manager == nil {
		return fmt.Errorf(
			"Azure source cleanup cannot continue without the management client",
		)
	}
	allocation := allocationFromStatus(source.Status.AzureResources)
	if err := reconciler.manager.DeleteForwardingSubscription(ctx, allocation); err != nil {
		return fmt.Errorf("delete forwarding subscription: %w", err)
	}
	if source.Status.AdapterConfig != nil {
		stopped, err := reconciler.stopAdapterConfig(ctx, source)
		if err != nil {
			return err
		}
		status := source.Status
		status.Conditions = append(
			[]metav1.Condition(nil),
			source.Status.Conditions...,
		)
		if stopped {
			status.AdapterConfig = nil
		}
		setFinalizingCondition(
			source,
			&status,
			"AdapterStopping",
			"Upstream forwarding is disabled; waiting for in-flight webhook deliveries to finish.",
		)
		return reconciler.updateStatus(ctx, object, source, status)
	}
	return reconciler.removeFinalizer(ctx, object)
}

func (reconciler *Reconciler) stopAdapterConfig(
	ctx context.Context,
	source *api.Source,
) (bool, error) {
	resource := reconciler.client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	)
	name := config.NameForUID(string(source.UID))
	configMap, err := resource.Get(ctx, name, metav1.GetOptions{})
	if apierrors.IsNotFound(err) {
		return true, nil
	}
	if err != nil {
		return false, fmt.Errorf("get adapter config for shutdown: %w", err)
	}
	if configMap.GetLabels()[config.LabelSourceUID] != string(source.UID) {
		return false, fmt.Errorf("adapter config %s has conflicting ownership", name)
	}
	annotations := configMap.GetAnnotations()
	if annotations[config.AnnotationStopped] == "true" {
		if err := resource.Delete(ctx, name, metav1.DeleteOptions{}); err != nil &&
			!apierrors.IsNotFound(err) {
			return false, fmt.Errorf("delete stopped adapter config: %w", err)
		}
		return true, nil
	}
	if annotations == nil {
		annotations = make(map[string]string)
	}
	if annotations[config.AnnotationStopRequested] != "true" {
		updated := configMap.DeepCopy()
		annotations[config.AnnotationStopRequested] = "true"
		delete(annotations, config.AnnotationStopped)
		updated.SetAnnotations(annotations)
		if _, err := resource.Update(ctx, updated, metav1.UpdateOptions{}); err != nil {
			return false, fmt.Errorf("request adapter shutdown: %w", err)
		}
	}
	return false, nil
}

func allocationFromStatus(status *api.AzureResourceStatus) management.Allocation {
	return management.Allocation{
		SourceUID:                  status.SourceUID,
		PublisherResourceID:        status.PublisherResourceID,
		ForwardingSubscriptionName: status.ForwardingSubscriptionName,
	}
}

func setFinalizingCondition(
	source *api.Source,
	status *api.SourceStatus,
	reason string,
	message string,
) {
	condition := metav1.Condition{
		Type:               ConditionReady,
		Status:             metav1.ConditionFalse,
		ObservedGeneration: source.Generation,
		LastTransitionTime: metav1.Now(),
		Reason:             reason,
		Message:            message,
	}
	for index := range status.Conditions {
		if status.Conditions[index].Type == ConditionReady {
			if status.Conditions[index].Status == condition.Status {
				condition.LastTransitionTime = status.Conditions[index].LastTransitionTime
			}
			status.Conditions[index] = condition
			return
		}
	}
	status.Conditions = append(status.Conditions, condition)
}

func (reconciler *Reconciler) reconcileAzure(
	ctx context.Context,
	object *unstructured.Unstructured,
	source *api.Source,
	sinkURI string,
) error {
	allocation, err := management.Allocate(source)
	if err != nil {
		return err
	}
	resourceStatus := azureResourceStatus(allocation)
	if source.Status.AzureResources == nil {
		status := buildStatus(source, sinkURI, nil,
			conditionValue{ConditionSinkResolved, metav1.ConditionTrue, "SinkResolved", "The sink address was resolved."},
			conditionValue{ConditionNetworkReady, metav1.ConditionUnknown, "AllocationRecorded", "Restricted webhook readiness will be checked after the source allocation is persisted."},
			conditionValue{ConditionAzureResourcesReady, metav1.ConditionUnknown, "AllocationRecorded", "Deterministic source ownership was recorded before Azure resource creation."},
			conditionValue{ConditionConfigurationReady, metav1.ConditionFalse, "AzureResourcesPending", "Adapter configuration waits for the deterministic Azure allocation."},
			conditionValue{ConditionAdapterReady, metav1.ConditionFalse, "ConfigurationUnavailable", "The adapter has no source configuration."},
			conditionValue{ConditionReady, metav1.ConditionFalse, "AzureResourcesPending", "The source allocation is recorded; Azure provisioning is pending."},
		)
		status.AzureResources = resourceStatus
		return reconciler.updateStatus(ctx, object, source, status)
	}
	if !azureResourceStatusMatches(source.Status.AzureResources, allocation) {
		status := buildStatus(source, sinkURI, nil,
			conditionValue{ConditionSinkResolved, metav1.ConditionTrue, "SinkResolved", "The sink address was resolved."},
			conditionValue{ConditionNetworkReady, metav1.ConditionUnknown, "OwnershipConflict", "Webhook checks are blocked by conflicting allocation data."},
			conditionValue{ConditionAzureResourcesReady, metav1.ConditionFalse, "OwnershipConflict", "Recorded Azure resource ownership does not match the source UID and publisher."},
			conditionValue{ConditionConfigurationReady, metav1.ConditionFalse, "OwnershipConflict", "The controller will not adopt unverified Azure resources."},
			conditionValue{ConditionAdapterReady, metav1.ConditionFalse, "ConfigurationUnavailable", "The adapter has no verified source configuration."},
			conditionValue{ConditionReady, metav1.ConditionFalse, "OwnershipConflict", "Restore the recorded allocation or delete and recreate the source."},
		)
		status.AzureResources = source.Status.AzureResources
		return reconciler.updateStatus(ctx, object, source, status)
	}
	_, data, digest, err := config.BuildWebhook(source, sinkURI)
	if err != nil {
		return err
	}
	configMap, err := reconciler.ensureConfig(ctx, source, data, digest)
	if err != nil {
		return reconciler.updateAzureProvisioningFailure(
			ctx,
			object,
			source,
			sinkURI,
			resourceStatus,
			"ConfigWriteFailed",
			err,
		)
	}
	loaded := configMap.GetAnnotations()[config.AnnotationLoadedDigest] == digest
	runtimeReady := configMap.GetAnnotations()[config.AnnotationRuntimeReady] == "true"
	runtimeCode := configMap.GetAnnotations()[config.AnnotationRuntimeCode]
	runtimeReason := configMap.GetAnnotations()[config.AnnotationRuntimeReason]
	adapterCondition := conditionValue{ConditionAdapterReady, metav1.ConditionFalse, "ConfigPending", "Waiting for the shared adapter to register this webhook path."}
	readyCondition := conditionValue{ConditionReady, metav1.ConditionFalse, "AdapterPending", "The authenticated webhook is not registered yet."}
	if loaded && !runtimeReady && runtimeReason != "" {
		if runtimeCode == "" {
			runtimeCode = "DeliveryUnavailable"
		}
		adapterCondition = conditionValue{ConditionAdapterReady, metav1.ConditionFalse, runtimeCode, runtimeReason}
		readyCondition = conditionValue{ConditionReady, metav1.ConditionFalse, runtimeCode, "The adapter cannot accept Event Grid webhook deliveries."}
	}
	if loaded && runtimeReady {
		adapterCondition = conditionValue{ConditionAdapterReady, metav1.ConditionTrue, "WebhookRegistered", "The shared adapter registered the authenticated source webhook."}
		if err := reconciler.manager.EnsureForwardingSubscription(
			ctx,
			allocation,
			source,
		); err != nil {
			return reconciler.updateAzureProvisioningFailure(
				ctx,
				object,
				source,
				sinkURI,
				resourceStatus,
				azureProvisioningReason("ForwardingNotReady", err),
				err,
			)
		}
		readyCondition = conditionValue{ConditionReady, metav1.ConditionTrue, "Ready", "Event Grid is delivering directly to the authenticated source webhook."}
	}
	status := buildStatus(source, sinkURI, &api.AdapterConfigStatus{
		Name:      configMap.GetName(),
		Namespace: configMap.GetNamespace(),
		Version:   config.Version,
		Digest:    digest,
	},
		conditionValue{ConditionSinkResolved, metav1.ConditionTrue, "SinkResolved", "The sink address was resolved."},
		conditionValue{ConditionNetworkReady, metav1.ConditionTrue, "RestrictedPublicWebhook", "Azure Front Door terminates public HTTPS; the AKS origin requires this deployment's Front Door ID and a source-scoped secret delivery header."},
		conditionValue{ConditionAzureResourcesReady, boolCondition(loaded && runtimeReady), azureResourceReason(loaded, runtimeReady), azureResourceMessage(loaded, runtimeReady)},
		conditionValue{ConditionConfigurationReady, metav1.ConditionTrue, "ConfigPublished", "Source-UID-bound webhook configuration was published."},
		adapterCondition,
		readyCondition,
	)
	status.AzureResources = resourceStatus
	return reconciler.updateStatus(ctx, object, source, status)
}

func azureProvisioningReason(fallback string, err error) string {
	var responseErr *management.AzureResponseError
	if !errors.As(err, &responseErr) {
		return fallback
	}
	switch {
	case responseErr.StatusCode == 429 ||
		strings.EqualFold(responseErr.Code, "TooManyRequests"):
		return "AzureThrottled"
	case responseErr.StatusCode == 401 ||
		responseErr.StatusCode == 403 ||
		strings.Contains(strings.ToLower(responseErr.Code), "authorization"):
		return "AzureAuthorizationFailed"
	case responseErr.StatusCode == 409:
		return "AzureConflict"
	default:
		return fallback
	}
}

func (reconciler *Reconciler) updateAzureProvisioningFailure(
	ctx context.Context,
	object *unstructured.Unstructured,
	source *api.Source,
	sinkURI string,
	resourceStatus *api.AzureResourceStatus,
	reason string,
	provisioningErr error,
) error {
	status := buildStatus(source, sinkURI, nil,
		conditionValue{ConditionSinkResolved, metav1.ConditionTrue, "SinkResolved", "The sink address was resolved."},
		conditionValue{ConditionNetworkReady, metav1.ConditionUnknown, reason, "Restricted webhook readiness depends on successful Azure provisioning."},
		conditionValue{ConditionAzureResourcesReady, metav1.ConditionFalse, reason, provisioningErr.Error()},
		conditionValue{ConditionConfigurationReady, metav1.ConditionFalse, reason, "Webhook configuration is not ready."},
		conditionValue{ConditionAdapterReady, metav1.ConditionFalse, "ConfigurationUnavailable", "The adapter has no verified webhook configuration."},
		conditionValue{ConditionReady, metav1.ConditionFalse, reason, "Azure source provisioning has not completed."},
	)
	status.AzureResources = resourceStatus
	if err := reconciler.updateStatus(ctx, object, source, status); err != nil {
		return errors.Join(provisioningErr, err)
	}
	return nil
}

func azureResourceStatus(allocation management.Allocation) *api.AzureResourceStatus {
	return &api.AzureResourceStatus{
		SourceUID:                  allocation.SourceUID,
		PublisherResourceID:        allocation.PublisherResourceID,
		ForwardingSubscriptionName: allocation.ForwardingSubscriptionName,
	}
}

func azureResourceStatusMatches(
	status *api.AzureResourceStatus,
	allocation management.Allocation,
) bool {
	return status != nil &&
		status.SourceUID == allocation.SourceUID &&
		status.PublisherResourceID == allocation.PublisherResourceID &&
		status.ForwardingSubscriptionName == allocation.ForwardingSubscriptionName
}

func boolCondition(ready bool) metav1.ConditionStatus {
	if ready {
		return metav1.ConditionTrue
	}
	return metav1.ConditionUnknown
}

func azureResourceReason(loaded, runtimeReady bool) string {
	if loaded && runtimeReady {
		return "EventSubscriptionReady"
	}
	return "WebhookPending"
}

func azureResourceMessage(loaded, runtimeReady bool) string {
	if loaded && runtimeReady {
		return "The authenticated Event Grid subscription is ready."
	}
	return "Event Grid subscription creation waits until the adapter has registered the authenticated webhook."
}

func (reconciler *Reconciler) ensureFinalizer(
	ctx context.Context,
	object *unstructured.Unstructured,
	source *api.Source,
) (*unstructured.Unstructured, *api.Source, error) {
	if slices.Contains(source.Finalizers, sourceFinalizer) {
		return object, source, nil
	}
	updated := object.DeepCopy()
	finalizers := append([]string{}, source.Finalizers...)
	finalizers = append(finalizers, sourceFinalizer)
	updated.SetFinalizers(finalizers)
	persisted, err := reconciler.client.Resource(api.SourceGVR).Namespace(
		source.Namespace,
	).Update(ctx, updated, metav1.UpdateOptions{})
	if err != nil {
		return nil, nil, fmt.Errorf("add source finalizer: %w", err)
	}
	persistedSource, err := sourceFromUnstructured(persisted)
	if err != nil {
		return nil, nil, fmt.Errorf("decode finalized source: %w", err)
	}
	return persisted, persistedSource, nil
}

func (reconciler *Reconciler) removeFinalizer(
	ctx context.Context,
	object *unstructured.Unstructured,
) error {
	finalizers := object.GetFinalizers()
	index := slices.Index(finalizers, sourceFinalizer)
	if index < 0 {
		return nil
	}
	updated := object.DeepCopy()
	updated.SetFinalizers(slices.Delete(finalizers, index, index+1))
	_, err := reconciler.client.Resource(api.SourceGVR).Namespace(
		object.GetNamespace(),
	).Update(ctx, updated, metav1.UpdateOptions{})
	if err != nil && !apierrors.IsNotFound(err) {
		return fmt.Errorf("remove source finalizer: %w", err)
	}
	return nil
}

func (reconciler *Reconciler) ensureConfig(ctx context.Context, source *api.Source, data []byte, digest string) (*unstructured.Unstructured, error) {
	name := config.NameForUID(string(source.UID))
	resource := reconciler.client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	)
	existing, err := resource.Get(ctx, name, metav1.GetOptions{})
	if err != nil && !apierrors.IsNotFound(err) {
		return nil, fmt.Errorf("get adapter config: %w", err)
	}
	annotations := map[string]string{config.AnnotationConfigDigest: digest}
	if err == nil {
		if existing.GetLabels()[config.LabelSourceUID] != string(source.UID) {
			return nil, fmt.Errorf("adapter config %s is owned by another source", name)
		}
		if existing.GetLabels()[config.LabelSourceNamespace] != source.Namespace ||
			existing.GetLabels()[config.LabelSourceName] != source.Name {
			return nil, fmt.Errorf(
				"adapter config %s source identity labels do not match",
				name,
			)
		}
		existingData, _, _ := unstructured.NestedString(existing.Object, "data", config.DataKey)
		if existing.GetAnnotations()[config.AnnotationConfigDigest] == digest && existingData == string(data) {
			return existing, nil
		}
		if loaded := existing.GetAnnotations()[config.AnnotationLoadedDigest]; loaded != "" {
			annotations[config.AnnotationLoadedDigest] = loaded
		}
	}
	configMap := &unstructured.Unstructured{Object: map[string]any{
		"apiVersion": "v1",
		"kind":       "ConfigMap",
		"metadata": map[string]any{
			"name":      name,
			"namespace": config.SystemNamespace,
			"labels": map[string]any{
				config.LabelAdapterConfig:   "true",
				config.LabelSourceUID:       string(source.UID),
				config.LabelSourceNamespace: source.Namespace,
				config.LabelSourceName:      source.Name,
			},
			"annotations": stringMapToAny(annotations),
		},
		"data": map[string]any{config.DataKey: string(data)},
	}}
	if apierrors.IsNotFound(err) {
		created, createErr := resource.Create(ctx, configMap, metav1.CreateOptions{})
		if createErr != nil {
			return nil, fmt.Errorf("create adapter config: %w", createErr)
		}
		return created, nil
	}
	configMap.SetResourceVersion(existing.GetResourceVersion())
	updated, updateErr := resource.Update(ctx, configMap, metav1.UpdateOptions{})
	if updateErr != nil {
		return nil, fmt.Errorf("update adapter config: %w", updateErr)
	}
	return updated, nil
}

func (reconciler *Reconciler) updateStatus(ctx context.Context, object *unstructured.Unstructured, source *api.Source, status api.SourceStatus) error {
	if reflect.DeepEqual(source.Status, status) {
		return nil
	}
	statusObject, err := runtime.DefaultUnstructuredConverter.ToUnstructured(&status)
	if err != nil {
		return err
	}
	updated := object.DeepCopy()
	updated.Object["status"] = statusObject
	_, err = reconciler.client.Resource(api.SourceGVR).Namespace(source.Namespace).UpdateStatus(ctx, updated, metav1.UpdateOptions{})
	if err != nil {
		return fmt.Errorf("update source status: %w", err)
	}
	return nil
}

func sourceFromUnstructured(object *unstructured.Unstructured) (*api.Source, error) {
	data, err := json.Marshal(object.Object)
	if err != nil {
		return nil, err
	}
	var source api.Source
	if err := json.Unmarshal(data, &source); err != nil {
		return nil, err
	}
	return &source, nil
}

func stringMapToAny(values map[string]string) map[string]any {
	result := make(map[string]any, len(values))
	for key, value := range values {
		result[key] = value
	}
	return result
}
