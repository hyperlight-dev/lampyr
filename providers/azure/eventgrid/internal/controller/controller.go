// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package controller

import (
	"context"
	"fmt"
	"log"
	"sync"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/watch"
	"k8s.io/client-go/dynamic"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
)

type Controller struct {
	client     dynamic.Interface
	reconciler *Reconciler
	resync     time.Duration
	retryMu    sync.Mutex
	retries    map[sourceKey]*retryState
}

type sourceKey struct {
	namespace string
	name      string
}

type retryState struct {
	attempt   int
	scheduled bool
}

func New(client dynamic.Interface, reconciler *Reconciler) *Controller {
	return &Controller{
		client:     client,
		reconciler: reconciler,
		resync:     5 * time.Second,
		retries:    make(map[sourceKey]*retryState),
	}
}

func (controller *Controller) Run(ctx context.Context) error {
	keys := make(chan sourceKey, 256)
	go controller.watch(ctx, keys)
	go controller.resyncSources(ctx, keys)

	for {
		select {
		case <-ctx.Done():
			return nil
		case key := <-keys:
			if err := controller.reconciler.Reconcile(ctx, key.namespace, key.name); err != nil {
				log.Printf("reconcile AzureEventGridSource %s/%s: %v", key.namespace, key.name, err)
				controller.scheduleRetry(ctx, keys, key)
				continue
			}
			controller.clearRetry(key)
		}
	}
}

func (controller *Controller) watch(ctx context.Context, keys chan<- sourceKey) {
	for ctx.Err() == nil {
		timeout := int64(300)
		stream, err := controller.client.Resource(api.SourceGVR).Namespace(metav1.NamespaceAll).Watch(ctx, metav1.ListOptions{
			TimeoutSeconds: &timeout,
		})
		if err != nil {
			log.Printf("watch AzureEventGridSources: %v", err)
			if !sleep(ctx, time.Second) {
				return
			}
			continue
		}
		controller.consumeWatch(ctx, stream, keys)
		stream.Stop()
	}
}

func (controller *Controller) consumeWatch(ctx context.Context, stream watch.Interface, keys chan<- sourceKey) {
	for {
		select {
		case <-ctx.Done():
			return
		case event, open := <-stream.ResultChan():
			if !open {
				return
			}
			if event.Type == watch.Error {
				return
			}
			object, ok := event.Object.(*unstructured.Unstructured)
			if !ok || object.GetName() == "" || object.GetNamespace() == "" {
				continue
			}
			enqueue(ctx, keys, sourceKey{namespace: object.GetNamespace(), name: object.GetName()})
		}
	}
}

func (controller *Controller) resyncSources(ctx context.Context, keys chan<- sourceKey) {
	controller.listAndEnqueue(ctx, keys)
	ticker := time.NewTicker(controller.resync)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			controller.listAndEnqueue(ctx, keys)
		}
	}
}

func (controller *Controller) listAndEnqueue(ctx context.Context, keys chan<- sourceKey) {
	sources, err := controller.client.Resource(api.SourceGVR).Namespace(metav1.NamespaceAll).List(ctx, metav1.ListOptions{})
	if err != nil {
		log.Printf("list AzureEventGridSources: %v", err)
		return
	}
	for index := range sources.Items {
		source := &sources.Items[index]
		enqueue(ctx, keys, sourceKey{namespace: source.GetNamespace(), name: source.GetName()})
	}
}

func (controller *Controller) scheduleRetry(
	ctx context.Context,
	keys chan<- sourceKey,
	key sourceKey,
) {
	controller.retryMu.Lock()
	state := controller.retries[key]
	if state == nil {
		state = &retryState{}
		controller.retries[key] = state
	}
	if state.scheduled {
		controller.retryMu.Unlock()
		return
	}
	state.attempt++
	state.scheduled = true
	delay := time.Second << min(state.attempt-1, 5)
	controller.retryMu.Unlock()

	go func(expected *retryState) {
		if !sleep(ctx, delay) {
			return
		}
		controller.retryMu.Lock()
		current, found := controller.retries[key]
		if !found || current != expected || !current.scheduled {
			controller.retryMu.Unlock()
			return
		}
		current.scheduled = false
		controller.retryMu.Unlock()
		enqueue(ctx, keys, key)
	}(state)
}

func (controller *Controller) clearRetry(key sourceKey) {
	controller.retryMu.Lock()
	delete(controller.retries, key)
	controller.retryMu.Unlock()
}

func enqueue(ctx context.Context, keys chan<- sourceKey, key sourceKey) {
	select {
	case keys <- key:
	case <-ctx.Done():
	default:
		log.Printf("reconcile queue full; source %s/%s will be picked up by resync", key.namespace, key.name)
	}
}

func sleep(ctx context.Context, duration time.Duration) bool {
	timer := time.NewTimer(duration)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}

func (key sourceKey) String() string {
	return fmt.Sprintf("%s/%s", key.namespace, key.name)
}
