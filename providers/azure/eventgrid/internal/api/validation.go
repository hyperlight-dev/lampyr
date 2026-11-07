// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package api

import (
	"fmt"
	"net/url"
	"regexp"
	"strings"
)

var extensionNamePattern = regexp.MustCompile(`^[a-z0-9]{1,20}$`)

var reservedCloudEventAttributes = map[string]struct{}{
	"specversion":      {},
	"id":               {},
	"source":           {},
	"type":             {},
	"subject":          {},
	"time":             {},
	"datacontenttype":  {},
	"dataschema":       {},
	"data":             {},
	"data_base64":      {},
	"locktoken":        {},
	"deliverycount":    {},
	"lockeduntil":      {},
	"brokerproperties": {},
}

func ValidateSource(source *Source) error {
	if strings.TrimSpace(source.Spec.SourceResourceID) == "" {
		return fmt.Errorf("spec.sourceResourceID is required")
	}
	if err := ValidateAzureResourceID(source.Spec.SourceResourceID); err != nil {
		return fmt.Errorf("spec.sourceResourceID: %w", err)
	}
	if err := ValidatePublisherResourceID(source.Spec.SourceResourceID); err != nil {
		return fmt.Errorf("spec.sourceResourceID: %w", err)
	}
	if err := ValidateDestination(source.Spec.Sink); err != nil {
		return fmt.Errorf("spec.sink: %w", err)
	}

	if source.Spec.Delivery.MaxConcurrentDeliveries < 0 ||
		source.Spec.Delivery.MaxConcurrentDeliveries > 32 {
		return fmt.Errorf("spec.delivery.maxConcurrentDeliveries must be between 1 and 32 when set")
	}
	if source.Spec.EventSubscription.RetryPolicy != nil {
		retry := source.Spec.EventSubscription.RetryPolicy
		if retry.MaxDeliveryAttempts < 0 || retry.MaxDeliveryAttempts > 30 {
			return fmt.Errorf(
				"spec.eventSubscription.retryPolicy.maxDeliveryAttempts must be between 1 and 30 when set",
			)
		}
		if retry.EventTimeToLiveMinutes < 0 ||
			retry.EventTimeToLiveMinutes > 1440 {
			return fmt.Errorf(
				"spec.eventSubscription.retryPolicy.eventTimeToLiveMinutes must be between 1 and 1440 when set",
			)
		}

	}
	for index, filter := range source.Spec.EventSubscription.AdvancedFilters {
		if err := ValidateAdvancedFilter(filter); err != nil {
			return fmt.Errorf(
				"spec.eventSubscription.advancedFilters[%d]: %w",
				index,
				err,
			)
		}
	}
	if source.Spec.CEOverrides != nil {
		for name := range source.Spec.CEOverrides.Extensions {
			if err := ValidateExtensionName(name); err != nil {
				return fmt.Errorf("spec.ceOverrides.extensions: %w", err)
			}
			if err := ValidateExtensionValue(
				name,
				source.Spec.CEOverrides.Extensions[name],
			); err != nil {
				return fmt.Errorf("spec.ceOverrides.extensions: %w", err)
			}
		}
	}
	return nil
}

func ValidateAdvancedFilter(filter AdvancedFilter) error {
	if strings.TrimSpace(filter.Key) == "" {
		return fmt.Errorf("key is required")
	}
	singular := map[string]struct{}{
		"BoolEquals":                {},
		"NumberGreaterThan":         {},
		"NumberGreaterThanOrEquals": {},
		"NumberLessThan":            {},
		"NumberLessThanOrEquals":    {},
	}
	empty := map[string]struct{}{
		"IsNotNull":         {},
		"IsNullOrUndefined": {},
	}
	plural := map[string]struct{}{
		"NumberIn":            {},
		"NumberInRange":       {},
		"NumberNotIn":         {},
		"NumberNotInRange":    {},
		"StringBeginsWith":    {},
		"StringContains":      {},
		"StringEndsWith":      {},
		"StringIn":            {},
		"StringNotBeginsWith": {},
		"StringNotContains":   {},
		"StringNotEndsWith":   {},
		"StringNotIn":         {},
	}
	if _, found := singular[filter.OperatorType]; found {
		if len(filter.Values) != 1 {
			return fmt.Errorf("operator %s requires exactly one value", filter.OperatorType)
		}
		return nil
	}
	if _, found := empty[filter.OperatorType]; found {
		if len(filter.Values) != 0 {
			return fmt.Errorf("operator %s does not accept values", filter.OperatorType)
		}
		return nil
	}
	if _, found := plural[filter.OperatorType]; found {
		if len(filter.Values) == 0 {
			return fmt.Errorf("operator %s requires one or more values", filter.OperatorType)
		}
		return nil
	}
	return fmt.Errorf("operator %q is not supported", filter.OperatorType)
}

func ValidatePublisherResourceID(resourceID string) error {
	segments := strings.Split(strings.Trim(resourceID, "/"), "/")
	if len(segments) != 8 ||
		!strings.EqualFold(segments[4], "providers") ||
		!strings.EqualFold(segments[5], "Microsoft.Storage") ||
		!strings.EqualFold(segments[6], "storageAccounts") {
		return fmt.Errorf(
			"only Azure Storage account publishers are supported in v1alpha1",
		)
	}
	return nil
}

func ValidateAzureResourceID(resourceID string) error {
	if strings.ContainsAny(resourceID, "?#") {
		return fmt.Errorf("must not contain a query or fragment")
	}
	segments := strings.Split(strings.Trim(resourceID, "/"), "/")
	if len(segments) < 8 ||
		!strings.EqualFold(segments[0], "subscriptions") || segments[1] == "" ||
		!strings.EqualFold(segments[2], "resourceGroups") || segments[3] == "" ||
		!strings.EqualFold(segments[4], "providers") || segments[5] == "" {
		return fmt.Errorf("must be a fully qualified Azure resource ID")
	}
	for _, segment := range segments {
		if strings.TrimSpace(segment) == "" {
			return fmt.Errorf("contains an empty path segment")
		}
	}
	return nil
}

func ValidateDestination(destination Destination) error {
	if destination.Ref == nil && strings.TrimSpace(destination.URI) == "" {
		return fmt.Errorf("ref or uri is required")
	}
	if destination.Ref != nil {
		if destination.Ref.APIVersion == "" || destination.Ref.Kind == "" || destination.Ref.Name == "" {
			return fmt.Errorf("ref apiVersion, kind, and name are required")
		}
	}
	if destination.URI != "" {
		if err := validateHTTPURI(destination.URI, destination.Ref != nil); err != nil {
			return fmt.Errorf("uri: %w", err)
		}
	}
	return nil
}

func ValidateExtensionName(name string) error {
	if !extensionNamePattern.MatchString(name) {
		return fmt.Errorf("extension %q must contain only lowercase letters and digits and be at most 20 characters", name)
	}
	if _, reserved := reservedCloudEventAttributes[name]; reserved {
		return fmt.Errorf("extension %q is reserved", name)
	}
	return nil
}

func validateHTTPURI(value string, allowRelative bool) error {
	parsed, err := url.Parse(value)
	if err != nil {
		return err
	}
	if allowRelative {
		if parsed.IsAbs() || parsed.Host != "" ||
			strings.HasPrefix(value, "//") {
			return fmt.Errorf(
				"absolute URI is not allowed when ref is present",
			)
		}
		if strings.HasPrefix(parsed.Path, "/") {
			return nil
		}
		return fmt.Errorf("relative URI must begin with /")
	}
	if parsed.Scheme != "http" && parsed.Scheme != "https" {
		return fmt.Errorf("must use http or https")
	}
	if parsed.Host == "" {
		return fmt.Errorf("host is required")
	}
	return nil
}
