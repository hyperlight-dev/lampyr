// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package fixture

import "testing"

func TestDisabled(t *testing.T) {
	// ruleid: go-disabled-test
	t.Skip("later")
	// ruleid: go-disabled-test
	t.Skipf("%s", "later")
	// ruleid: go-disabled-test
	t.SkipNow()
}

func TestEnabled(t *testing.T) {
	// ok: go-disabled-test
	t.Log(`t.Skip("fixture")`)
}
