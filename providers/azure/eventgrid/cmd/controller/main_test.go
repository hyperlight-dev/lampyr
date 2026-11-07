// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"context"
	"strings"
	"testing"

	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/client-go/kubernetes/fake"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/webhook"
)

func TestAcknowledgedWebhookKeyWaitsForProjectedAdapterKey(t *testing.T) {
	key := []byte("abcdefghijklmnopqrstuvwxyzABCDEF")
	client := fake.NewSimpleClientset(
		&corev1.Secret{
			ObjectMeta: metav1.ObjectMeta{
				Name:      webhook.SecretName,
				Namespace: webhook.SecretNamespace,
			},
			Data: map[string][]byte{"key": key},
		},
		&corev1.ConfigMap{
			ObjectMeta: metav1.ObjectMeta{
				Name:      webhook.KeyAcknowledgementConfigMap,
				Namespace: webhook.SecretNamespace,
			},
			Data: map[string]string{
				webhook.KeyDigestDataKey: webhook.KeyDigest(
					[]byte("01234567890123456789012345678901"),
				),
			},
		},
	)
	secrets := client.CoreV1().Secrets(webhook.SecretNamespace)
	configMaps := client.CoreV1().ConfigMaps(webhook.SecretNamespace)
	_, err := acknowledgedWebhookKey(
		context.Background(),
		secrets,
		configMaps,
	)
	if err == nil || !strings.Contains(err.Error(), "has not acknowledged") {
		t.Fatalf("lagging projection error = %v", err)
	}
	acknowledgement, err := configMaps.Get(
		context.Background(),
		webhook.KeyAcknowledgementConfigMap,
		metav1.GetOptions{},
	)
	if err != nil {
		t.Fatal(err)
	}
	acknowledgement.Data[webhook.KeyDigestDataKey] = webhook.KeyDigest(key)
	if _, err := configMaps.Update(
		context.Background(),
		acknowledgement,
		metav1.UpdateOptions{},
	); err != nil {
		t.Fatal(err)
	}
	acknowledged, err := acknowledgedWebhookKey(
		context.Background(),
		secrets,
		configMaps,
	)
	if err != nil {
		t.Fatal(err)
	}
	if string(acknowledged) != string(key) {
		t.Fatal("acknowledged key does not match the Secret")
	}
}
