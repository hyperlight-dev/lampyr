package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/gorilla/websocket"
)

const (
	bodyLimit    = 1024 * 1024
	receiveQueue = 16
)

type requestBody struct {
	io.Reader
	close func() error
}

func (body *requestBody) Close() error { return body.close() }

// webSockets counts hijacked connections whose hooks are still running.
var webSockets sync.WaitGroup

func main() {
	if Service.Handle == nil {
		log.Fatal("required Handle(request, context) hook is missing")
	}
	for _, arg := range os.Args[1:] {
		if arg == "--require-event" && Service.HandleEvent == nil {
			log.Fatal("events policy requires HandleEvent")
		}
	}
	for _, arg := range os.Args[1:] {
		if arg == "--check" {
			return
		}
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	port := os.Getenv("PORT")
	if port == "" {
		port = "8080"
	}
	host := os.Getenv("LAMPYR_LISTEN_HOST")
	if host == "" {
		host = "0.0.0.0"
	}
	server := &http.Server{
		Addr:              net.JoinHostPort(host, port),
		Handler:           http.HandlerFunc(serve),
		ReadHeaderTimeout: 30 * time.Second,
		ReadTimeout:       30 * time.Second,
		BaseContext:       func(net.Listener) context.Context { return ctx },
	}
	done := make(chan struct{})
	go func() {
		<-ctx.Done()
		deadline, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		if err := server.Shutdown(deadline); err != nil {
			log.Printf("shutdown: %v", err)
			if err := server.Close(); err != nil {
				log.Printf("close server: %v", err)
			}
		}
		// Shutdown does not track hijacked connections, so wait for WebSocket
		// hooks separately or main returns before their cleanup runs.
		hooks := make(chan struct{})
		go func() {
			webSockets.Wait()
			close(hooks)
		}()
		select {
		case <-hooks:
		case <-deadline.Done():
			log.Printf("shutdown: WebSocket hooks still running")
		}
		close(done)
	}()
	if err := server.ListenAndServe(); !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
	<-done
}

func serve(output http.ResponseWriter, input *http.Request) {
	started := false
	defer func() {
		if value := recover(); value != nil {
			if value == http.ErrAbortHandler {
				panic(value)
			}
			log.Printf("handler panic: %v", value)
			if started {
				panic(http.ErrAbortHandler)
			}
			http.Error(output, "request failed", 500)
		}
	}()
	path := input.RequestURI
	if input.URL.IsAbs() {
		// An absolute-form target must not leak scheme and authority into Path,
		// where handler prefix checks would fail open.
		path = input.URL.RequestURI()
	}
	head := RequestHead{Method: input.Method, Path: path, Headers: input.Header.Clone()}
	head.Headers.Set("Host", input.Host)
	limited := http.MaxBytesReader(output, input.Body, bodyLimit)
	request := Request{RequestHead: head, Body: &requestBody{Reader: limited, close: sync.OnceValue(limited.Close)}}
	defer request.Body.Close()
	ctx := Context{Context: input.Context()}
	if websocket.IsWebSocketUpgrade(input) {
		if Service.HandleWebSocket == nil {
			http.NotFound(output, input)
			return
		}
		// Register before the upgrade hijacks the connection, while Shutdown
		// still counts this request as active.
		webSockets.Add(1)
		defer webSockets.Done()
		connection, err := (&websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}).Upgrade(output, input, nil)
		if err != nil {
			log.Printf("websocket upgrade: %v", err)
			return
		}
		defer connection.Close()
		connection.SetReadLimit(bodyLimit)
		sessionContext, cancel := context.WithCancel(ctx.Context)
		defer cancel()
		session := &WebSocketSession{
			connection: connection, context: sessionContext, messages: make(chan Message, receiveQueue),
		}
		finished := make(chan struct{})
		go func() {
			defer close(finished)
			session.read(cancel)
		}()
		closing := make(chan struct{})
		stop := context.AfterFunc(sessionContext, func() {
			defer close(closing)
			// After the hijack only server shutdown cancels the request context.
			if ctx.Context.Err() != nil {
				session.finish(websocket.CloseGoingAway, "server shutdown")
			}
			connection.Close()
		})
		defer func() {
			cancel()
			// Let a running AfterFunc finish its close frame before the
			// connection is torn down underneath it.
			if !stop() {
				<-closing
			}
			connection.Close()
			<-finished
		}()
		started = true
		err = func() (err error) {
			defer func() {
				if value := recover(); value != nil {
					if value == http.ErrAbortHandler {
						panic(value)
					}
					err = fmt.Errorf("handler panic: %v", value)
				}
			}()
			return Service.HandleWebSocket(session, ProtocolContext{Context: sessionContext, Request: head})
		}()
		if err != nil {
			log.Printf("websocket handler: %v", err)
		}
		switch {
		case ctx.Context.Err() != nil:
			session.finish(websocket.CloseGoingAway, "server shutdown")
		case err != nil && sessionContext.Err() == nil:
			session.finish(websocket.CloseInternalServerErr, "request failed")
		case err == nil:
			session.finish(websocket.CloseNormalClosure, "")
		}
		return
	}
	if input.ContentLength > bodyLimit {
		http.Error(output, "request body too large", 413)
		return
	}
	if err := http.NewResponseController(output).EnableFullDuplex(); err != nil {
		log.Printf("enable streaming request: %v", err)
		http.Error(output, "request failed", 500)
		return
	}
	var err error
	var response Response
	if isEvent(request) {
		if Service.HandleEvent == nil {
			http.NotFound(output, input)
			return
		}
		var event Event
		event, err = eventOf(request)
		if err == nil {
			response, err = Service.HandleEvent(event, ProtocolContext{Context: ctx.Context, Request: head})
		}
	} else {
		response, err = Service.Handle(request, ctx)
	}
	if err == nil {
		err = writeResponse(output, input, response, &started)
	}
	if err != nil {
		log.Printf("request: %v", err)
		if started {
			panic(http.ErrAbortHandler)
		}
		var typed *HandlerError
		var limit *http.MaxBytesError
		if errors.As(err, &limit) {
			http.Error(output, "request body too large", 413)
		} else if errors.As(err, &typed) && typed.Status >= 400 && typed.Status <= 599 {
			http.Error(output, typed.Message, typed.Status)
		} else {
			http.Error(output, "request failed", 500)
		}
	}
}

func writeResponse(output http.ResponseWriter, input *http.Request, response Response, started *bool) error {
	if closer := response.Body; closer != nil {
		closeBody := sync.OnceFunc(func() {
			// Cancellation runs this on an AfterFunc goroutine, outside serve's
			// recover, so a panicking Close would otherwise end the process.
			defer func() {
				if value := recover(); value != nil {
					log.Printf("close response stream panic: %v", value)
				}
			}()
			if err := closer.Close(); err != nil {
				log.Printf("close response stream: %v", err)
			}
		})
		stop := context.AfterFunc(input.Context(), closeBody)
		defer func() {
			stop()
			closeBody()
		}()
	}
	status := response.Status
	if status == 0 {
		status = 200
	}
	if status < 200 || status > 599 {
		return fmt.Errorf("invalid response status")
	}
	for name, values := range response.Headers {
		switch strings.ToLower(name) {
		case "content-length", "transfer-encoding", "connection":
			return fmt.Errorf("framing headers are adapter-owned")
		}
		if !validHeaderName(name) {
			return fmt.Errorf("invalid response header")
		}
		for _, value := range values {
			if strings.IndexFunc(value, func(c rune) bool {
				return c != '\t' && (c < ' ' || c > '~')
			}) >= 0 {
				return fmt.Errorf("invalid response header")
			}
		}
	}
	noBody := input.Method == "HEAD" || status == 204 || status == 304
	// Browsers must not sniff handler bytes into HTML; a handler may set its own
	// X-Content-Type-Options.
	nosniff := true
	for key := range response.Headers {
		if strings.EqualFold(key, "X-Content-Type-Options") {
			nosniff = false
		}
	}
	if nosniff {
		output.Header().Set("X-Content-Type-Options", "nosniff")
	}
	for key, values := range response.Headers {
		for _, value := range values {
			output.Header().Add(key, value)
		}
	}
	// net/http would sniff a missing Content-Type (possibly to text/html); a nil
	// entry suppresses that.
	if len(output.Header().Values("Content-Type")) == 0 {
		output.Header()["Content-Type"] = nil
	}
	output.WriteHeader(status)
	*started = true
	if noBody || response.Body == nil {
		return nil
	}
	controller := http.NewResponseController(output)
	buffer := make([]byte, 32*1024)
	for {
		if err := input.Context().Err(); err != nil {
			return err
		}
		n, readErr := response.Body.Read(buffer)
		if n > 0 {
			if err := controller.SetWriteDeadline(time.Now().Add(30 * time.Second)); err != nil {
				return err
			}
			if _, err := output.Write(buffer[:n]); err != nil {
				return err
			}
			if err := controller.Flush(); err != nil {
				return err
			}
		}
		if readErr != nil {
			if errors.Is(readErr, io.EOF) {
				return nil
			}
			return readErr
		}
	}
}

func validHeaderName(value string) bool {
	if value == "" {
		return false
	}
	for _, c := range value {
		if !(c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || strings.ContainsRune("!#$%&'*+-.^_`|~", c)) {
			return false
		}
	}
	return true
}

func isEvent(request Request) bool {
	// Check every Content-Type field, so a duplicated one still reaches the
	// event parser's duplicate-header rejection whatever the header order.
	for _, value := range request.Headers.Values("Content-Type") {
		if strings.EqualFold(strings.TrimSpace(strings.Split(value, ";")[0]), "application/cloudevents+json") {
			return true
		}
	}
	for key := range request.Headers {
		if strings.HasPrefix(strings.ToLower(key), "ce-") {
			return true
		}
	}
	return false
}

func eventOf(request Request) (Event, error) {
	invalid := &HandlerError{400, "invalid CloudEvent"}
	headers := request.Headers
	for key, values := range headers {
		name := strings.ToLower(key)
		if (strings.HasPrefix(name, "ce-") || name == "content-type" || name == "x-lampyr-delivery-attempt") && len(values) != 1 {
			return Event{}, invalid
		}
	}
	body, err := request.ReadAll()
	if err != nil {
		return Event{}, err
	}
	if !utf8.Valid(body) {
		return Event{}, invalid
	}
	rawAttempt := headers.Get("X-Lampyr-Delivery-Attempt")
	if _, exists := headers[http.CanonicalHeaderKey("X-Lampyr-Delivery-Attempt")]; !exists {
		rawAttempt = "1"
	}
	for _, c := range rawAttempt {
		if c < '0' || c > '9' {
			return Event{}, &HandlerError{400, "invalid delivery attempt"}
		}
	}
	attempt, err := strconv.Atoi(rawAttempt)
	if err != nil || attempt < 1 || uint64(attempt) > 9007199254740991 {
		return Event{}, &HandlerError{400, "invalid delivery attempt"}
	}
	event := Event{Attempt: attempt}
	if strings.EqualFold(strings.TrimSpace(strings.Split(headers.Get("Content-Type"), ";")[0]), "application/cloudevents+json") {
		var envelope map[string]json.RawMessage
		if err := json.Unmarshal(body, &envelope); err != nil || envelope == nil {
			return Event{}, invalid
		}
		if _, exists := envelope["data_base64"]; exists {
			return Event{}, invalid
		}
		var version string
		if err := json.Unmarshal(envelope["specversion"], &version); err != nil || version != "1.0" {
			return Event{}, invalid
		}
		for name, target := range map[string]*string{"id": &event.ID, "type": &event.Type, "source": &event.Source, "subject": &event.Subject, "time": &event.Time} {
			value, exists := envelope[name]
			if !exists && (name == "subject" || name == "time") {
				continue
			}
			if string(value) == "null" {
				return Event{}, invalid
			}
			if err := json.Unmarshal(value, target); err != nil {
				return Event{}, invalid
			}
		}
		if data, exists := envelope["data"]; exists {
			if err := json.Unmarshal(data, &event.Data); err != nil {
				return Event{}, invalid
			}
		}
	} else {
		if headers.Get("Ce-Specversion") != "1.0" {
			return Event{}, invalid
		}
		event.ID, event.Type, event.Source = headers.Get("Ce-Id"), headers.Get("Ce-Type"), headers.Get("Ce-Source")
		event.Subject, event.Time = headers.Get("Ce-Subject"), headers.Get("Ce-Time")
		if err := json.Unmarshal(body, &event.Data); err != nil {
			event.Data = string(body)
		}
	}
	if event.ID == "" || event.Type == "" || event.Source == "" {
		return Event{}, invalid
	}
	return event, nil
}
