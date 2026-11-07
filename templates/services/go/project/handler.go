package main

import (
	"encoding/json"
	"io"
	"log"
	"net/http"
	"os"
	"strings"
	"time"
)

var Service = Hooks{Handle: Handle, HandleEvent: HandleEvent, HandleWebSocket: HandleWebSocket}

func Handle(request Request, context Context) (Response, error) {
	handlerLog("request received", request.Path, nil)
	if request.Path == "/stream" {
		return Response{
			Headers: http.Header{"Content-Type": {"application/x-ndjson"}},
			Body:    io.NopCloser(strings.NewReader("{\"chunk\":1}\n{\"chunk\":2}\n{\"chunk\":3}\n")),
		}, nil
	}
	return JSON(map[string]any{
		"message": "hello from Go on Hyperlight edge", "path": request.Path,
	})
}

func HandleEvent(event Event, context ProtocolContext) (Response, error) {
	path := context.Request.Path
	handlerLog("event received", path, event)
	status := 200
	if path == "/events/fail" {
		status = 500
	} else if path == "/events/knative" {
		status = 204
	}
	response, err := JSON(map[string]any{
		"message": "hello from Go on Hyperlight edge", "path": path, "event": event,
	})
	response.Status = status
	return response, err
}

func HandleWebSocket(session *WebSocketSession, context ProtocolContext) error {
	if context.Request.Path != "/ws" {
		return session.Close(1008, "not found")
	}
	for context.Err() == nil {
		message, err := session.Receive()
		if err != nil {
			return err
		}
		if err := session.Send(message); err != nil {
			return err
		}
	}
	return context.Err()
}

func handlerLog(message, path string, event any) {
	data, err := json.Marshal(map[string]any{
		"timestamp": time.Now().UTC().Format(time.RFC3339Nano),
		"kind":      "handler", "file": "handler.go", "service": "hello-go",
		"message": message, "path": path, "event": event,
	})
	if err != nil {
		log.Printf("handler log: %v", err)
		return
	}
	if _, err := os.Stdout.Write(append(data, '\n')); err != nil {
		log.Printf("handler log: %v", err)
	}
}
