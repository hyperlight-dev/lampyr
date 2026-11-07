// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package api

import (
	"strings"
	"testing"
)

func TestValidateSource(t *testing.T) {
	valid := Source{
		Spec: SourceSpec{
			SourceResourceID: "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/demo/providers/Microsoft.Storage/storageAccounts/uploads",
			Sink: Destination{Ref: &KReference{
				APIVersion: "eventing.knative.dev/v1",
				Kind:       "Broker",
				Name:       "default",
			}},
			CEOverrides: &CloudEventOverrides{
				Extensions: map[string]any{"environment": "test"},
			},
		},
	}

	if err := ValidateSource(&valid); err != nil {
		t.Fatalf("ValidateSource(valid) = %v", err)
	}

	tests := []struct {
		name   string
		mutate func(*Source)
		want   string
	}{
		{
			name: "publisher is required",
			mutate: func(source *Source) {
				source.Spec.SourceResourceID = ""
			},
			want: "sourceResourceID",
		},
		{
			name: "sink is required",
			mutate: func(source *Source) {
				source.Spec.Sink = Destination{}
			},
			want: "sink",
		},
		{
			name: "ref URI cannot replace the sink host",
			mutate: func(source *Source) {
				source.Spec.Sink.URI = "https://example.test/events"
			},
			want: "absolute URI is not allowed",
		},
		{
			name: "publisher must be a resource ID",
			mutate: func(source *Source) {
				source.Spec.SourceResourceID = "uploads"
			},
			want: "fully qualified Azure resource ID",
		},
		{
			name: "publisher must be a storage account",
			mutate: func(source *Source) {
				source.Spec.SourceResourceID = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/demo/providers/Microsoft.EventGrid/topics/uploads"
			},
			want: "only Azure Storage account publishers",
		},
		{
			name: "reserved override is rejected",
			mutate: func(source *Source) {
				source.Spec.CEOverrides.Extensions = map[string]any{"type": "changed"}
			},
			want: "extension",
		},
		{
			name: "delivery concurrency is bounded",
			mutate: func(source *Source) {
				source.Spec.Delivery.MaxConcurrentDeliveries = 33
			},
			want: "maxConcurrentDeliveries",
		},
		{
			name: "forwarding attempts are bounded",
			mutate: func(source *Source) {
				source.Spec.EventSubscription.RetryPolicy = &RetryPolicy{
					MaxDeliveryAttempts: 31,
				}
			},
			want: "maxDeliveryAttempts",
		},
		{
			name: "forwarding TTL is bounded",
			mutate: func(source *Source) {
				source.Spec.EventSubscription.RetryPolicy = &RetryPolicy{
					EventTimeToLiveMinutes: 1441,
				}
			},
			want: "eventTimeToLiveMinutes",
		},
		{
			name: "advanced filter operator is validated",
			mutate: func(source *Source) {
				source.Spec.EventSubscription.AdvancedFilters = []AdvancedFilter{{
					OperatorType: "NumberGreaterThan",
					Key:          "data.size",
					Values:       []any{1, 2},
				}}
			},
			want: "requires exactly one value",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			source := valid.DeepCopy()
			test.mutate(source)
			err := ValidateSource(source)
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("ValidateSource() = %v, want error containing %q", err, test.want)
			}
		})
	}
}

func TestValidateExtensionName(t *testing.T) {
	for _, name := range []string{"environment", "traceparent", "tenant1"} {
		if err := ValidateExtensionName(name); err != nil {
			t.Errorf("ValidateExtensionName(%q) = %v", name, err)
		}
	}
	for _, name := range []string{"id", "Data", "bad-name", "locktoken"} {
		if err := ValidateExtensionName(name); err == nil {
			t.Errorf("ValidateExtensionName(%q) unexpectedly succeeded", name)
		}
	}
}
