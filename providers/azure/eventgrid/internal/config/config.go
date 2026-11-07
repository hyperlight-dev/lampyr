// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package config

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"net/url"
	"regexp"
	"strings"

	"k8s.io/apimachinery/pkg/runtime/schema"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

const (
	Version                 = "v1alpha1"
	SystemNamespace         = "lampyr-system"
	DataKey                 = "config.json"
	LabelAdapterConfig      = "sources.lampyr.dev/adapter-config"
	LabelSourceUID          = "sources.lampyr.dev/source-uid"
	LabelSourceNamespace    = "sources.lampyr.dev/source-namespace"
	LabelSourceName         = "sources.lampyr.dev/source-name"
	AnnotationConfigDigest  = "sources.lampyr.dev/config-digest"
	AnnotationLoadedDigest  = "sources.lampyr.dev/loaded-digest"
	AnnotationRuntimeReady  = "sources.lampyr.dev/runtime-ready"
	AnnotationRuntimeCode   = "sources.lampyr.dev/runtime-code"
	AnnotationRuntimeReason = "sources.lampyr.dev/runtime-reason"
	AnnotationStopRequested = "sources.lampyr.dev/stop-requested"
	AnnotationStopped       = "sources.lampyr.dev/stopped"
)

var ConfigGVR = schema.GroupVersionResource{Version: "v1", Resource: "configmaps"}

type AdapterConfig struct {
	Version     string            `json:"version"`
	Source      SourceReference   `json:"source"`
	Generation  int64             `json:"generation"`
	Webhook     *WebhookReference `json:"webhook,omitempty"`
	SinkURI     string            `json:"sinkUri"`
	CEOverrides map[string]any    `json:"ceOverrides,omitempty"`
}

type SourceReference struct {
	APIVersion string `json:"apiVersion"`
	Kind       string `json:"kind"`
	Namespace  string `json:"namespace"`
	Name       string `json:"name"`
	UID        string `json:"uid"`
}

type WebhookReference struct {
	Path                    string `json:"path"`
	MaxConcurrentDeliveries int    `json:"maxConcurrentDeliveries"`
}

func BuildWebhook(
	source *api.Source,
	sinkURI string,
) (AdapterConfig, []byte, string, error) {
	concurrency := int(source.Spec.Delivery.MaxConcurrentDeliveries)
	if concurrency == 0 {
		concurrency = 1
	}
	return build(source, sinkURI, &WebhookReference{
		Path:                    WebhookPath(string(source.UID)),
		MaxConcurrentDeliveries: concurrency,
	})
}

func build(
	source *api.Source,
	sinkURI string,
	webhook *WebhookReference,
) (AdapterConfig, []byte, string, error) {
	adapterConfig := AdapterConfig{
		Version: Version,
		Source: SourceReference{
			APIVersion: api.GroupVersion,
			Kind:       api.Kind,
			Namespace:  source.Namespace,
			Name:       source.Name,
			UID:        string(source.UID),
		},
		Generation: source.Generation,
		Webhook:    webhook,
		SinkURI:    sinkURI,
	}
	if source.Spec.CEOverrides != nil {
		adapterConfig.CEOverrides = source.Spec.CEOverrides.Extensions
	}
	if err := Validate(adapterConfig); err != nil {
		return AdapterConfig{}, nil, "", err
	}
	data, err := json.Marshal(adapterConfig)
	if err != nil {
		return AdapterConfig{}, nil, "", fmt.Errorf("encode adapter config: %w", err)
	}
	sum := sha256.Sum256(data)
	return adapterConfig, data, hex.EncodeToString(sum[:]), nil
}

func Validate(adapterConfig AdapterConfig) error {
	if adapterConfig.Version != Version {
		return fmt.Errorf("unsupported config version %q", adapterConfig.Version)
	}
	if adapterConfig.Source.UID == "" || adapterConfig.Source.Namespace == "" || adapterConfig.Source.Name == "" {
		return fmt.Errorf("source namespace, name, and UID are required")
	}
	if adapterConfig.Webhook == nil {
		return fmt.Errorf("webhook configuration is required")
	}
	if adapterConfig.Webhook.Path != WebhookPath(adapterConfig.Source.UID) {
		return fmt.Errorf("webhook path does not match source UID")
	}
	if adapterConfig.Webhook.MaxConcurrentDeliveries < 1 ||
		adapterConfig.Webhook.MaxConcurrentDeliveries > 32 {
		return fmt.Errorf("webhook maxConcurrentDeliveries must be between 1 and 32")
	}
	return validateSinkAndOverrides(adapterConfig)
}

func validateSinkAndOverrides(adapterConfig AdapterConfig) error {
	if adapterConfig.SinkURI == "" {
		return fmt.Errorf("sink URI is required")
	}
	for name, value := range adapterConfig.CEOverrides {
		if err := api.ValidateExtensionName(name); err != nil {
			return err
		}
		if err := api.ValidateExtensionValue(name, value); err != nil {
			return err
		}
	}
	return nil
}

func WebhookPath(uid string) string {
	return "/sources/" + url.PathEscape(uid)
}

var invalidName = regexp.MustCompile(`[^a-z0-9-]+`)

func NameForUID(uid string) string {
	name := strings.ToLower(uid)
	name = invalidName.ReplaceAllString(name, "-")
	name = strings.Trim(name, "-")
	if len(name) > 40 {
		name = name[:40]
	}
	if name == "" {
		name = "unknown"
	}
	return "eventgrid-source-" + name
}
