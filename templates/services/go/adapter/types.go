package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unicode/utf8"

	"github.com/gorilla/websocket"
)

type RequestHead struct {
	Method  string
	Path    string
	Headers http.Header
}

type Request struct {
	RequestHead
	Body io.ReadCloser
}

type Context struct{ context.Context }

type ProtocolContext struct {
	context.Context
	Request RequestHead
}

func (request Request) ReadAll() ([]byte, error) {
	data, err := io.ReadAll(io.LimitReader(request.Body, bodyLimit+1))
	if err != nil {
		return nil, err
	}
	if len(data) > bodyLimit {
		return nil, &HandlerError{413, "request body too large"}
	}
	return data, nil
}

func (request Request) Text() (string, error) {
	data, err := request.ReadAll()
	if err == nil && !utf8.Valid(data) {
		return "", &HandlerError{400, "invalid UTF-8 body"}
	}
	return string(data), err
}

func (request Request) JSON(value any) error {
	data, err := request.ReadAll()
	if err != nil {
		return err
	}
	if !utf8.Valid(data) {
		return &HandlerError{400, "invalid UTF-8 body"}
	}
	if err := json.Unmarshal(data, value); err != nil {
		return &HandlerError{400, "invalid JSON body"}
	}
	return nil
}

type Response struct {
	Status  int
	Headers http.Header
	Body    io.ReadCloser
}

func Bytes(data []byte) Response {
	return Response{Headers: make(http.Header), Body: io.NopCloser(bytes.NewReader(data))}
}

func Text(text string) Response {
	return Response{
		Headers: http.Header{"Content-Type": {"text/plain; charset=utf-8"}},
		Body:    io.NopCloser(strings.NewReader(text)),
	}
}

func JSON(value any) (Response, error) {
	data, err := json.Marshal(value)
	if err != nil {
		return Response{}, err
	}
	response := Bytes(data)
	response.Headers.Set("Content-Type", "application/json")
	return response, nil
}

type Event struct {
	ID      string `json:"id"`
	Type    string `json:"type"`
	Source  string `json:"source"`
	Subject string `json:"subject"`
	Time    string `json:"time"`
	Data    any    `json:"data"`
	Attempt int    `json:"attempt"`
}

type HandlerError struct {
	Status  int
	Message string
}

func (e *HandlerError) Error() string { return fmt.Sprintf("%d: %s", e.Status, e.Message) }

type Hooks struct {
	Handle          func(Request, Context) (Response, error)
	HandleEvent     func(Event, ProtocolContext) (Response, error)
	HandleWebSocket func(*WebSocketSession, ProtocolContext) error
}

type WebSocketSession struct {
	connection *websocket.Conn
	context    context.Context
	messages   chan Message
	readError  error
	closed     atomic.Bool
	// finishing serializes adapter close frames so a concurrent finish
	// returns only after the winning frame is written.
	finishing sync.Mutex
}

type Message struct {
	Type int
	Data []byte
}

const (
	TextMessage   = websocket.TextMessage
	BinaryMessage = websocket.BinaryMessage
)

func (s *WebSocketSession) Receive() (Message, error) {
	select {
	case message, open := <-s.messages:
		if !open {
			return Message{}, s.readError
		}
		return message, nil
	case <-s.context.Done():
		return Message{}, s.context.Err()
	}
}

func (s *WebSocketSession) read(cancel context.CancelFunc) {
	defer cancel()
	defer close(s.messages)
	for {
		kind, data, err := s.connection.ReadMessage()
		if err == nil && kind == TextMessage && !utf8.Valid(data) {
			err = fmt.Errorf("invalid UTF-8 WebSocket text message")
			if closeErr := s.connection.WriteControl(
				websocket.CloseMessage,
				websocket.FormatCloseMessage(websocket.CloseInvalidFramePayloadData, "invalid UTF-8"),
				time.Now().Add(time.Second),
			); closeErr != nil {
				err = fmt.Errorf("%w: close frame: %v", err, closeErr)
			}
		}
		if err != nil {
			s.readError = err
			return
		}
		select {
		case s.messages <- Message{Type: kind, Data: data}:
		default:
			// Keep the socket reader live so peer disconnects still cancel idle hooks.
			s.readError = fmt.Errorf("WebSocket receive queue overloaded")
			_ = s.connection.WriteControl(
				websocket.CloseMessage,
				websocket.FormatCloseMessage(websocket.CloseTryAgainLater, "overloaded"),
				time.Now().Add(time.Second),
			)
			return
		}
	}
}

// webSocketWriteTimeout bounds each hook write so a peer that stops reading
// cannot pin the hook forever once TCP buffers fill.
var webSocketWriteTimeout = 30 * time.Second

func (s *WebSocketSession) Send(message Message) error {
	if err := s.connection.SetWriteDeadline(time.Now().Add(webSocketWriteTimeout)); err != nil {
		return err
	}
	return s.connection.WriteMessage(message.Type, message.Data)
}

func (s *WebSocketSession) Close(code int, reason string) error {
	// Hold finishing so a concurrent shutdown finish waits for this frame
	// instead of tearing the connection down underneath it.
	s.finishing.Lock()
	defer s.finishing.Unlock()
	err := s.connection.WriteControl(websocket.CloseMessage, websocket.FormatCloseMessage(code, reason),
		time.Now().Add(time.Second))
	// Only a sent frame closes the session; otherwise the adapter still owes
	// the peer one (e.g. a reason over 123 bytes is rejected).
	if err == nil {
		s.closed.Store(true)
	}
	return err
}

// finish sends the adapter's close frame unless the handler already closed the session.
func (s *WebSocketSession) finish(code int, reason string) {
	s.finishing.Lock()
	defer s.finishing.Unlock()
	if s.closed.Swap(true) {
		return
	}
	_ = s.connection.WriteControl(
		websocket.CloseMessage, websocket.FormatCloseMessage(code, reason), time.Now().Add(time.Second))
}
