// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

const maxWebhookBodyBytes = 1024 * 1024
const webhookKeyOverlap = 5 * time.Minute

const (
	webhookTokenHeader = "X-Lampyr-Token"
	frontDoorIDHeader  = "X-Azure-FDID"
)

type WebhookHandler struct {
	runtime     *Runtime
	authKeys    *RotatingWebhookKeys
	frontDoorID string
}

type RotatingWebhookKeys struct {
	load        func() ([]byte, error)
	mu          sync.Mutex
	currentKey  []byte
	previousKey []byte
	previousTo  time.Time
}

func NewRotatingWebhookKeys(
	load func() ([]byte, error),
) *RotatingWebhookKeys {
	return &RotatingWebhookKeys{load: load}
}

func (keys *RotatingWebhookKeys) Refresh() (string, error) {
	return keys.refreshAt(time.Now())
}

func (keys *RotatingWebhookKeys) refreshAt(now time.Time) (string, error) {
	authKey, err := keys.load()
	if err != nil {
		return "", err
	}
	if len(authKey) < 32 {
		return "", fmt.Errorf(
			"webhook authentication key must contain at least 32 bytes",
		)
	}
	keys.mu.Lock()
	defer keys.mu.Unlock()
	if len(keys.currentKey) == 0 {
		keys.currentKey = append([]byte(nil), authKey...)
	} else if !bytes.Equal(keys.currentKey, authKey) {
		keys.previousKey = keys.currentKey
		keys.previousTo = now.Add(webhookKeyOverlap)
		keys.currentKey = append([]byte(nil), authKey...)
	}
	return webhook.KeyDigest(authKey), nil
}

func (keys *RotatingWebhookKeys) authenticationKeys(
	now time.Time,
) ([][]byte, error) {
	keys.mu.Lock()
	defer keys.mu.Unlock()
	if len(keys.currentKey) == 0 {
		return nil, fmt.Errorf("webhook authentication key is unavailable")
	}
	current := [][]byte{append([]byte(nil), keys.currentKey...)}
	if len(keys.previousKey) != 0 && now.Before(keys.previousTo) {
		current = append(current, append([]byte(nil), keys.previousKey...))
	} else {
		keys.previousKey = nil
	}
	return current, nil
}

func NewWebhookHandler(
	runtime *Runtime,
	authKey []byte,
	frontDoorID string,
) *WebhookHandler {
	key := append([]byte(nil), authKey...)
	keys := NewRotatingWebhookKeys(
		func() ([]byte, error) {
			return append([]byte(nil), key...), nil
		},
	)
	_, _ = keys.Refresh()
	return NewWebhookHandlerWithKeys(runtime, keys, frontDoorID)
}

func NewWebhookHandlerWithKeys(
	runtime *Runtime,
	authKeys *RotatingWebhookKeys,
	frontDoorID string,
) *WebhookHandler {
	return &WebhookHandler{
		runtime:     runtime,
		authKeys:    authKeys,
		frontDoorID: strings.TrimSpace(frontDoorID),
	}
}

func (handler *WebhookHandler) ServeHTTP(
	response http.ResponseWriter,
	request *http.Request,
) {
	uid, found := sourceUID(request.URL.Path)
	if !found {
		http.NotFound(response, request)
		return
	}
	if handler.frontDoorID != "" &&
		!strings.EqualFold(
			strings.TrimSpace(request.Header.Get(frontDoorIDHeader)),
			handler.frontDoorID,
		) {
		http.Error(response, "forbidden", http.StatusForbidden)
		return
	}
	registration, found := handler.runtime.beginWebhook(uid)
	if !found || registration.config.Webhook == nil ||
		registration.config.Webhook.Path != request.URL.Path {
		if found {
			registration.active.Done()
		}
		http.NotFound(response, request)
		return
	}
	defer registration.active.Done()
	authKeys, err := handler.authKeys.authenticationKeys(time.Now())
	if err != nil {
		http.Error(
			response,
			"webhook authentication is unavailable",
			http.StatusServiceUnavailable,
		)
		return
	}
	authorized := false
	for _, authKey := range authKeys {
		if webhook.Authenticate(
			authKey,
			uid,
			request.Header.Get(webhookTokenHeader),
		) {
			authorized = true
			break
		}
	}
	if !authorized {
		http.Error(response, "unauthorized", http.StatusUnauthorized)
		return
	}
	switch request.Method {
	case http.MethodOptions:
		handleWebhookValidation(response, request)
	case http.MethodPost:
		handler.deliver(response, request, registration)
	default:
		response.Header().Set("Allow", "OPTIONS, POST")
		http.Error(response, "method not allowed", http.StatusMethodNotAllowed)
	}
}

func (handler *WebhookHandler) deliver(
	response http.ResponseWriter,
	request *http.Request,
	registration *webhookRegistration,
) {
	body, err := io.ReadAll(http.MaxBytesReader(
		response,
		request.Body,
		maxWebhookBodyBytes,
	))
	if err != nil {
		http.Error(response, "invalid request body", http.StatusRequestEntityTooLarge)
		return
	}
	events, err := webhookEvents(body)
	if err != nil {
		http.Error(response, err.Error(), http.StatusBadRequest)
		return
	}
	select {
	case registration.gate <- struct{}{}:
		defer func() { <-registration.gate }()
	case <-request.Context().Done():
		http.Error(response, "request cancelled", http.StatusRequestTimeout)
		return
	}
	for _, event := range events {
		if err := handler.runtime.processor.ProcessWebhook(
			request.Context(),
			registration.config,
			event,
		); err != nil {
			var invalid *InvalidEventError
			if errors.As(err, &invalid) {
				http.Error(response, invalid.Error(), http.StatusBadRequest)
				return
			}
			http.Error(response, "sink delivery failed", http.StatusInternalServerError)
			return
		}
	}
	response.WriteHeader(http.StatusNoContent)
}

func handleWebhookValidation(
	response http.ResponseWriter,
	request *http.Request,
) {
	origin := strings.TrimSpace(request.Header.Get("WebHook-Request-Origin"))
	if origin == "" {
		http.Error(response, "missing WebHook-Request-Origin", http.StatusBadRequest)
		return
	}
	response.Header().Set("WebHook-Allowed-Origin", origin)
	response.Header().Set("WebHook-Allowed-Rate", "*")
	response.WriteHeader(http.StatusOK)
}

func sourceUID(path string) (string, bool) {
	const prefix = "/sources/"
	if !strings.HasPrefix(path, prefix) {
		return "", false
	}
	uid := strings.TrimPrefix(path, prefix)
	if uid == "" || strings.Contains(uid, "/") {
		return "", false
	}
	return uid, true
}

func webhookEvents(body []byte) ([][]byte, error) {
	body = bytes.TrimSpace(body)
	if len(body) == 0 {
		return nil, fmt.Errorf("request body is empty")
	}
	if body[0] != '[' {
		var object map[string]any
		if err := json.Unmarshal(body, &object); err != nil {
			return nil, fmt.Errorf("decode CloudEvent: %w", err)
		}
		return [][]byte{append([]byte(nil), body...)}, nil
	}
	var batch []json.RawMessage
	if err := json.Unmarshal(body, &batch); err != nil {
		return nil, fmt.Errorf("decode CloudEvent batch: %w", err)
	}
	if len(batch) == 0 || len(batch) > 100 {
		return nil, fmt.Errorf("CloudEvent batch must contain between 1 and 100 events")
	}
	events := make([][]byte, 0, len(batch))
	for _, event := range batch {
		var object map[string]any
		if err := json.Unmarshal(event, &object); err != nil {
			return nil, fmt.Errorf("decode batched CloudEvent: %w", err)
		}
		events = append(events, append([]byte(nil), event...))
	}
	return events, nil
}
