// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package management

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	"github.com/Azure/azure-sdk-for-go/sdk/azcore"
	"github.com/Azure/azure-sdk-for-go/sdk/azcore/policy"
	"github.com/Azure/azure-sdk-for-go/sdk/azidentity"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

const (
	armTokenScope              = "https://management.azure.com/.default"
	eventGridManagementVersion = "2025-02-15"
	azureRequestTimeout        = 30 * time.Second
	maxAzureResponseBytes      = 1 << 20
)

var errResourceNotFound = errors.New("Azure resource not found")

type AzureOptions struct {
	SubscriptionID         string
	PublisherResourceGroup string
	WebhookBaseURL         string
	WebhookAuthKey         []byte
	WebhookAuthKeyLoader   func(context.Context) ([]byte, error)
	ManagementEndpoint     string
}

type AzureClient struct {
	options    AzureOptions
	credential azcore.TokenCredential
	client     *http.Client
}

type AzureResponseError struct {
	StatusCode int
	Code       string
	Message    string
}

func (err *AzureResponseError) Error() string {
	if err.Code != "" {
		return fmt.Sprintf("Azure %s: %s", err.Code, err.Message)
	}
	return fmt.Sprintf("Azure returned HTTP %d: %s", err.StatusCode, err.Message)
}

func NewAzureClient(options AzureOptions) (*AzureClient, error) {
	credential, err := azidentity.NewDefaultAzureCredential(nil)
	if err != nil {
		return nil, fmt.Errorf("configure Azure management credential: %w", err)
	}
	return newAzureClient(options, credential, azureHTTPClient())
}

func azureHTTPClient() *http.Client {
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = (&net.Dialer{
		Timeout:   10 * time.Second,
		KeepAlive: 30 * time.Second,
	}).DialContext
	transport.TLSHandshakeTimeout = 10 * time.Second
	transport.ResponseHeaderTimeout = 15 * time.Second
	transport.ExpectContinueTimeout = time.Second
	return &http.Client{
		Transport: transport,
		Timeout:   azureRequestTimeout,
	}
}

func newAzureClient(
	options AzureOptions,
	credential azcore.TokenCredential,
	client *http.Client,
) (*AzureClient, error) {
	for name, value := range map[string]string{
		"subscription ID":          options.SubscriptionID,
		"publisher resource group": options.PublisherResourceGroup,
		"webhook base URL":         options.WebhookBaseURL,
	} {
		if strings.TrimSpace(value) == "" {
			return nil, fmt.Errorf("Azure %s is required", name)
		}
	}
	webhookURL, err := url.Parse(options.WebhookBaseURL)
	if err != nil || webhookURL.Scheme != "https" || webhookURL.Host == "" {
		return nil, fmt.Errorf("Azure webhook base URL must be an absolute HTTPS URL")
	}
	if options.WebhookAuthKeyLoader == nil && len(options.WebhookAuthKey) < 32 {
		return nil, fmt.Errorf("Azure webhook authentication key must contain at least 32 bytes")
	}
	if options.ManagementEndpoint == "" {
		options.ManagementEndpoint = "https://management.azure.com"
	}
	if client == nil {
		client = azureHTTPClient()
	}
	return &AzureClient{
		options:    options,
		credential: credential,
		client:     client,
	}, nil
}

func loadWebhookAuthKey(
	ctx context.Context,
	options AzureOptions,
) ([]byte, error) {
	if options.WebhookAuthKeyLoader != nil {
		key, err := options.WebhookAuthKeyLoader(ctx)
		if err != nil {
			return nil, fmt.Errorf("load Azure webhook authentication key: %w", err)
		}
		return append([]byte(nil), key...), nil
	}
	return append([]byte(nil), options.WebhookAuthKey...), nil
}

func (client *AzureClient) EnsureForwardingSubscription(
	ctx context.Context,
	allocation Allocation,
	source *api.Source,
) error {
	if err := client.validatePublisherScope(
		allocation.PublisherResourceID,
	); err != nil {
		return err
	}
	resourceID := allocation.PublisherResourceID +
		"/providers/Microsoft.EventGrid/eventSubscriptions/" +
		url.PathEscape(allocation.ForwardingSubscriptionName)
	webhookAuthKey, err := loadWebhookAuthKey(ctx, client.options)
	if err != nil {
		return err
	}
	if len(webhookAuthKey) < 32 {
		return fmt.Errorf("Azure webhook authentication key must contain at least 32 bytes")
	}
	keyDigest := webhook.KeyDigest(webhookAuthKey)
	exists, current, state, err := client.inspectForwardingSubscription(
		ctx,
		resourceID,
		allocation,
		source.Generation,
		keyDigest,
	)
	if err != nil {
		return err
	}

	if exists {
		if state != "" && !strings.EqualFold(state, "Succeeded") {
			return fmt.Errorf(
				"event subscription provisioning state is %s",
				state,
			)
		}
		if current {
			return nil
		}
	}
	filter, err := forwardingFilter(source)
	if err != nil {
		return err
	}
	properties := map[string]any{
		"destination": map[string]any{
			"endpointType": "WebHook",
			"properties": map[string]any{
				"endpointUrl": client.webhookURL(allocation),
				"deliveryAttributeMappings": []any{
					map[string]any{
						"name": "X-Lampyr-Token",
						"type": "Static",
						"properties": map[string]any{
							"value": webhook.Token(
								webhookAuthKey,
								allocation.SourceUID,
							),
							"isSecret": true,
						},
					},
				},
			},
		},
		"eventDeliverySchema": "CloudEventSchemaV1_0",
		"filter":              filter,
		"labels": []string{
			"lampyr-source-uid=" + allocation.SourceUID,
			fmt.Sprintf(
				"lampyr-source-generation=%d",
				source.Generation,
			),
			"lampyr-webhook-key-sha256=" + keyDigest,
		},
		"retryPolicy": forwardingRetryPolicy(source),
	}
	if err := client.put(
		ctx,
		resourceID,
		eventGridManagementVersion,
		map[string]any{"properties": properties},
		nil,
	); err != nil {
		return err
	}
	exists, current, state, err = client.inspectForwardingSubscription(
		ctx,
		resourceID,
		allocation,
		source.Generation,
		keyDigest,
	)
	if err != nil {
		return err
	}
	if !exists || !current {
		return fmt.Errorf(
			"event subscription provisioning state is Pending",
		)
	}
	if state != "" && !strings.EqualFold(state, "Succeeded") {
		return fmt.Errorf(
			"event subscription provisioning state is %s",
			state,
		)
	}
	return nil
}

func publisherScope(resourceID string) (string, string, error) {
	segments := strings.Split(strings.Trim(resourceID, "/"), "/")
	if len(segments) < 4 ||
		!strings.EqualFold(segments[0], "subscriptions") ||
		!strings.EqualFold(segments[2], "resourceGroups") ||
		strings.TrimSpace(segments[1]) == "" ||
		strings.TrimSpace(segments[3]) == "" {
		return "", "", fmt.Errorf(
			"publisher %q is not a fully qualified Azure resource ID",
			resourceID,
		)
	}
	return segments[1], segments[3], nil
}

func (client *AzureClient) DeleteForwardingSubscription(
	ctx context.Context,
	allocation Allocation,
) error {
	if err := client.validatePublisherScope(
		allocation.PublisherResourceID,
	); err != nil {
		return err
	}
	resourceID := allocation.PublisherResourceID +
		"/providers/Microsoft.EventGrid/eventSubscriptions/" +
		url.PathEscape(allocation.ForwardingSubscriptionName)
	return client.delete(ctx, resourceID, eventGridManagementVersion)
}

func (client *AzureClient) validatePublisherScope(resourceID string) error {
	subscriptionID, resourceGroup, err := publisherScope(resourceID)
	if err != nil {
		return err
	}
	if strings.EqualFold(subscriptionID, client.options.SubscriptionID) &&
		strings.EqualFold(resourceGroup, client.options.PublisherResourceGroup) {
		return nil
	}
	return fmt.Errorf(
		"publisher %q is outside allowed Azure scope /subscriptions/%s/resourceGroups/%s",
		resourceID,
		client.options.SubscriptionID,
		client.options.PublisherResourceGroup,
	)
}

func (client *AzureClient) inspectForwardingSubscription(
	ctx context.Context,
	resourceID string,
	allocation Allocation,
	generation int64,
	keyDigest string,
) (bool, bool, string, error) {
	var existing map[string]any
	err := client.get(ctx, resourceID, eventGridManagementVersion, &existing)
	if errors.Is(err, errResourceNotFound) {
		return false, false, "", nil
	}
	if err != nil {
		return false, false, "", err
	}
	labels, _, _ := nestedSlice(existing, "properties", "labels")
	expected := "lampyr-source-uid=" + allocation.SourceUID
	currentGeneration := fmt.Sprintf(
		"lampyr-source-generation=%d",
		generation,
	)
	currentKey := "lampyr-webhook-key-sha256=" + keyDigest
	owned := false
	generationMatches := false
	keyMatches := false
	for _, label := range labels {
		switch label {
		case expected:
			owned = true
		case currentGeneration:
			generationMatches = true
		case currentKey:
			keyMatches = true
		}
	}
	if !owned {
		return false, false, "", fmt.Errorf(
			"forwarding subscription %q has conflicting ownership",
			resourceID,
		)
	}
	state, _ := nestedString(existing, "properties", "provisioningState")
	return true, generationMatches && keyMatches, state, nil
}

func (client *AzureClient) webhookURL(allocation Allocation) string {
	endpoint, _ := url.Parse(strings.TrimRight(client.options.WebhookBaseURL, "/"))
	endpoint.Path = strings.TrimRight(endpoint.Path, "/") +
		"/sources/" + url.PathEscape(allocation.SourceUID)
	return endpoint.String()
}

func (client *AzureClient) get(
	ctx context.Context,
	resourceID string,
	apiVersion string,
	result any,
) error {
	return client.request(ctx, http.MethodGet, resourceID, apiVersion, nil, result)
}

func (client *AzureClient) put(
	ctx context.Context,
	resourceID string,
	apiVersion string,
	body any,
	result any,
) error {
	return client.request(ctx, http.MethodPut, resourceID, apiVersion, body, result)
}

func (client *AzureClient) delete(
	ctx context.Context,
	resourceID string,
	apiVersion string,
) error {
	err := client.request(ctx, http.MethodDelete, resourceID, apiVersion, nil, nil)
	if errors.Is(err, errResourceNotFound) {
		return nil
	}
	if err != nil {
		return err
	}
	for attempt := 0; attempt < 60; attempt++ {
		var resource map[string]any
		err := client.get(ctx, resourceID, apiVersion, &resource)
		if errors.Is(err, errResourceNotFound) {
			return nil
		}
		if err != nil {
			return err
		}
		timer := time.NewTimer(2 * time.Second)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
	}
	return fmt.Errorf("timed out confirming deletion of Azure resource %s", resourceID)
}

func (client *AzureClient) request(
	ctx context.Context,
	method string,
	resourceID string,
	apiVersion string,
	body any,
	result any,
) error {
	requestContext, cancel := context.WithTimeout(ctx, azureRequestTimeout)
	defer cancel()
	token, err := client.credential.GetToken(requestContext, policy.TokenRequestOptions{
		Scopes: []string{armTokenScope},
	})
	if err != nil {
		return fmt.Errorf("acquire Azure management token: %w", err)
	}
	var requestBody io.Reader
	if body != nil {
		encoded, err := json.Marshal(body)
		if err != nil {
			return err
		}
		requestBody = bytes.NewReader(encoded)
	}
	endpoint := strings.TrimRight(client.options.ManagementEndpoint, "/") +
		resourceID + "?api-version=" + url.QueryEscape(apiVersion)
	request, err := http.NewRequestWithContext(
		requestContext,
		method,
		endpoint,
		requestBody,
	)
	if err != nil {
		return err
	}
	request.Header.Set("Authorization", "Bearer "+token.Token)
	request.Header.Set("Content-Type", "application/json")
	response, err := client.client.Do(request)
	if err != nil {
		return fmt.Errorf("Azure management %s %s: %w", method, resourceID, err)
	}
	defer response.Body.Close()
	if response.StatusCode == http.StatusNotFound {
		return errResourceNotFound
	}
	if response.StatusCode < 200 || response.StatusCode >= 300 {
		message, _ := io.ReadAll(io.LimitReader(response.Body, 8192))
		var errorBody struct {
			Error struct {
				Code    string `json:"code"`
				Message string `json:"message"`
			} `json:"error"`
		}
		_ = json.Unmarshal(message, &errorBody)
		errorMessage := strings.TrimSpace(errorBody.Error.Message)
		if errorMessage == "" {
			errorMessage = strings.TrimSpace(string(message))
		}
		return &AzureResponseError{
			StatusCode: response.StatusCode,
			Code:       errorBody.Error.Code,
			Message:    errorMessage,
		}
	}
	if result == nil || response.StatusCode == http.StatusNoContent {
		return nil
	}
	encoded, err := io.ReadAll(io.LimitReader(
		response.Body,
		maxAzureResponseBytes+1,
	))
	if err != nil {
		return fmt.Errorf("read Azure management response: %w", err)
	}
	if len(encoded) > maxAzureResponseBytes {
		return fmt.Errorf(
			"Azure management response exceeds %d bytes",
			maxAzureResponseBytes,
		)
	}
	if err := json.Unmarshal(encoded, result); err != nil {
		return fmt.Errorf("decode Azure management response: %w", err)
	}
	return nil
}

func forwardingFilter(source *api.Source) (map[string]any, error) {
	filter := map[string]any{
		"isSubjectCaseSensitive": false,
	}
	if len(source.Spec.EventSubscription.IncludedEventTypes) > 0 {
		filter["includedEventTypes"] = source.Spec.EventSubscription.IncludedEventTypes
	}
	if source.Spec.EventSubscription.SubjectBeginsWith != "" {
		filter["subjectBeginsWith"] = source.Spec.EventSubscription.SubjectBeginsWith
	}
	if source.Spec.EventSubscription.SubjectEndsWith != "" {
		filter["subjectEndsWith"] = source.Spec.EventSubscription.SubjectEndsWith
	}
	if len(source.Spec.EventSubscription.AdvancedFilters) > 0 {
		filters := make([]map[string]any, 0, len(source.Spec.EventSubscription.AdvancedFilters))
		for _, advanced := range source.Spec.EventSubscription.AdvancedFilters {
			if err := api.ValidateAdvancedFilter(advanced); err != nil {
				return nil, err
			}
			value := map[string]any{
				"operatorType": advanced.OperatorType,
				"key":          advanced.Key,
			}
			switch advanced.OperatorType {
			case "BoolEquals",
				"NumberGreaterThan",
				"NumberGreaterThanOrEquals",
				"NumberLessThan",
				"NumberLessThanOrEquals":
				value["value"] = advanced.Values[0]
			case "IsNotNull", "IsNullOrUndefined":
			default:
				value["values"] = advanced.Values
			}
			filters = append(filters, value)
		}
		filter["advancedFilters"] = filters
	}
	return filter, nil
}

func forwardingRetryPolicy(source *api.Source) map[string]any {
	maximum := int32(30)
	ttl := int32(1440)
	if source.Spec.EventSubscription.RetryPolicy != nil {
		if source.Spec.EventSubscription.RetryPolicy.MaxDeliveryAttempts > 0 {
			maximum = source.Spec.EventSubscription.RetryPolicy.MaxDeliveryAttempts
		}
		if source.Spec.EventSubscription.RetryPolicy.EventTimeToLiveMinutes > 0 {
			ttl = source.Spec.EventSubscription.RetryPolicy.EventTimeToLiveMinutes
		}
	}
	return map[string]any{
		"maxDeliveryAttempts":      maximum,
		"eventTimeToLiveInMinutes": ttl,
	}
}

func deterministicUUID(parts ...string) string {
	sum := sha256.Sum256([]byte(strings.Join(parts, "\x00")))
	value := sum[:16]
	value[6] = (value[6] & 0x0f) | 0x50
	value[8] = (value[8] & 0x3f) | 0x80
	hexValue := hex.EncodeToString(value)
	return fmt.Sprintf(
		"%s-%s-%s-%s-%s",
		hexValue[0:8],
		hexValue[8:12],
		hexValue[12:16],
		hexValue[16:20],
		hexValue[20:32],
	)
}

func nestedString(object map[string]any, fields ...string) (string, bool) {
	value := any(object)
	for _, field := range fields {
		current, ok := value.(map[string]any)
		if !ok {
			return "", false
		}
		value, ok = current[field]
		if !ok {
			return "", false
		}
	}
	result, ok := value.(string)
	return result, ok
}

func nestedSlice(object map[string]any, fields ...string) ([]string, bool, error) {
	value := any(object)
	for _, field := range fields {
		current, ok := value.(map[string]any)
		if !ok {
			return nil, false, nil
		}
		value, ok = current[field]
		if !ok {
			return nil, false, nil
		}
	}
	items, ok := value.([]any)
	if !ok {
		return nil, false, nil
	}
	result := make([]string, 0, len(items))
	for _, item := range items {
		if text, ok := item.(string); ok {
			result = append(result, text)
		}
	}
	return result, true, nil
}
