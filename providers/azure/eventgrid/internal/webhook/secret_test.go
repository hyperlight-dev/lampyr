// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package webhook

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"testing"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/types"
)

type fakeSecrets struct {
	gets         []*corev1.Secret
	getErrors    []error
	patchError   error
	patchType    types.PatchType
	patchName    string
	patchPayload []byte
	patchOptions metav1.PatchOptions
	getCalls     int
}

func (fake *fakeSecrets) Get(
	_ context.Context,
	_ string,
	_ metav1.GetOptions,
) (*corev1.Secret, error) {
	index := fake.getCalls
	fake.getCalls++
	if index < len(fake.getErrors) && fake.getErrors[index] != nil {
		return nil, fake.getErrors[index]
	}
	return fake.gets[index].DeepCopy(), nil
}

func (fake *fakeSecrets) Patch(
	_ context.Context,
	name string,
	patchType types.PatchType,
	payload []byte,
	options metav1.PatchOptions,
	_ ...string,
) (*corev1.Secret, error) {
	fake.patchName = name
	fake.patchType = patchType
	fake.patchPayload = append([]byte(nil), payload...)
	fake.patchOptions = options
	if fake.patchError != nil {
		return nil, fake.patchError
	}
	var secret corev1.Secret
	if err := json.Unmarshal(payload, &secret); err != nil {
		return nil, err
	}
	return &secret, nil
}

func TestEnsureKeyReusesExistingSecret(t *testing.T) {
	expected := bytes.Repeat([]byte{7}, keySize)
	fake := &fakeSecrets{gets: []*corev1.Secret{{Data: map[string][]byte{secretDataKey: expected}}}}
	key, err := ensureKey(context.Background(), fake, errorReader{})
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(key, expected) {
		t.Fatalf("key = %x, want %x", key, expected)
	}
	if fake.patchName != "" {
		t.Fatal("existing Secret was patched")
	}
}

func TestEnsureKeyPopulatesEmptySecretWithNameRestrictedApply(t *testing.T) {
	expected := bytes.Repeat([]byte{9}, keySize)
	fake := &fakeSecrets{gets: []*corev1.Secret{{}}}
	key, err := ensureKey(context.Background(), fake, bytes.NewReader(expected))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(key, expected) {
		t.Fatalf("key = %x, want %x", key, expected)
	}
	if fake.patchName != SecretName || fake.patchType != types.ApplyPatchType {
		t.Fatalf("patch = %s %s", fake.patchType, fake.patchName)
	}
	if fake.patchOptions.FieldManager == "" || fake.patchOptions.Force != nil {
		t.Fatalf("patch options = %#v", fake.patchOptions)
	}
	var secret corev1.Secret
	if err := json.Unmarshal(fake.patchPayload, &secret); err != nil {
		t.Fatal(err)
	}
	if secret.Namespace != SecretNamespace || secret.Type != corev1.SecretTypeOpaque ||
		!bytes.Equal(secret.Data[secretDataKey], expected) {
		t.Fatalf("populated Secret = %#v", secret)
	}
}

func TestEnsureKeyReusesConcurrentWinner(t *testing.T) {
	conflict := apierrors.NewConflict(
		schema.GroupResource{Resource: "secrets"},
		SecretName,
		errors.New("field is already owned"),
	)
	winner := bytes.Repeat([]byte{3}, keySize)
	fake := &fakeSecrets{
		gets:       []*corev1.Secret{{}, {Data: map[string][]byte{secretDataKey: winner}}},
		patchError: conflict,
	}
	key, err := ensureKey(context.Background(), fake, bytes.NewReader(bytes.Repeat([]byte{4}, keySize)))
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(key, winner) {
		t.Fatalf("key = %x, want concurrent winner %x", key, winner)
	}
}

func TestEnsureKeyRejectsMissingBootstrapSecret(t *testing.T) {
	notFound := apierrors.NewNotFound(schema.GroupResource{Resource: "secrets"}, SecretName)
	fake := &fakeSecrets{getErrors: []error{notFound}}
	if _, err := ensureKey(context.Background(), fake, bytes.NewReader(nil)); err == nil {
		t.Fatal("missing bootstrap Secret was accepted")
	}
	if fake.patchName != "" {
		t.Fatal("missing bootstrap Secret was patched")
	}
}

func TestEnsureKeyRejectsInvalidSecret(t *testing.T) {
	fake := &fakeSecrets{gets: []*corev1.Secret{{Data: map[string][]byte{secretDataKey: []byte("short")}}}}
	if _, err := ensureKey(context.Background(), fake, bytes.NewReader(nil)); err == nil {
		t.Fatal("invalid Secret was accepted")
	}
}

func TestEnsureKeyReportsEntropyFailure(t *testing.T) {
	fake := &fakeSecrets{gets: []*corev1.Secret{{}}}
	if _, err := ensureKey(context.Background(), fake, errorReader{}); err == nil {
		t.Fatal("entropy failure was ignored")
	}
	if fake.patchName != "" {
		t.Fatal("Secret was patched without a generated key")
	}
}

type errorReader struct{}

func (errorReader) Read([]byte) (int, error) {
	return 0, errors.New("entropy unavailable")
}
