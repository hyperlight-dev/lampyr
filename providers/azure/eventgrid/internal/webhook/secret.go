// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package webhook

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/types"
)

const (
	SecretNamespace             = "lampyr-system"
	SecretName                  = "eventgrid-webhook-auth"
	KeyAcknowledgementConfigMap = "eventgrid-adapter-webhook-key"
	KeyDigestDataKey            = "sha256"
	secretDataKey               = "key"
	keySize                     = 32
)

func KeyDigest(key []byte) string {
	digest := sha256.Sum256(key)
	return hex.EncodeToString(digest[:])
}

type secretClient interface {
	Get(context.Context, string, metav1.GetOptions) (*corev1.Secret, error)
	Patch(context.Context, string, types.PatchType, []byte, metav1.PatchOptions, ...string) (*corev1.Secret, error)
}

func EnsureKey(ctx context.Context, secrets secretClient) ([]byte, error) {
	return ensureKey(ctx, secrets, rand.Reader)
}

func ensureKey(ctx context.Context, secrets secretClient, random io.Reader) ([]byte, error) {
	secret, err := secrets.Get(ctx, SecretName, metav1.GetOptions{})
	if err == nil {
		if key := secret.Data[secretDataKey]; len(key) != 0 {
			return keyFrom(secret)
		}
	}
	if err != nil {
		if apierrors.IsNotFound(err) {
			return nil, fmt.Errorf(
				"Event Grid webhook authentication Secret %s/%s is missing; reapply the cluster runtime",
				SecretNamespace,
				SecretName,
			)
		}
		return nil, fmt.Errorf("get Event Grid webhook authentication Secret: %w", err)
	}

	key := make([]byte, keySize)
	if _, err := io.ReadFull(random, key); err != nil {
		return nil, fmt.Errorf("generate Event Grid webhook authentication key: %w", err)
	}
	desired := corev1.Secret{
		TypeMeta: metav1.TypeMeta{APIVersion: "v1", Kind: "Secret"},
		ObjectMeta: metav1.ObjectMeta{
			Name:      SecretName,
			Namespace: SecretNamespace,
		},
		Type: corev1.SecretTypeOpaque,
		Data: map[string][]byte{secretDataKey: key},
	}
	payload, err := json.Marshal(&desired)
	if err != nil {
		return nil, fmt.Errorf("encode Event Grid webhook authentication Secret: %w", err)
	}
	digest := sha256.Sum256(key)
	secret, err = secrets.Patch(
		ctx,
		SecretName,
		types.ApplyPatchType,
		payload,
		metav1.PatchOptions{
			FieldManager: fmt.Sprintf("lampyr-eventgrid-controller-%x", digest[:8]),
		},
	)
	if err == nil {
		return keyFrom(secret)
	}
	if !apierrors.IsConflict(err) {
		return nil, fmt.Errorf("populate Event Grid webhook authentication Secret: %w", err)
	}
	secret, err = secrets.Get(ctx, SecretName, metav1.GetOptions{})
	if err != nil {
		return nil, fmt.Errorf("read concurrently populated Event Grid webhook authentication Secret: %w", err)
	}
	return keyFrom(secret)
}

func keyFrom(secret *corev1.Secret) ([]byte, error) {
	key := secret.Data[secretDataKey]
	if len(key) != keySize {
		return nil, fmt.Errorf(
			"Event Grid webhook authentication Secret %s/%s must contain a %d-byte %q",
			SecretNamespace,
			SecretName,
			keySize,
			secretDataKey,
		)
	}
	return append([]byte(nil), key...), nil
}
