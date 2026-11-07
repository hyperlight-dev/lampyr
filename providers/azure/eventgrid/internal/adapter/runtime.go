// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"sync"
	"sync/atomic"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/client-go/dynamic"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/config"
)

type Runtime struct {
	client    dynamic.Interface
	processor DeliveryProcessor
	interval  time.Duration
	ready     atomic.Bool

	mu       sync.RWMutex
	webhooks map[string]*webhookRegistration
}

type DeliveryProcessor interface {
	ProcessWebhook(context.Context, config.AdapterConfig, []byte) error
	Metrics() *Metrics
}

type webhookRegistration struct {
	digest string
	config config.AdapterConfig
	gate   chan struct{}
	active sync.WaitGroup
}

func NewRuntime(client dynamic.Interface, processor DeliveryProcessor) *Runtime {
	return &Runtime{
		client:    client,
		processor: processor,
		interval:  2 * time.Second,
		webhooks:  make(map[string]*webhookRegistration),
	}
}

func (runtime *Runtime) Ready() bool {
	return runtime.ready.Load()
}

func (runtime *Runtime) Run(ctx context.Context) error {
	defer runtime.stopWebhooks()
	if err := runtime.sync(ctx); err != nil {
		log.Printf("initial adapter configuration sync: %v", err)
	}
	ticker := time.NewTicker(runtime.interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return nil
		case <-ticker.C:
			if err := runtime.sync(ctx); err != nil {
				runtime.ready.Store(false)
				log.Printf("adapter configuration sync: %v", err)
			}
		}
	}
}

func (runtime *Runtime) sync(ctx context.Context) error {
	list, err := runtime.client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).List(ctx, metav1.ListOptions{
		LabelSelector: config.LabelAdapterConfig + "=true",
	})
	if err != nil {
		return fmt.Errorf("list adapter configurations: %w", err)
	}
	seen := make(map[string]struct{}, len(list.Items))
	var syncErrors []error
	for index := range list.Items {
		configMap := &list.Items[index]
		adapterConfig, digest, err := parseConfigMap(configMap)
		if err != nil {
			syncErrors = append(syncErrors, fmt.Errorf("%s/%s: %w", configMap.GetNamespace(), configMap.GetName(), err))
			continue
		}
		uid := adapterConfig.Source.UID
		seen[uid] = struct{}{}
		if configMap.GetAnnotations()[config.AnnotationStopRequested] == "true" {
			runtime.stopSource(uid)
			if configMap.GetAnnotations()[config.AnnotationStopped] != "true" {
				if err := runtime.markStopped(ctx, configMap, digest); err != nil {
					syncErrors = append(syncErrors, err)
				}
			}
			continue
		}
		runtime.registerWebhook(uid, digest, adapterConfig)
		if configMap.GetAnnotations()[config.AnnotationLoadedDigest] != digest {
			if err := runtime.markLoaded(ctx, configMap, digest); err != nil {
				syncErrors = append(syncErrors, err)
			}
		}
		if configMap.GetAnnotations()[config.AnnotationRuntimeReady] != "true" {
			if err := runtime.markRuntimeStatus(
				ctx,
				digest,
				adapterConfig,
				true,
				"",
				"",
			); err != nil {
				syncErrors = append(syncErrors, err)
			}
		}
	}
	runtime.stopMissing(seen)
	if len(syncErrors) == 0 {
		runtime.ready.Store(true)
		runtime.processor.Metrics().configuredSources.Store(int64(len(seen)))
	} else {
		runtime.processor.Metrics().configurationErrors.Add(uint64(len(syncErrors)))
	}
	return errors.Join(syncErrors...)
}

func parseConfigMap(configMap *unstructured.Unstructured) (config.AdapterConfig, string, error) {
	if configMap.GetNamespace() != config.SystemNamespace {
		return config.AdapterConfig{}, "", fmt.Errorf("configuration is outside the protected namespace")
	}
	rawConfig, found, err := unstructured.NestedString(configMap.Object, "data", config.DataKey)
	if err != nil || !found {
		return config.AdapterConfig{}, "", fmt.Errorf("%s is missing", config.DataKey)
	}
	var adapterConfig config.AdapterConfig
	if err := json.Unmarshal([]byte(rawConfig), &adapterConfig); err != nil {
		return config.AdapterConfig{}, "", fmt.Errorf("decode configuration: %w", err)
	}
	if err := config.Validate(adapterConfig); err != nil {
		return config.AdapterConfig{}, "", err
	}
	if configMap.GetLabels()[config.LabelSourceUID] != adapterConfig.Source.UID {
		return config.AdapterConfig{}, "", fmt.Errorf("source UID label does not match configuration")
	}
	if configMap.GetName() != config.NameForUID(adapterConfig.Source.UID) {
		return config.AdapterConfig{}, "", fmt.Errorf("configuration name does not match source UID")
	}
	if configMap.GetLabels()[config.LabelSourceNamespace] != adapterConfig.Source.Namespace ||
		configMap.GetLabels()[config.LabelSourceName] != adapterConfig.Source.Name {
		return config.AdapterConfig{}, "", fmt.Errorf("source identity labels do not match configuration")
	}
	digest := configMap.GetAnnotations()[config.AnnotationConfigDigest]
	if digest == "" {
		return config.AdapterConfig{}, "", fmt.Errorf("configuration digest annotation is missing")
	}
	sum := sha256.Sum256([]byte(rawConfig))
	if digest != hex.EncodeToString(sum[:]) {
		return config.AdapterConfig{}, "", fmt.Errorf("configuration digest does not match content")
	}
	return adapterConfig, digest, nil
}

func (runtime *Runtime) registerWebhook(
	uid string,
	digest string,
	adapterConfig config.AdapterConfig,
) {
	runtime.mu.Lock()
	current := runtime.webhooks[uid]
	if current != nil && current.digest == digest {
		runtime.mu.Unlock()
		return
	}
	runtime.webhooks[uid] = &webhookRegistration{
		digest: digest,
		config: adapterConfig,
		gate: make(
			chan struct{},
			adapterConfig.Webhook.MaxConcurrentDeliveries,
		),
	}
	runtime.mu.Unlock()
	if current != nil {
		current.active.Wait()
	}
}

func (runtime *Runtime) webhook(
	uid string,
) (*webhookRegistration, bool) {
	runtime.mu.RLock()
	defer runtime.mu.RUnlock()
	registration, found := runtime.webhooks[uid]
	return registration, found
}

func (runtime *Runtime) beginWebhook(
	uid string,
) (*webhookRegistration, bool) {
	runtime.mu.RLock()
	registration, found := runtime.webhooks[uid]
	if found {
		registration.active.Add(1)
	}
	runtime.mu.RUnlock()
	return registration, found
}

func (runtime *Runtime) unregisterWebhook(uid string) {
	runtime.mu.Lock()
	registration := runtime.webhooks[uid]
	if registration != nil {
		delete(runtime.webhooks, uid)
	}
	runtime.mu.Unlock()
	if registration != nil {
		registration.active.Wait()
	}
}

func (runtime *Runtime) markRuntimeStatus(
	ctx context.Context,
	digest string,
	adapterConfig config.AdapterConfig,
	ready bool,
	reasonCode string,
	reason string,
) error {
	resource := runtime.client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	)
	name := config.NameForUID(adapterConfig.Source.UID)
	configMap, err := resource.Get(ctx, name, metav1.GetOptions{})
	if err != nil {
		return err
	}
	if configMap.GetAnnotations()[config.AnnotationConfigDigest] != digest {
		return nil
	}
	if len(reason) > 512 {
		reason = reason[:512]
	}
	annotations := configMap.GetAnnotations()
	if annotations == nil {
		annotations = make(map[string]string)
	}
	if ready {
		annotations[config.AnnotationRuntimeReady] = "true"
		delete(annotations, config.AnnotationRuntimeCode)
		delete(annotations, config.AnnotationRuntimeReason)
	} else {
		annotations[config.AnnotationRuntimeReady] = "false"
		annotations[config.AnnotationRuntimeCode] = reasonCode
		annotations[config.AnnotationRuntimeReason] = reason
	}
	updated := configMap.DeepCopy()
	updated.SetAnnotations(annotations)
	_, err = resource.Update(ctx, updated, metav1.UpdateOptions{})
	return err
}

func (runtime *Runtime) markLoaded(ctx context.Context, configMap *unstructured.Unstructured, digest string) error {
	updated := configMap.DeepCopy()
	annotations := updated.GetAnnotations()
	if annotations == nil {
		annotations = make(map[string]string)
	}
	annotations[config.AnnotationLoadedDigest] = digest
	updated.SetAnnotations(annotations)
	_, err := runtime.client.Resource(config.ConfigGVR).Namespace(updated.GetNamespace()).Update(ctx, updated, metav1.UpdateOptions{})
	if err != nil {
		return fmt.Errorf("mark adapter config %s/%s loaded: %w", updated.GetNamespace(), updated.GetName(), err)
	}
	return nil
}

func (runtime *Runtime) markStopped(
	ctx context.Context,
	configMap *unstructured.Unstructured,
	digest string,
) error {
	updated := configMap.DeepCopy()
	annotations := updated.GetAnnotations()
	if annotations == nil ||
		annotations[config.AnnotationConfigDigest] != digest {
		return nil
	}
	annotations[config.AnnotationStopped] = "true"
	annotations[config.AnnotationRuntimeReady] = "false"
	annotations[config.AnnotationRuntimeCode] = "Stopped"
	annotations[config.AnnotationRuntimeReason] = "Adapter receive loop stopped and in-flight work settled."
	updated.SetAnnotations(annotations)
	_, err := runtime.client.Resource(config.ConfigGVR).Namespace(
		config.SystemNamespace,
	).Update(ctx, updated, metav1.UpdateOptions{})
	if err != nil {
		return fmt.Errorf(
			"mark adapter config %s/%s stopped: %w",
			updated.GetNamespace(),
			updated.GetName(),
			err,
		)
	}
	return nil
}

func (runtime *Runtime) stopSource(uid string) {
	runtime.unregisterWebhook(uid)
}

func (runtime *Runtime) stopMissing(seen map[string]struct{}) {
	var stoppedWebhooks []*webhookRegistration
	runtime.mu.Lock()
	for uid := range runtime.webhooks {
		if _, found := seen[uid]; !found {
			stoppedWebhooks = append(stoppedWebhooks, runtime.webhooks[uid])
			delete(runtime.webhooks, uid)
		}
	}
	runtime.mu.Unlock()
	for _, registration := range stoppedWebhooks {
		registration.active.Wait()
	}
}

func (runtime *Runtime) stopWebhooks() {
	var stoppedWebhooks []*webhookRegistration
	runtime.mu.Lock()
	for uid, registration := range runtime.webhooks {
		stoppedWebhooks = append(stoppedWebhooks, registration)
		delete(runtime.webhooks, uid)
	}
	runtime.mu.Unlock()
	for _, registration := range stoppedWebhooks {
		registration.active.Wait()
	}
}
