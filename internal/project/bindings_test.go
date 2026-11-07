// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package project

import (
	"reflect"
	"testing"
)

func TestBindingResolverKeepsPodNameserversWithoutSearchExpansion(t *testing.T) {
	source := "search default.svc.cluster.local svc.cluster.local\nnameserver 10.43.0.10\nnameserver fd00::53\noptions ndots:5 timeout:2\n"
	got, err := BindingResolver(source)
	want := "nameserver 10.43.0.10\nnameserver fd00::53\noptions ndots:1\n"
	if err != nil || got != want {
		t.Fatalf("binding resolver = %q, %v; want %q", got, err, want)
	}
	for _, invalid := range []string{"search cluster.local\n", "nameserver not-an-IP\n", "nameserver\n"} {
		if _, err := BindingResolver(invalid); err == nil {
			t.Fatalf("invalid resolver silently accepted: %q", invalid)
		}
	}
}

func TestBindingArgsUseSameContractForLocalAndDeployed(t *testing.T) {
	bindings := map[string]string{"processor": "processor"}
	for _, test := range []struct {
		local map[string]string
		url   string
		host  string
	}{
		{nil, "http://processor.default.svc.cluster.local", "processor.default.svc.cluster.local"},
		{map[string]string{"processor": "http://192.168.1.2:2345/_lampyr/token"}, "http://192.168.1.2:2345/_lampyr/token", "192.168.1.2"},
	} {
		args, err := BindingArgs(bindings, "default", test.local)
		want := []string{"--env", "LAMPYR_BINDING_PROCESSOR=" + test.url, "--net-allow", test.host}
		if err != nil || !reflect.DeepEqual(args, want) {
			t.Fatalf("args = %v, %v; want %v", args, err, want)
		}
	}
	if _, err := BindingArgs(bindings, "default", map[string]string{}); err == nil {
		t.Fatal("missing local target fell back to deployment")
	}
	for _, invalid := range []map[string]string{
		{"UPPERCASE": "processor"}, {"1name": "processor"}, {"bad-name": "processor"},
		{"name": "bad/path"}, {"name": ""},
	} {
		if _, err := BindingArgs(invalid, "default", nil); err == nil {
			t.Fatalf("invalid binding accepted: %v", invalid)
		}
	}
}
