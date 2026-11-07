// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package management

import (
	"regexp"
	"testing"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

func TestAllocateIsDeterministicAndUIDScoped(t *testing.T) {
	source := allocationSource("source-a")
	first, err := Allocate(source)
	if err != nil {
		t.Fatal(err)
	}
	second, err := Allocate(source.DeepCopy())
	if err != nil {
		t.Fatal(err)
	}
	if first != second {
		t.Fatalf("allocations differ: %#v != %#v", first, second)
	}
	pattern := regexp.MustCompile(`^[a-z]+-[a-f0-9]{16}$`)
	for label, name := range map[string]string{
		"forwarding": first.ForwardingSubscriptionName,
	} {
		if !pattern.MatchString(name) {
			t.Fatalf("%s name = %q", label, name)
		}
	}

	source.UID = types.UID("source-b")
	third, err := Allocate(source)
	if err != nil {
		t.Fatal(err)
	}
	if first.ForwardingSubscriptionName == third.ForwardingSubscriptionName {
		t.Fatalf("UID-scoped allocations collided: %#v and %#v", first, third)
	}
}

func TestAllocateRejectsMissingOwnershipInputs(t *testing.T) {
	source := allocationSource("")
	if _, err := Allocate(source); err == nil {
		t.Fatal("missing UID was accepted")
	}
	source = allocationSource("source-a")
	source.Spec.SourceResourceID = ""
	if _, err := Allocate(source); err == nil {
		t.Fatal("missing publisher was accepted")
	}
}

func allocationSource(uid string) *api.Source {
	return &api.Source{
		ObjectMeta: metav1.ObjectMeta{UID: types.UID(uid)},
		Spec: api.SourceSpec{
			SourceResourceID: "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/demo/providers/Microsoft.Storage/storageAccounts/uploads",
		},
	}
}
