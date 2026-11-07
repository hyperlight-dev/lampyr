// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package api

import (
	"encoding/json"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
)

const (
	GroupVersion = "sources.lampyr.dev/v1alpha1"
	Kind         = "AzureEventGridSource"
)

var SourceGVR = schema.GroupVersionResource{
	Group:    "sources.lampyr.dev",
	Version:  "v1alpha1",
	Resource: "azureeventgridsources",
}

type Source struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`
	Spec              SourceSpec   `json:"spec"`
	Status            SourceStatus `json:"status,omitempty"`
}

type SourceSpec struct {
	SourceResourceID  string                `json:"sourceResourceID"`
	EventSubscription EventSubscriptionSpec `json:"eventSubscription,omitempty"`
	Delivery          DeliverySpec          `json:"delivery,omitempty"`
	Sink              Destination           `json:"sink"`
	CEOverrides       *CloudEventOverrides  `json:"ceOverrides,omitempty"`
}

type EventSubscriptionSpec struct {
	IncludedEventTypes []string         `json:"includedEventTypes,omitempty"`
	SubjectBeginsWith  string           `json:"subjectBeginsWith,omitempty"`
	SubjectEndsWith    string           `json:"subjectEndsWith,omitempty"`
	AdvancedFilters    []AdvancedFilter `json:"advancedFilters,omitempty"`
	RetryPolicy        *RetryPolicy     `json:"retryPolicy,omitempty"`
}

type AdvancedFilter struct {
	OperatorType string `json:"operatorType"`
	Key          string `json:"key"`
	Values       []any  `json:"values,omitempty"`
}

type RetryPolicy struct {
	MaxDeliveryAttempts    int32 `json:"maxDeliveryAttempts,omitempty"`
	EventTimeToLiveMinutes int32 `json:"eventTimeToLiveMinutes,omitempty"`
}

type DeliverySpec struct {
	MaxConcurrentDeliveries int32 `json:"maxConcurrentDeliveries,omitempty"`
}

type Destination struct {
	Ref *KReference `json:"ref,omitempty"`
	URI string      `json:"uri,omitempty"`
}

type KReference struct {
	APIVersion string `json:"apiVersion"`
	Kind       string `json:"kind"`
	Namespace  string `json:"namespace,omitempty"`
	Name       string `json:"name"`
}

type CloudEventOverrides struct {
	Extensions map[string]any `json:"extensions,omitempty"`
}

type SourceStatus struct {
	ObservedGeneration int64                `json:"observedGeneration,omitempty"`
	SinkURI            string               `json:"sinkUri,omitempty"`
	AdapterConfig      *AdapterConfigStatus `json:"adapterConfig,omitempty"`
	AzureResources     *AzureResourceStatus `json:"azureResources,omitempty"`
	Conditions         []metav1.Condition   `json:"conditions,omitempty"`
}

type AdapterConfigStatus struct {
	Name      string `json:"name"`
	Namespace string `json:"namespace"`
	Version   string `json:"version"`
	Digest    string `json:"digest"`
}

type AzureResourceStatus struct {
	SourceUID                  string `json:"sourceUid"`
	PublisherResourceID        string `json:"publisherResourceId"`
	ForwardingSubscriptionName string `json:"forwardingSubscriptionName"`
}

func (source Source) DeepCopy() *Source {
	data, _ := json.Marshal(source)
	var copied Source
	_ = json.Unmarshal(data, &copied)
	return &copied
}
