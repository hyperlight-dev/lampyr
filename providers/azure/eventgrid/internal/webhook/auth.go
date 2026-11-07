// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package webhook

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/base64"
)

func Token(key []byte, sourceUID string) string {
	mac := hmac.New(sha256.New, key)
	_, _ = mac.Write([]byte(sourceUID))
	return base64.RawURLEncoding.EncodeToString(mac.Sum(nil))
}

func Authenticate(key []byte, sourceUID, token string) bool {
	expected := Token(key, sourceUID)
	return hmac.Equal([]byte(expected), []byte(token))
}
