// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package api

import "testing"

func TestCloudEventIdentity(t *testing.T) {
	source, id, err := CloudEventIdentity([]byte(
		`{"specversion":"1.0","id":"event-1","source":"urn:test","type":"demo"}`,
	))
	if err != nil {
		t.Fatal(err)
	}
	if source != "urn:test" || id != "event-1" {
		t.Fatalf("identity = (%q, %q)", source, id)
	}
}

func TestValidateCloudEventRejectsInvalidAttributeTypes(t *testing.T) {
	for _, event := range []string{
		`{"specversion":"1.0","id":"one","source":"urn:test","type":123}`,
		`{"specversion":"1.0","id":"one","source":"urn:test","type":"demo","tenant":{"id":1}}`,
		`{"specversion":"1.0","id":"one","source":"urn:test","type":"demo","time":"not-a-time"}`,
	} {
		if err := ValidateCloudEvent([]byte(event)); err == nil {
			t.Fatalf("invalid CloudEvent was accepted: %s", event)
		}
	}
}
