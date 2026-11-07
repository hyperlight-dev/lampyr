// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package management

import (
	"context"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

type Client interface {
	EnsureForwardingSubscription(context.Context, Allocation, *api.Source) error
	DeleteForwardingSubscription(context.Context, Allocation) error
}
