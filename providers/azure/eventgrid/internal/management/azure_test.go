// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package management

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

func TestAzureClientCreatesOwnedResourceGraph(t *testing.T) {
	var (
		mu       sync.Mutex
		requests []recordedRequest
		created  map[string]any
	)
	publisherID := "/subscriptions/sub/resourceGroups/publishers/providers/Microsoft.Storage/storageAccounts/uploads"
	server := httptest.NewServer(http.HandlerFunc(func(response http.ResponseWriter, request *http.Request) {
		if request.Header.Get("Authorization") != "Bearer management-token" {
			http.Error(response, "missing token", http.StatusUnauthorized)
			return
		}
		var body map[string]any
		if request.Body != nil {
			_ = json.NewDecoder(request.Body).Decode(&body)
		}
		mu.Lock()
		requests = append(requests, recordedRequest{
			Method: request.Method,
			Path:   request.URL.Path,
			API:    request.URL.Query().Get("api-version"),
			Body:   body,
		})
		if request.Method == http.MethodPut {
			created = body
		}
		current := created
		mu.Unlock()
		switch request.Method {
		case http.MethodGet:
			if current == nil {
				http.NotFound(response, request)
				return
			}
			properties, _ := current["properties"].(map[string]any)
			labels, _ := properties["labels"].([]any)
			_ = json.NewEncoder(response).Encode(map[string]any{
				"properties": map[string]any{
					"labels":            labels,
					"provisioningState": "Succeeded",
				},
			})
		default:
			_ = json.NewEncoder(response).Encode(map[string]any{})
		}
	}))
	defer server.Close()
	client, err := newAzureClient(AzureOptions{
		SubscriptionID:         "sub",
		PublisherResourceGroup: "publishers",
		WebhookBaseURL:         "https://events.example.test",
		WebhookAuthKey:         []byte("01234567890123456789012345678901"),
		ManagementEndpoint:     server.URL,
	}, staticCredential{}, server.Client())
	if err != nil {
		t.Fatal(err)
	}

	source := azureSource(publisherID)
	allocation, err := Allocate(source)
	if err != nil {
		t.Fatal(err)
	}
	if err := client.EnsureForwardingSubscription(
		context.Background(),
		allocation,
		source,
	); err != nil {
		t.Fatal(err)
	}

	assertRequestBody(t, requests, "/eventSubscriptions/"+allocation.ForwardingSubscriptionName, func(body map[string]any) bool {
		properties, _ := body["properties"].(map[string]any)
		destination, _ := properties["destination"].(map[string]any)
		destinationProperties, _ := destination["properties"].(map[string]any)
		endpoint, _ := destinationProperties["endpointUrl"].(string)
		mappings, _ := destinationProperties["deliveryAttributeMappings"].([]any)
		if len(mappings) != 1 {
			return false
		}
		mapping, _ := mappings[0].(map[string]any)
		mappingProperties, _ := mapping["properties"].(map[string]any)
		parsed, err := url.Parse(endpoint)
		return err == nil &&
			destination["endpointType"] == "WebHook" &&
			parsed.Path == "/sources/"+allocation.SourceUID &&
			parsed.RawQuery == "" &&
			mapping["name"] == "X-Lampyr-Token" &&
			mapping["type"] == "Static" &&
			mappingProperties["isSecret"] == true &&
			mappingProperties["value"] == webhook.Token(
				[]byte("01234567890123456789012345678901"),
				allocation.SourceUID,
			)
	})
	foundPublisherScope := false
	for _, request := range requests {
		if request.Method == http.MethodPut &&
			strings.HasPrefix(
				request.Path,
				publisherID+"/providers/Microsoft.EventGrid/eventSubscriptions/",
			) {
			foundPublisherScope = true
		}
	}

	if !foundPublisherScope {
		t.Fatalf("event subscription was not created at publisher scope: %#v", requests)
	}
}

func TestAzureClientRefreshesSubscriptionsAfterWebhookKeyRotation(t *testing.T) {
	first := []byte("01234567890123456789012345678901")
	second := []byte("abcdefghijklmnopqrstuvwxyzABCDEF")
	currentKey := first
	var current map[string]any
	putCount := 0
	server := httptest.NewServer(http.HandlerFunc(
		func(response http.ResponseWriter, request *http.Request) {
			switch request.Method {
			case http.MethodGet:
				if current == nil {
					http.NotFound(response, request)
					return
				}
				properties := current["properties"].(map[string]any)
				_ = json.NewEncoder(response).Encode(map[string]any{
					"properties": map[string]any{
						"labels":            properties["labels"],
						"provisioningState": "Succeeded",
					},
				})
			case http.MethodPut:
				putCount++
				if err := json.NewDecoder(request.Body).Decode(&current); err != nil {
					t.Fatal(err)
				}
				_ = json.NewEncoder(response).Encode(map[string]any{})
			}
		},
	))
	defer server.Close()
	client, err := newAzureClient(AzureOptions{
		SubscriptionID:         "sub",
		PublisherResourceGroup: "publishers",
		WebhookBaseURL:         "https://events.example.test",
		WebhookAuthKeyLoader: func(context.Context) ([]byte, error) {
			return append([]byte(nil), currentKey...), nil
		},
		ManagementEndpoint: server.URL,
	}, staticCredential{}, server.Client())
	if err != nil {
		t.Fatal(err)
	}
	source := azureSource(
		"/subscriptions/sub/resourceGroups/publishers/providers/Microsoft.Storage/storageAccounts/uploads",
	)
	allocation, err := Allocate(source)
	if err != nil {
		t.Fatal(err)
	}
	if err := client.EnsureForwardingSubscription(
		context.Background(),
		allocation,
		source,
	); err != nil {
		t.Fatal(err)
	}
	if err := client.EnsureForwardingSubscription(
		context.Background(),
		allocation,
		source,
	); err != nil {
		t.Fatal(err)
	}
	if putCount != 1 {
		t.Fatalf("PUT count before rotation = %d", putCount)
	}
	currentKey = second
	if err := client.EnsureForwardingSubscription(
		context.Background(),
		allocation,
		source,
	); err != nil {
		t.Fatal(err)
	}
	if putCount != 2 {
		t.Fatalf("PUT count after rotation = %d", putCount)
	}
	properties := current["properties"].(map[string]any)
	labels := properties["labels"].([]any)
	wantDigest := "lampyr-webhook-key-sha256=" + webhook.KeyDigest(second)
	foundDigest := false
	for _, label := range labels {
		foundDigest = foundDigest || label == wantDigest
	}
	if !foundDigest {
		t.Fatalf("rotated key digest label missing: %#v", labels)
	}
	destination := properties["destination"].(map[string]any)
	destinationProperties := destination["properties"].(map[string]any)
	mappings := destinationProperties["deliveryAttributeMappings"].([]any)
	mapping := mappings[0].(map[string]any)
	mappingProperties := mapping["properties"].(map[string]any)
	if mappingProperties["value"] != webhook.Token(second, allocation.SourceUID) {
		t.Fatal("rotated subscription token was not updated")
	}
}

func TestDeleteForwardingSubscriptionRejectsOutsideScope(t *testing.T) {
	requested := false
	server := httptest.NewServer(http.HandlerFunc(
		func(response http.ResponseWriter, _ *http.Request) {
			requested = true
			response.WriteHeader(http.StatusNoContent)
		},
	))
	defer server.Close()
	client, err := newAzureClient(AzureOptions{
		SubscriptionID:         "sub",
		PublisherResourceGroup: "publishers",
		WebhookBaseURL:         "https://events.example.test",
		WebhookAuthKey:         []byte("01234567890123456789012345678901"),
		ManagementEndpoint:     server.URL,
	}, staticCredential{}, server.Client())
	if err != nil {
		t.Fatal(err)
	}
	err = client.DeleteForwardingSubscription(
		context.Background(),
		Allocation{
			PublisherResourceID:        "/subscriptions/sub/resourceGroups/other/providers/Microsoft.Storage/storageAccounts/uploads",
			ForwardingSubscriptionName: "forwarding",
		},
	)
	if err == nil || !strings.Contains(err.Error(), "outside allowed Azure scope") {
		t.Fatalf("scope error = %v", err)
	}
	if requested {
		t.Fatal("out-of-scope delete reached Azure")
	}
}

func TestDeterministicUUID(t *testing.T) {
	first := deterministicUUID("scope", "principal", "role")
	second := deterministicUUID("scope", "principal", "role")
	if first != second || len(first) != 36 {
		t.Fatalf("UUIDs = %q and %q", first, second)
	}
	if first == deterministicUUID("scope", "other", "role") {
		t.Fatal("different role assignment inputs collided")
	}
}

func TestAzureClientWaitsForOwnedSubscriptionProvisioning(t *testing.T) {
	publisherID := "/subscriptions/sub/resourceGroups/publishers/providers/Microsoft.Storage/storageAccounts/uploads"
	state := "Creating"
	putCount := 0
	server := httptest.NewServer(http.HandlerFunc(
		func(response http.ResponseWriter, request *http.Request) {
			switch request.Method {
			case http.MethodGet:
				_ = json.NewEncoder(response).Encode(map[string]any{
					"properties": map[string]any{
						"labels": []string{
							"lampyr-source-uid=11111111-2222-3333-4444-555555555555",
							"lampyr-source-generation=1",
							"lampyr-webhook-key-sha256=" + webhook.KeyDigest(
								[]byte("01234567890123456789012345678901"),
							),
						},
						"provisioningState": state,
					},
				})
			case http.MethodPut:
				putCount++
				_ = json.NewEncoder(response).Encode(map[string]any{})
			}
		},
	))
	defer server.Close()
	client, err := newAzureClient(AzureOptions{
		SubscriptionID:         "sub",
		PublisherResourceGroup: "publishers",
		WebhookBaseURL:         "https://events.example.test",
		WebhookAuthKey:         []byte("01234567890123456789012345678901"),
		ManagementEndpoint:     server.URL,
	}, staticCredential{}, server.Client())
	if err != nil {
		t.Fatal(err)
	}

	source := azureSource(publisherID)
	source.Generation = 1
	allocation, err := Allocate(source)
	if err != nil {
		t.Fatal(err)
	}
	err = client.EnsureForwardingSubscription(
		context.Background(),
		allocation,
		source,
	)
	if err == nil || !strings.Contains(err.Error(), "Creating") {
		t.Fatalf("provisioning error = %v", err)
	}
	state = "Succeeded"
	if err := client.EnsureForwardingSubscription(
		context.Background(),
		allocation,
		source,
	); err != nil {
		t.Fatal(err)
	}
	if putCount != 0 {
		t.Fatalf("owned current subscription was updated %d time(s)", putCount)
	}
}

func TestAzureClientRejectsPublisherOutsideAllowedScope(t *testing.T) {
	client, err := newAzureClient(AzureOptions{
		SubscriptionID:         "sub",
		PublisherResourceGroup: "allowed",
		WebhookBaseURL:         "https://events.example.test",
		WebhookAuthKey:         []byte("01234567890123456789012345678901"),
	}, staticCredential{}, http.DefaultClient)
	if err != nil {
		t.Fatal(err)
	}
	source := azureSource(
		"/subscriptions/sub/resourceGroups/other/providers/Microsoft.Storage/storageAccounts/uploads",
	)
	allocation, err := Allocate(source)
	if err != nil {
		t.Fatal(err)
	}
	err = client.EnsureForwardingSubscription(
		context.Background(),
		allocation,
		source,
	)
	if err == nil || !strings.Contains(err.Error(), "outside allowed Azure scope") {
		t.Fatalf("scope error = %v", err)
	}
}

func TestAzureClientBoundsSuccessfulResponseBodies(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(
		func(response http.ResponseWriter, _ *http.Request) {
			response.Header().Set("Content-Type", "application/json")
			_, _ = response.Write([]byte(`{"value":"`))
			_, _ = response.Write([]byte(strings.Repeat(
				"x",
				maxAzureResponseBytes,
			)))
			_, _ = response.Write([]byte(`"}`))
		},
	))
	defer server.Close()
	client, err := newAzureClient(AzureOptions{
		SubscriptionID:         "sub",
		PublisherResourceGroup: "publishers",
		WebhookBaseURL:         "https://events.example.test",
		WebhookAuthKey:         []byte("01234567890123456789012345678901"),
		ManagementEndpoint:     server.URL,
	}, staticCredential{}, server.Client())
	if err != nil {
		t.Fatal(err)
	}
	var result map[string]any
	err = client.get(context.Background(), "/resource", "test", &result)
	if err == nil || !strings.Contains(err.Error(), "response exceeds") {
		t.Fatalf("oversized response error = %v", err)
	}
}

func TestDefaultAzureHTTPClientHasBoundedTransport(t *testing.T) {
	client := azureHTTPClient()
	if client.Timeout != azureRequestTimeout {
		t.Fatalf("client timeout = %s", client.Timeout)
	}
	transport, ok := client.Transport.(*http.Transport)
	if !ok {
		t.Fatalf("transport = %T", client.Transport)
	}
	if transport.TLSHandshakeTimeout == 0 ||
		transport.ResponseHeaderTimeout == 0 {
		t.Fatalf("transport timeouts are not bounded: %#v", transport)
	}
}

type recordedRequest struct {
	Method string
	Path   string
	API    string
	Body   map[string]any
}

func assertRequestBody(
	t *testing.T,
	requests []recordedRequest,
	pathSuffix string,
	verify func(map[string]any) bool,
) {
	t.Helper()
	for _, request := range requests {
		if request.Method == http.MethodPut &&
			strings.HasSuffix(request.Path, pathSuffix) {
			if !verify(request.Body) {
				t.Fatalf("request %s body = %#v", request.Path, request.Body)
			}
			return
		}
	}
	t.Fatalf("PUT request ending in %q was not found: %#v", pathSuffix, requests)
}

type staticCredential struct{}

func (staticCredential) GetToken(
	context.Context,
	policy.TokenRequestOptions,
) (azcore.AccessToken, error) {
	return azcore.AccessToken{
		Token:     "management-token",
		ExpiresOn: time.Now().Add(time.Hour),
	}, nil
}

func azureSource(publisherID string) *api.Source {
	return &api.Source{
		ObjectMeta: metav1.ObjectMeta{
			UID: types.UID("11111111-2222-3333-4444-555555555555"),
		},
		Spec: api.SourceSpec{
			SourceResourceID: publisherID,
			EventSubscription: api.EventSubscriptionSpec{
				IncludedEventTypes: []string{"Microsoft.Storage.BlobCreated"},
			},
			Delivery: api.DeliverySpec{MaxConcurrentDeliveries: 5},
		},
	}
}
