// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"bytes"
	"context"
	"fmt"
	"net/http"
)

type HTTPSink struct {
	client *http.Client
}

func NewHTTPSink(client *http.Client) *HTTPSink {
	if client == nil {
		client = http.DefaultClient
	}
	return &HTTPSink{client: client}
}

func (sink *HTTPSink) Send(ctx context.Context, sinkURI string, event []byte) error {
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, sinkURI, bytes.NewReader(event))
	if err != nil {
		return err
	}
	request.Header.Set("Content-Type", "application/cloudevents+json")
	response, err := sink.client.Do(request)
	if err != nil {
		return err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		return fmt.Errorf("sink returned %s", response.Status)
	}
	return nil
}
