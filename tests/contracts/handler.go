// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"errors"
	"io"
	"net/http"
	"os"
	"reflect"
	"strconv"
	"strings"
	"sync"
	"time"
)

var Service = Hooks{Handle: Handle}

var (
	closedMu     sync.Mutex
	closedBodies = []string{}
)

type closeRecorder struct{ path string }

func (*closeRecorder) Read([]byte) (int, error) {
	return 0, errors.New("bodyless response source was read")
}

func (r *closeRecorder) Close() error {
	closedMu.Lock()
	closedBodies = append(closedBodies, r.path)
	closedMu.Unlock()
	return nil
}

func Handle(request Request, context Context) (Response, error) {
	switch request.Path {
	case "/http-context":
		_, exists := reflect.TypeOf(context).FieldByName("Request")
		return JSON(map[string]bool{"requestInContext": exists})
	case "/upload-stream":
		return Response{Body: request.Body}, nil
	case "/own-nosniff":
		response := Text("own")
		response.Headers.Set("X-Content-Type-Options", "handler")
		return response, nil
	case "/repeated-headers":
		response, err := JSON(map[string]any{"values": request.Headers.Values("X-Repeat")})
		response.Headers["Set-Cookie"] = []string{"a=1", "b=2"}
		return response, err
	case "/json-body":
		var value any
		if err := request.JSON(&value); err != nil {
			return Response{}, err
		}
		return JSON(value)
	case "/value":
		response, err := JSON(map[string]bool{"ok": true})
		response.Status = 201
		response.Headers.Set("x-contract", "value")
		return response, err
	case "/bytes":
		response := Bytes([]byte("raw\x00bytes"))
		response.Headers.Set("Content-Type", "application/octet-stream")
		return response, nil
	case "/text":
		response := Text("plain text")
		response.Headers.Set("Content-Type", "text/plain")
		return response, nil
	case "/array":
		return JSON([]int{1, 2, 3})
	case "/stream":
		return Response{Body: io.NopCloser(strings.NewReader("one\ntwo\nthree\n"))}, nil
	case "/typed-error":
		return Response{}, &HandlerError{422, "<bad>\r\nX-Injected: yes"}
	case "/error":
		panic("private details")
	case "/serialize-error":
		return JSON(make(chan int))
	case "/empty":
		return Response{Status: 204, Body: Text("must not appear").Body}, nil
	case "/headers":
		return Response{Headers: http.Header{"Content-Length": {"999"}}, Body: Text("bad").Body}, nil
	case "/unicode-header":
		return Response{Headers: http.Header{"X-Locale": {"snowman \u2603"}}, Body: Text("bad").Body}, nil
	case "/closed-bodies":
		closedMu.Lock()
		defer closedMu.Unlock()
		return JSON(closedBodies)
	case "/close-head", "/close-204", "/close-304":
		status := map[string]int{"/close-204": 204, "/close-304": 304}[request.Path]
		return Response{Status: status, Body: &closeRecorder{path: request.Path}}, nil
	case "/not-modified":
		return Response{Status: 304, Body: Text("must not appear").Body}, nil
	case "/broken-stream", "/cancel", "/backpressure":
		return Response{Body: &contractStream{mode: request.Path, context: context}}, nil
	}
	body, err := request.Text()
	if err != nil {
		return Response{}, err
	}
	return JSON(map[string]string{
		"method": request.Method, "path": request.Path,
		"body": body, "header": request.Headers.Get("X-Test"),
	})
}

type contractStream struct {
	mode    string
	context Context
	offset  int
}

func (s *contractStream) Read(data []byte) (int, error) {
	if err := s.context.Err(); err != nil {
		return 0, err
	}
	switch s.mode {
	case "/broken-stream":
		if s.offset != 0 {
			return 0, errors.New("stream failed")
		}
		s.offset++
		return copy(data, "first"), nil
	case "/backpressure":
		if s.offset >= 128*1024*1024 {
			return 0, io.EOF
		}
		if s.offset%(1024*1024) == 0 {
			if err := os.WriteFile("produced", []byte(strconv.Itoa(s.offset/(1024*1024)+1)), 0600); err != nil {
				return 0, err
			}
		}
	case "/cancel":
		time.Sleep(5 * time.Millisecond)
	}
	for index := range data {
		data[index] = 'x'
	}
	s.offset += len(data)
	return len(data), nil
}

func (s *contractStream) Close() error {
	if s.mode == "/cancel" {
		return os.WriteFile("cancelled", []byte("closed"), 0600)
	}
	return nil
}
