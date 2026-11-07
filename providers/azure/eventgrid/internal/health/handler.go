// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package health

import (
	"net/http"
	"sync/atomic"
)

type State struct {
	ready atomic.Bool
}

func (state *State) SetReady(ready bool) {
	state.ready.Store(ready)
}

func (state *State) Ready() bool {
	return state.ready.Load()
}

func NewHandler(ready func() bool) http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/healthz", func(response http.ResponseWriter, _ *http.Request) {
		response.WriteHeader(http.StatusOK)
	})
	mux.HandleFunc("/readyz", func(response http.ResponseWriter, _ *http.Request) {
		if !ready() {
			http.Error(response, "not ready", http.StatusServiceUnavailable)
			return
		}
		response.WriteHeader(http.StatusOK)
	})
	return mux
}
