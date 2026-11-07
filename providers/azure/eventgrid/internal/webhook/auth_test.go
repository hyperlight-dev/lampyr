// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package webhook

import "testing"

func TestTokenIsSourceScoped(t *testing.T) {
	key := []byte("01234567890123456789012345678901")
	first := Token(key, "source-a")
	if first == "" || first == Token(key, "source-b") {
		t.Fatalf("tokens are not source-scoped: %q", first)
	}
	if !Authenticate(key, "source-a", first) {
		t.Fatal("valid token was rejected")
	}
	if Authenticate(key, "source-b", first) {
		t.Fatal("token was accepted for another source")
	}
}
