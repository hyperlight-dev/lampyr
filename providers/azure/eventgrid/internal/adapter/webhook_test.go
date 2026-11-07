// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"bytes"
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/config"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

func TestWebhookHandlerValidatesAndDelivers(t *testing.T) {
	authKey := []byte("01234567890123456789012345678901")
	sink := &fakeSink{results: map[string]error{}}
	processor := NewProcessor(sink)
	adapterRuntime := NewRuntime(nil, processor)
	adapterConfig := webhookConfig("source-uid")
	adapterRuntime.registerWebhook(
		adapterConfig.Source.UID,
		"digest",
		adapterConfig,
	)
	handler := NewWebhookHandler(adapterRuntime, authKey, "front-door-id")
	endpoint := adapterConfig.Webhook.Path

	validation := httptest.NewRequest(http.MethodOptions, endpoint, nil)
	validation.Header.Set("WebHook-Request-Origin", "eventgrid.azure.net")
	authorizeWebhook(
		validation,
		authKey,
		adapterConfig.Source.UID,
		"front-door-id",
	)
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, validation)
	if response.Code != http.StatusOK ||
		response.Header().Get("WebHook-Allowed-Origin") != "eventgrid.azure.net" {
		t.Fatalf("validation response = %d %#v", response.Code, response.Header())
	}

	request := httptest.NewRequest(
		http.MethodPost,
		endpoint,
		eventBody("one"),
	)
	authorizeWebhook(
		request,
		authKey,
		adapterConfig.Source.UID,
		"front-door-id",
	)
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusNoContent {
		t.Fatalf("delivery response = %d: %s", response.Code, response.Body.String())
	}
	if len(sink.delivered) != 1 {
		t.Fatalf("delivered events = %d", len(sink.delivered))
	}
}

func TestWebhookHandlerRejectsUnauthorizedAndRetriesSinkFailures(t *testing.T) {
	authKey := []byte("01234567890123456789012345678901")
	sink := &fakeSink{results: map[string]error{"retry": errors.New("unavailable")}}
	processor := NewProcessor(sink)
	adapterRuntime := NewRuntime(nil, processor)
	adapterConfig := webhookConfig("source-uid")
	adapterRuntime.registerWebhook(
		adapterConfig.Source.UID,
		"digest",
		adapterConfig,
	)
	handler := NewWebhookHandler(adapterRuntime, authKey, "front-door-id")

	unauthorized := httptest.NewRequest(
		http.MethodPost,
		adapterConfig.Webhook.Path,
		eventBody("ignored"),
	)
	unauthorized.Header.Set(frontDoorIDHeader, "front-door-id")
	unauthorized.Header.Set(webhookTokenHeader, "wrong")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, unauthorized)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("unauthorized response = %d", response.Code)
	}

	retry := httptest.NewRequest(
		http.MethodPost,
		adapterConfig.Webhook.Path,
		eventBody("retry"),
	)
	authorizeWebhook(
		retry,
		authKey,
		adapterConfig.Source.UID,
		"front-door-id",
	)
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, retry)
	if response.Code != http.StatusInternalServerError {
		t.Fatalf("sink failure response = %d", response.Code)
	}
}

func TestWebhookHandlerRejectsWrongFrontDoor(t *testing.T) {
	authKey := []byte("01234567890123456789012345678901")
	adapterRuntime := NewRuntime(
		nil,
		NewProcessor(&fakeSink{}),
	)
	adapterConfig := webhookConfig("source-uid")
	adapterRuntime.registerWebhook(
		adapterConfig.Source.UID,
		"digest",
		adapterConfig,
	)
	handler := NewWebhookHandler(adapterRuntime, authKey, "expected")
	request := httptest.NewRequest(
		http.MethodPost,
		adapterConfig.Webhook.Path,
		eventBody("ignored"),
	)
	authorizeWebhook(request, authKey, adapterConfig.Source.UID, "other")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusForbidden {
		t.Fatalf("front door response = %d", response.Code)
	}
}

func TestWebhookHandlerReloadsRotatedAuthenticationKeys(t *testing.T) {
	first := []byte("01234567890123456789012345678901")
	second := []byte("abcdefghijklmnopqrstuvwxyzABCDEF")
	current := first
	adapterRuntime := NewRuntime(nil, NewProcessor(&fakeSink{}))
	adapterConfig := webhookConfig("source-uid")
	adapterRuntime.registerWebhook(
		adapterConfig.Source.UID,
		"digest",
		adapterConfig,
	)
	keys := NewRotatingWebhookKeys(
		func() ([]byte, error) {
			return append([]byte(nil), current...), nil
		},
	)
	if _, err := keys.Refresh(); err != nil {
		t.Fatal(err)
	}
	handler := NewWebhookHandlerWithKeys(
		adapterRuntime,
		keys,
		"front-door-id",
	)
	request := httptest.NewRequest(
		http.MethodOptions,
		adapterConfig.Webhook.Path,
		nil,
	)
	request.Header.Set("WebHook-Request-Origin", "eventgrid.azure.net")
	authorizeWebhook(request, first, adapterConfig.Source.UID, "front-door-id")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("initial key response = %d", response.Code)
	}

	current = second
	if _, err := keys.Refresh(); err != nil {
		t.Fatal(err)
	}
	request = httptest.NewRequest(
		http.MethodOptions,
		adapterConfig.Webhook.Path,
		nil,
	)
	request.Header.Set("WebHook-Request-Origin", "eventgrid.azure.net")
	authorizeWebhook(request, second, adapterConfig.Source.UID, "front-door-id")
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("rotated key response = %d", response.Code)
	}

	request = httptest.NewRequest(
		http.MethodOptions,
		adapterConfig.Webhook.Path,
		nil,
	)
	request.Header.Set("WebHook-Request-Origin", "eventgrid.azure.net")
	authorizeWebhook(request, first, adapterConfig.Source.UID, "front-door-id")
	response = httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("overlap key response = %d", response.Code)
	}
}

func TestRotatingWebhookKeysExpireWithoutWaitingForTraffic(t *testing.T) {
	first := []byte("01234567890123456789012345678901")
	second := []byte("abcdefghijklmnopqrstuvwxyzABCDEF")
	current := first
	keys := NewRotatingWebhookKeys(func() ([]byte, error) {
		return append([]byte(nil), current...), nil
	})
	rotation := time.Unix(1_000, 0)
	if _, err := keys.refreshAt(rotation); err != nil {
		t.Fatal(err)
	}
	current = second
	if _, err := keys.refreshAt(rotation.Add(time.Second)); err != nil {
		t.Fatal(err)
	}
	active, err := keys.authenticationKeys(
		rotation.Add(webhookKeyOverlap + 2*time.Second),
	)
	if err != nil {
		t.Fatal(err)
	}
	if len(active) != 1 || !bytes.Equal(active[0], second) {
		t.Fatalf("active keys after overlap = %q", active)
	}
}

func authorizeWebhook(
	request *http.Request,
	authKey []byte,
	uid string,
	frontDoorID string,
) {
	request.Header.Set(frontDoorIDHeader, frontDoorID)
	request.Header.Set(
		webhookTokenHeader,
		webhook.Token(authKey, uid),
	)
}

func webhookConfig(uid string) config.AdapterConfig {
	return config.AdapterConfig{
		Version: config.Version,
		Source: config.SourceReference{
			APIVersion: "sources.lampyr.dev/v1alpha1",
			Kind:       "AzureEventGridSource",
			Namespace:  "default",
			Name:       "events",
			UID:        uid,
		},
		Generation: 1,
		Webhook: &config.WebhookReference{
			Path:                    config.WebhookPath(uid),
			MaxConcurrentDeliveries: 1,
		},
		SinkURI: "http://sink",
	}
}

func eventBody(id string) *strings.Reader {
	return strings.NewReader(
		`{"specversion":"1.0","id":"` + id +
			`","source":"urn:test","type":"demo","data":{"ok":true}}`,
	)
}

type fakeSink struct {
	results   map[string]error
	delivered [][]byte
	mu        sync.Mutex
}

func (sink *fakeSink) Send(_ context.Context, _ string, event []byte) error {
	sink.mu.Lock()
	defer sink.mu.Unlock()
	sink.delivered = append(sink.delivered, append([]byte(nil), event...))
	for id, err := range sink.results {
		if bytes.Contains(event, []byte(`"id":"`+id+`"`)) {
			return err
		}
	}
	return nil
}
