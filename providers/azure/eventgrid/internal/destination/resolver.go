// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package destination

import (
	"context"
	"fmt"
	"net/url"

	"k8s.io/apimachinery/pkg/api/meta"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/dynamic"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

type ResourceMapper interface {
	ResourceFor(apiVersion, kind string) (schema.GroupVersionResource, error)
}

type RESTMapper struct {
	Mapper meta.RESTMapper
}

func (mapper RESTMapper) ResourceFor(apiVersion, kind string) (schema.GroupVersionResource, error) {
	groupVersion, err := schema.ParseGroupVersion(apiVersion)
	if err != nil {
		return schema.GroupVersionResource{}, fmt.Errorf("parse apiVersion %q: %w", apiVersion, err)
	}
	mapping, err := mapper.Mapper.RESTMapping(schema.GroupKind{Group: groupVersion.Group, Kind: kind}, groupVersion.Version)
	if err != nil {
		return schema.GroupVersionResource{}, fmt.Errorf("map %s %s: %w", apiVersion, kind, err)
	}
	return mapping.Resource, nil
}

type Resolver struct {
	client dynamic.Interface
	mapper ResourceMapper
}

func NewResolver(client dynamic.Interface, mapper ResourceMapper) *Resolver {
	return &Resolver{client: client, mapper: mapper}
}

func (resolver *Resolver) Resolve(ctx context.Context, sourceNamespace string, destination api.Destination) (string, error) {
	if err := api.ValidateDestination(destination); err != nil {
		return "", err
	}
	if destination.Ref == nil {
		return destination.URI, nil
	}

	resource, err := resolver.mapper.ResourceFor(destination.Ref.APIVersion, destination.Ref.Kind)
	if err != nil {
		return "", err
	}
	namespace := destination.Ref.Namespace
	if namespace == "" {
		namespace = sourceNamespace
	}
	if namespace != sourceNamespace {
		return "", fmt.Errorf(
			"referenced sink namespace %q must match source namespace %q",
			namespace,
			sourceNamespace,
		)
	}
	target, err := resolver.client.Resource(resource).Namespace(namespace).Get(ctx, destination.Ref.Name, metav1.GetOptions{})
	if err != nil {
		return "", fmt.Errorf("get sink %s/%s: %w", namespace, destination.Ref.Name, err)
	}
	base, found, err := unstructuredAddress(target.Object)
	if err != nil {
		return "", err
	}
	if !found {
		return "", fmt.Errorf("sink %s/%s has no status.address.url", namespace, destination.Ref.Name)
	}
	if destination.URI == "" {
		return base, nil
	}
	baseURI, err := url.Parse(base)
	if err != nil {
		return "", fmt.Errorf("parse sink address: %w", err)
	}
	suffix, err := url.Parse(destination.URI)
	if err != nil {
		return "", fmt.Errorf("parse sink URI: %w", err)
	}
	return baseURI.ResolveReference(suffix).String(), nil
}

func unstructuredAddress(object map[string]any) (string, bool, error) {
	address, found, err := nestedString(object, "status", "address", "url")
	if err != nil || found {
		return address, found, err
	}
	return nestedString(object, "status", "url")
}

func nestedString(object map[string]any, fields ...string) (string, bool, error) {
	current := any(object)
	for _, field := range fields {
		asMap, ok := current.(map[string]any)
		if !ok {
			return "", false, fmt.Errorf("%v is not an object", fields)
		}
		value, found := asMap[field]
		if !found {
			return "", false, nil
		}
		current = value
	}
	value, ok := current.(string)
	if !ok {
		return "", false, fmt.Errorf("%v is not a string", fields)
	}
	if value == "" {
		return "", false, nil
	}
	return value, true, nil
}
