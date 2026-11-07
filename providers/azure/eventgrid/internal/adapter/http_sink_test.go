// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestHTTPSinkDoesNotExposeResponseBody(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, _ *http.Request) {
		http.Error(response, "secret-payload", http.StatusServiceUnavailable)
	}))
	defer server.Close()
	sink := NewHTTPSink(server.Client())
	err := sink.Send(
		context.Background(),
		server.URL,
		[]byte(`{"specversion":"1.0","id":"one","source":"urn:test","type":"demo"}`),
	)
	if err == nil {
		t.Fatal("sink failure was ignored")
	}
	if strings.Contains(err.Error(), "secret-payload") {
		t.Fatalf("error exposed response body: %v", err)
	}
}
