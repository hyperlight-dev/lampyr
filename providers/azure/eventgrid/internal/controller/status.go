// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package controller

import (
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

const (
	ConditionSinkResolved        = "SinkResolved"
	ConditionNetworkReady        = "NetworkReady"
	ConditionAzureResourcesReady = "AzureResourcesReady"
	ConditionConfigurationReady  = "ConfigurationReady"
	ConditionAdapterReady        = "AdapterReady"
	ConditionReady               = "Ready"
)

type conditionValue struct {
	Type    string
	Status  metav1.ConditionStatus
	Reason  string
	Message string
}

func buildStatus(source *api.Source, sinkURI string, adapterConfig *api.AdapterConfigStatus, values ...conditionValue) api.SourceStatus {
	now := metav1.NewTime(time.Now().UTC())
	previous := make(map[string]metav1.Condition, len(source.Status.Conditions))
	for _, condition := range source.Status.Conditions {
		previous[condition.Type] = condition
	}
	conditions := make([]metav1.Condition, 0, len(values))
	for _, value := range values {
		transition := now
		if old, found := previous[value.Type]; found && old.Status == value.Status {
			transition = old.LastTransitionTime
		}
		conditions = append(conditions, metav1.Condition{
			Type:               value.Type,
			Status:             value.Status,
			ObservedGeneration: source.Generation,
			LastTransitionTime: transition,
			Reason:             value.Reason,
			Message:            value.Message,
		})
	}
	if adapterConfig == nil {
		adapterConfig = source.Status.AdapterConfig
	}
	return api.SourceStatus{
		ObservedGeneration: source.Generation,
		SinkURI:            sinkURI,
		AdapterConfig:      adapterConfig,
		AzureResources:     source.Status.AzureResources,
		Conditions:         conditions,
	}
}
