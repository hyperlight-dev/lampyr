// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"errors"
	"sync/atomic"
)

var disconnected atomic.Bool

var Service = Hooks{
	Handle: func(_ Request, _ Context) (Response, error) {
		return JSON(map[string]bool{"disconnected": disconnected.Load()})
	},
	HandleWebSocket: func(session *WebSocketSession, ctx ProtocolContext) error {
		if ctx.Request.Path == "/cancel" {
			<-ctx.Done()
			disconnected.Store(true)
			return ctx.Err()
		}
		if ctx.Request.Path == "/complete" {
			return nil
		}
		if ctx.Request.Path == "/fail" {
			return errors.New("websocket handler failed")
		}
		if ctx.Request.Path == "/panic" {
			panic("websocket handler panicked")
		}
		if ctx.Request.Path == "/close-panic" {
			if err := session.Close(4000, "finished"); err != nil {
				return err
			}
			panic("websocket handler panicked after closing")
		}
		if ctx.Request.Path == "/close" {
			return session.Close(4000, "finished")
		}
		for {
			message, err := session.Receive()
			if err != nil {
				return err
			}
			if err := session.Send(message); err != nil {
				return err
			}
		}
	},
}
