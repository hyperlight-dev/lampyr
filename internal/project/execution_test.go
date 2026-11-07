// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package project

import (
	"strings"
	"testing"
)

func TestExecutionPolicyDefaultsAndValidation(t *testing.T) {
	for _, test := range []struct {
		name       string
		policy     *ExecutionPolicy
		mode, idle string
		err        string
	}{
		{"default", nil, ExecutionFresh, "", ""},
		{"explicit-fresh", &ExecutionPolicy{Mode: ExecutionFresh}, ExecutionFresh, "", ""},
		{"reuse-default", &ExecutionPolicy{Mode: ExecutionReuse}, ExecutionReuse, "30s", ""},
		{"reuse-idle", &ExecutionPolicy{Mode: ExecutionReuse, IdleTimeout: "250ms"}, ExecutionReuse, "250ms", ""},
		{"fresh-idle", &ExecutionPolicy{Mode: ExecutionFresh, IdleTimeout: "30s"}, "", "", "only valid with mode reuse"},
		{"invalid-mode", &ExecutionPolicy{Mode: "session"}, "", "", "execution.mode"},
		{"zero-idle", &ExecutionPolicy{Mode: ExecutionReuse, IdleTimeout: "0s"}, "", "", "positive duration"},
		{"negative-idle", &ExecutionPolicy{Mode: ExecutionReuse, IdleTimeout: "-1s"}, "", "", "positive duration"},
		{"invalid-idle", &ExecutionPolicy{Mode: ExecutionReuse, IdleTimeout: "soon"}, "", "", "positive duration"},
		{"no-park", &ExecutionPolicy{Mode: ExecutionReuse, IdleAction: "park"}, "", "", "park/resume is not supported"},
	} {
		t.Run(test.name, func(t *testing.T) {
			manifest := DefaultNode("execution")
			manifest.Execution = test.policy
			got, err := manifest.EffectiveExecution()
			if test.err != "" {
				if err == nil || !strings.Contains(err.Error(), test.err) || manifest.Validate() == nil {
					t.Fatalf("invalid policy was accepted: %#v, %v", got, err)
				}
				return
			}
			if err != nil || got.Mode != test.mode || got.IdleTimeout != test.idle {
				t.Fatalf("effective policy = %#v, %v", got, err)
			}
			if err := manifest.Validate(); err != nil {
				t.Fatal(err)
			}
		})
	}
}
