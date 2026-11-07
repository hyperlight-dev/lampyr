// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package controller

import (
	"testing"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

func TestBuildStatusPreservesProviderCleanupState(t *testing.T) {
	source := &api.Source{
		ObjectMeta: metav1.ObjectMeta{Generation: 2},
		Status: api.SourceStatus{
			AdapterConfig: &api.AdapterConfigStatus{Name: "adapter"},
			AzureResources: &api.AzureResourceStatus{
				PublisherResourceID: "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Storage/storageAccounts/account",
			},
		},
	}
	status := buildStatus(source, "", nil)
	if status.AdapterConfig != source.Status.AdapterConfig {
		t.Fatal("adapter cleanup state was not preserved")
	}
	if status.AzureResources != source.Status.AzureResources {
		t.Fatal("Azure cleanup state was not preserved")
	}
}
