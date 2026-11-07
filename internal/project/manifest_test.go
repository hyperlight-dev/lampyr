// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package project

import (
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestManifestRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "lampyr.yaml")
	want := DefaultNode("hello-edge")

	if err := Write(path, want); err != nil {
		t.Fatal(err)
	}
	got, err := Load(path)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("manifest = %#v, want %#v", got, want)
	}
}

func TestManifestRejectsUnsupportedRuntime(t *testing.T) {
	for _, runtime := range []string{"unknown", "wasm"} {
		manifest := DefaultNode("hello-edge")
		manifest.Runtime = runtime
		err := manifest.Validate()
		if err == nil || !strings.Contains(err.Error(), "not supported") {
			t.Fatalf("Validate() for %q error = %v, want unsupported runtime", runtime, err)
		}
	}
}

func TestManifestGRPCProtocol(t *testing.T) {
	manifest := DefaultNode("greeter")
	manifest.Protocols = Protocols{HTTP: true, Streaming: true, GRPC: true}
	if err := manifest.Validate(); err != nil {
		t.Fatalf("Validate() rejected a gRPC service: %v", err)
	}
	manifest.Protocols.WebSocket = true
	if err := manifest.Validate(); err == nil || !strings.Contains(err.Error(), "protocols.grpc") {
		t.Fatalf("Validate() error = %v, want gRPC and WebSocket rejected together", err)
	}
}
