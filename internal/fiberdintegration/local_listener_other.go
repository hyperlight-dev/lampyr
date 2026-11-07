// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//go:build !linux && !darwin

package fiberdintegration

import (
	"context"
	"fmt"
)

func processOwnsLocalListener(context.Context, int, string) (bool, error) {
	return false, fmt.Errorf("local Fiber listener ownership is unsupported on this platform")
}
