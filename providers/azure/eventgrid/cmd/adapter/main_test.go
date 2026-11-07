// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes/fake"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

func TestWaitForWebhookAuthKeyHandlesProjectedSecretPopulation(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "webhook-key")
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	go func() {
		time.Sleep(20 * time.Millisecond)
		_ = os.WriteFile(
			path,
			[]byte("01234567890123456789012345678901"),
			0o600,
		)
	}()
	authKey, err := waitForWebhookAuthKey(ctx, path, time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}

	if len(authKey) != 32 {
		t.Fatalf("authentication key length = %d", len(authKey))
	}
}

func TestAdapterPublishesMountedWebhookKeyAcknowledgement(t *testing.T) {
	client := fake.NewSimpleClientset()
	configMaps := client.CoreV1().ConfigMaps(webhook.SecretNamespace)
	key := []byte("01234567890123456789012345678901")
	if err := publishWebhookKeyAcknowledgement(
		context.Background(),
		configMaps,
		webhook.KeyDigest(key),
	); err != nil {
		t.Fatal(err)
	}
	acknowledgement, err := configMaps.Get(
		context.Background(),
		webhook.KeyAcknowledgementConfigMap,
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	if acknowledgement.Data[webhook.KeyDigestDataKey] != webhook.KeyDigest(key) {
		t.Fatalf("acknowledgement = %#v", acknowledgement.Data)
	}
}
