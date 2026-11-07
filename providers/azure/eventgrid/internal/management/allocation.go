// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package management

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"strings"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

type Allocation struct {
	SourceUID                  string `json:"sourceUid"`
	PublisherResourceID        string `json:"publisherResourceId"`
	ForwardingSubscriptionName string `json:"forwardingSubscriptionName"`
}

func Allocate(source *api.Source) (Allocation, error) {
	if source == nil || source.UID == "" {
		return Allocation{}, fmt.Errorf("source UID is required for Azure allocation")
	}
	if strings.TrimSpace(source.Spec.SourceResourceID) == "" {
		return Allocation{}, fmt.Errorf("publisher resource ID is required for Azure allocation")
	}
	sum := sha256.Sum256([]byte(source.UID))
	suffix := hex.EncodeToString(sum[:])[:16]
	return Allocation{
		SourceUID:                  string(source.UID),
		PublisherResourceID:        source.Spec.SourceResourceID,
		ForwardingSubscriptionName: "forward-" + suffix,
	}, nil
}
