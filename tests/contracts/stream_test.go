// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"bytes"
	"context"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

type blockingBody struct {
	reader  *io.PipeReader
	reading chan struct{}
	closing chan struct{}
	release chan struct{}
	closes  atomic.Int32
}

func (body *blockingBody) Read(data []byte) (int, error) {
	close(body.reading)
	return body.reader.Read(data)
}

func (body *blockingBody) Close() error {
	body.closes.Add(1)
	err := body.reader.Close()
	close(body.closing)
	<-body.release
	return err
}

func await(t *testing.T, channel <-chan struct{}) {
	t.Helper()
	select {
	case <-channel:
	case <-time.After(3 * time.Second):
		t.Fatal("response stream did not finish its lifecycle operation")
	}
}

func TestBlockedResponseClosesOnCancellation(t *testing.T) {
	reader, writer := io.Pipe()
	defer writer.Close()
	body := &blockingBody{
		reader: reader, reading: make(chan struct{}),
		closing: make(chan struct{}), release: make(chan struct{}),
	}
	defer close(body.release)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	request := httptest.NewRequest("GET", "/", nil).WithContext(ctx)
	finished := make(chan struct{})
	go func() {
		defer close(finished)
		started := false
		_ = writeResponse(httptest.NewRecorder(), request, Response{Body: body}, &started)
	}()
	await(t, body.reading)
	cancel()
	await(t, body.closing)
	select {
	case <-finished:
		t.Fatal("response returned before its body finished closing")
	case <-time.After(20 * time.Millisecond):
	}
	body.release <- struct{}{}
	await(t, finished)
	if count := body.closes.Load(); count != 1 {
		t.Fatalf("body closed %d times, want once", count)
	}
}

type panickingBody struct {
	reader  *io.PipeReader
	reading chan struct{}
	closed  chan struct{}
}

func (body *panickingBody) Read(data []byte) (int, error) {
	close(body.reading)
	return body.reader.Read(data)
}

func (body *panickingBody) Close() error {
	defer close(body.closed)
	_ = body.reader.Close()
	panic("close failed")
}

func TestPanickingCloseOnCancellationIsContained(t *testing.T) {
	reader, writer := io.Pipe()
	defer writer.Close()
	body := &panickingBody{reader: reader, reading: make(chan struct{}), closed: make(chan struct{})}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	request := httptest.NewRequest("GET", "/", nil).WithContext(ctx)
	finished := make(chan struct{})
	go func() {
		defer close(finished)
		started := false
		_ = writeResponse(httptest.NewRecorder(), request, Response{Body: body}, &started)
	}()
	await(t, body.reading)
	cancel()
	await(t, body.closed)
	await(t, finished)
}

type countedBody struct {
	*strings.Reader
	closes int
}

func (body *countedBody) Close() error {
	body.closes++
	return nil
}

func TestResponseClosesOnceOnCompletion(t *testing.T) {
	for _, status := range []int{200, 204, 304, 999} {
		for _, cancelled := range []bool{false, true} {
			body := &countedBody{Reader: strings.NewReader("body")}
			ctx, cancel := context.WithCancel(context.Background())
			if cancelled {
				cancel()
			}

			request := httptest.NewRequest("GET", "/", nil).WithContext(ctx)
			started := false
			_ = writeResponse(httptest.NewRecorder(), request, Response{Status: status, Body: body}, &started)
			cancel()
			if body.closes != 1 {
				t.Fatalf("status %d cancelled %v: body closed %d times", status, cancelled, body.closes)
			}
		}
	}
}

func TestBoundedHelpersPreserveStreamFailures(t *testing.T) {
	request := Request{Body: io.NopCloser(bytes.NewReader(make([]byte, bodyLimit)))}
	data, err := request.ReadAll()
	if err != nil || len(data) != bodyLimit {
		t.Fatalf("boundary read: len %d, error %v", len(data), err)
	}

	request.Body = io.NopCloser(bytes.NewReader(make([]byte, bodyLimit+1)))
	_, err = request.ReadAll()
	var typed *HandlerError
	if !errors.As(err, &typed) || typed.Status != 413 {
		t.Fatalf("oversize helper must fail with status 413, got %v", err)
	}
	failure := errors.New("transport failed")
	reader, writer := io.Pipe()
	if err := writer.CloseWithError(failure); err != nil {
		t.Fatal(err)
	}
	request.Body = reader
	if _, err := request.ReadAll(); !errors.Is(err, failure) {
		t.Fatalf("lost stream error: %v", err)
	}
	request.Body = io.NopCloser(strings.NewReader("{"))
	var value any
	if err := request.JSON(&value); !errors.As(err, &typed) || typed.Status != 400 {
		t.Fatalf("invalid JSON must fail with status 400, got %v", err)
	}
	request.Body = io.NopCloser(bytes.NewReader([]byte{'"', 0xff, '"'}))
	if err := request.JSON(&value); !errors.As(err, &typed) || typed.Status != 400 {
		t.Fatalf("invalid UTF-8 JSON must fail with status 400, got %v", err)
	}
}

func TestBytesResponseAllowsHeaders(t *testing.T) {
	payload := []byte{0x00, 0xff, 0x80}
	response := Bytes(payload)
	defer response.Body.Close()
	response.Headers.Set("Content-Type", "application/octet-stream")
	response.Headers.Add("Set-Cookie", "a=1")
	response.Headers.Add("Set-Cookie", "b=2")
	if response.Headers.Get("Content-Type") != "application/octet-stream" || len(response.Headers.Values("Set-Cookie")) != 2 {
		t.Fatal("binary response did not preserve custom headers")
	}
	data, err := io.ReadAll(response.Body)
	if err != nil || !bytes.Equal(data, payload) {
		t.Fatalf("binary response changed: data %x error %v", data, err)
	}
	other := Bytes(nil)
	defer other.Body.Close()
	other.Headers.Set("Content-Type", "application/custom")
	if response.Headers.Get("Content-Type") != "application/octet-stream" {
		t.Fatal("binary responses share headers")
	}
}

func TestWebSocketDisconnectCancelsIdleHook(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	started, finished := make(chan struct{}), make(chan struct{})
	Service.HandleWebSocket = func(_ *WebSocketSession, ctx ProtocolContext) error {
		close(started)
		<-ctx.Done()
		close(finished)
		return ctx.Err()
	}
	server := httptest.NewServer(http.HandlerFunc(serve))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	await(t, started)
	if err := connection.Close(); err != nil {
		t.Fatal(err)
	}
	await(t, finished)
}

func TestWebSocketSendToStalledPeerTimesOut(t *testing.T) {
	original, timeout := Service, webSocketWriteTimeout
	defer func() { Service, webSocketWriteTimeout = original, timeout }()
	webSocketWriteTimeout = 200 * time.Millisecond
	failed := make(chan error, 1)
	Service.HandleWebSocket = func(session *WebSocketSession, _ ProtocolContext) error {
		chunk := make([]byte, 64<<10)
		for {
			if err := session.Send(Message{Type: BinaryMessage, Data: chunk}); err != nil {
				failed <- err
				return err
			}
		}
	}
	server := httptest.NewServer(http.HandlerFunc(serve))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	select {
	case <-failed:
	case <-time.After(10 * time.Second):
		t.Fatal("Send to a peer that never reads did not time out")
	}
}

func TestShutdownSendsGoingAwayAndWaitsForWebSocketHooks(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	started, cleaned := make(chan struct{}), make(chan struct{})
	Service.HandleWebSocket = func(_ *WebSocketSession, ctx ProtocolContext) error {
		close(started)
		<-ctx.Done()
		time.Sleep(100 * time.Millisecond) // hook cleanup after cancellation
		close(cleaned)
		return ctx.Err()
	}
	base, shutdown := context.WithCancel(context.Background())
	defer shutdown()
	server := httptest.NewUnstartedServer(http.HandlerFunc(serve))
	server.Config.BaseContext = func(net.Listener) context.Context { return base }
	server.Start()
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	await(t, started)
	shutdown()
	if _, _, err := connection.ReadMessage(); !websocket.IsCloseError(err, websocket.CloseGoingAway) {
		t.Fatalf("got %v, want a 1001 close frame", err)
	}
	waited := make(chan struct{})
	go func() {
		webSockets.Wait()
		close(waited)
	}()
	await(t, waited)
	select {
	case <-cleaned:
	default:
		t.Fatal("webSockets.Wait returned before the hook finished its cleanup")
	}
}

func TestShutdownGoingAwayWhenHookReturnsImmediately(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	for i := 0; i < 50; i++ {
		started := make(chan struct{})
		Service.HandleWebSocket = func(session *WebSocketSession, ctx ProtocolContext) error {
			close(started)
			_, err := session.Receive()
			return err
		}
		base, shutdown := context.WithCancel(context.Background())
		server := httptest.NewUnstartedServer(http.HandlerFunc(serve))
		server.Config.BaseContext = func(net.Listener) context.Context { return base }
		server.Start()
		connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
		if err != nil {
			t.Fatal(err)
		}
		await(t, started)
		shutdown()
		_, _, err = connection.ReadMessage()
		connection.Close()
		server.Close()
		if !websocket.IsCloseError(err, websocket.CloseGoingAway) {
			t.Fatalf("iteration %d: got %v, want a 1001 close frame", i, err)
		}
	}
}

func TestHookCloseDuringShutdownReachesPeer(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	for i := 0; i < 50; i++ {
		started := make(chan struct{})
		Service.HandleWebSocket = func(session *WebSocketSession, ctx ProtocolContext) error {
			close(started)
			<-ctx.Done()
			return session.Close(4000, "bye")
		}
		base, shutdown := context.WithCancel(context.Background())
		server := httptest.NewUnstartedServer(http.HandlerFunc(serve))
		server.Config.BaseContext = func(net.Listener) context.Context { return base }
		server.Start()
		connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
		if err != nil {
			t.Fatal(err)
		}
		await(t, started)
		shutdown()
		_, _, err = connection.ReadMessage()
		connection.Close()
		server.Close()
		if !websocket.IsCloseError(err, 4000, websocket.CloseGoingAway) {
			t.Fatalf("iteration %d: got %v, want the hook's 4000 or 1001", i, err)
		}
	}
}

func TestRejectedHookCloseStillSendsAdapterCloseFrame(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	Service.HandleWebSocket = func(session *WebSocketSession, _ ProtocolContext) error {
		return session.Close(4000, strings.Repeat("x", 200))
	}
	server := httptest.NewServer(http.HandlerFunc(serve))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, _, err := connection.ReadMessage(); !websocket.IsCloseError(err, websocket.CloseInternalServerErr) {
		t.Fatalf("got %v, want the adapter's 1011 close frame", err)
	}
}

func TestAbsoluteFormTargetUsesOriginPath(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	var seen string
	Service.Handle = func(request Request, _ Context) (Response, error) {
		seen = request.Path
		return Response{Status: 204}, nil
	}
	server := httptest.NewServer(http.HandlerFunc(serve))
	defer server.Close()
	connection, err := net.Dial("tcp", strings.TrimPrefix(server.URL, "http://"))
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, err := io.WriteString(connection, "GET http://svc/admin/x?q=1 HTTP/1.1\r\nHost: svc\r\nConnection: close\r\n\r\n"); err != nil {
		t.Fatal(err)
	}
	if _, err := io.ReadAll(connection); err != nil {
		t.Fatal(err)
	}
	if seen != "/admin/x?q=1" {
		t.Fatalf("Path = %q, want /admin/x?q=1", seen)
	}
}

func TestWebSocketAbortHandlerSkipsCloseFrame(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	finished := make(chan struct{})
	Service.HandleWebSocket = func(_ *WebSocketSession, _ ProtocolContext) error {
		panic(http.ErrAbortHandler)
	}
	server := httptest.NewServer(http.HandlerFunc(func(output http.ResponseWriter, input *http.Request) {
		defer close(finished)
		serve(output, input)
	}))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if err := connection.SetReadDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	_, data, err := connection.ReadMessage()
	await(t, finished)
	if !websocket.IsCloseError(err, websocket.CloseAbnormalClosure) {
		t.Fatalf("ErrAbortHandler must abort without a close frame, got data %x error %v", data, err)
	}
}

func TestWebSocketDisconnectAfterQueueSaturationCancelsIdleHook(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	started, finished := make(chan struct{}), make(chan struct{})
	Service.HandleWebSocket = func(_ *WebSocketSession, ctx ProtocolContext) error {
		close(started)
		<-ctx.Done()
		close(finished)
		return ctx.Err()
	}
	server := httptest.NewServer(http.HandlerFunc(serve))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	await(t, started)
	for range receiveQueue {
		if err := connection.WriteMessage(websocket.TextMessage, []byte("queued")); err != nil {
			t.Fatal(err)
		}
	}
	if err := connection.Close(); err != nil {
		t.Fatal(err)
	}
	await(t, finished)
}

func TestWebSocketOverloadClosesSession(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	finished := make(chan struct{})
	Service.HandleWebSocket = func(_ *WebSocketSession, ctx ProtocolContext) error {
		<-ctx.Done()
		close(finished)
		return ctx.Err()
	}
	server := httptest.NewServer(http.HandlerFunc(serve))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	for range receiveQueue + 2 {
		if err := connection.WriteMessage(websocket.TextMessage, []byte("queued")); err != nil {
			break
		}
	}
	await(t, finished)
}

func TestWebSocketRejectsInvalidTextButPreservesBinary(t *testing.T) {
	original := Service
	defer func() { Service = original }()
	Service.HandleWebSocket = func(session *WebSocketSession, _ ProtocolContext) error {
		for {
			message, err := session.Receive()
			if err != nil {
				return err
			}
			if err := session.Send(message); err != nil {
				return err
			}
		}
	}
	server := httptest.NewServer(http.HandlerFunc(serve))
	defer server.Close()
	connection, _, err := websocket.DefaultDialer.Dial("ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if err := connection.SetReadDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	if err := connection.WriteMessage(websocket.BinaryMessage, []byte{0xff}); err != nil {
		t.Fatal(err)
	}
	kind, data, err := connection.ReadMessage()
	if err != nil || kind != websocket.BinaryMessage || !bytes.Equal(data, []byte{0xff}) {
		t.Fatalf("binary message changed: kind %d data %x error %v", kind, data, err)
	}
	if err := connection.WriteMessage(websocket.TextMessage, []byte{0xff}); err != nil {
		t.Fatal(err)
	}
	_, data, err = connection.ReadMessage()
	if !websocket.IsCloseError(err, websocket.CloseInvalidFramePayloadData) {
		t.Fatalf("invalid text must close with 1007, got data %x error %v", data, err)
	}
}
