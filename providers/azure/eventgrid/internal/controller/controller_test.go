// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package controller

import (
	"context"
	"testing"
)

func TestScheduleRetryDeduplicatesPendingKey(t *testing.T) {
	controller := &Controller{retries: make(map[sourceKey]*retryState)}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	keys := make(chan sourceKey, 1)
	key := sourceKey{namespace: "demo", name: "source"}

	controller.scheduleRetry(ctx, keys, key)
	controller.scheduleRetry(ctx, keys, key)

	controller.retryMu.Lock()
	state := controller.retries[key]
	controller.retryMu.Unlock()
	if state == nil || state.attempt != 1 || !state.scheduled {
		t.Fatalf("retry state = %#v", state)
	}
	controller.clearRetry(key)
}
