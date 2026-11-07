// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package adapter

import (
	"context"
	"fmt"
	"time"

	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/api"
	"github.com/hyperlight-dev/lampyr/providers/azure/eventgrid/internal/config"
)

type Sink interface {
	Send(context.Context, string, []byte) error
}

type Processor struct {
	sink     Sink
	inFlight chan struct{}
	metrics  *Metrics
}

type InvalidEventError struct {
	Err error
}

func (err *InvalidEventError) Error() string {
	return err.Err.Error()
}

func (err *InvalidEventError) Unwrap() error {
	return err.Err
}

func NewProcessor(sink Sink) *Processor {
	return &Processor{
		sink:     sink,
		inFlight: make(chan struct{}, 64),
		metrics:  NewMetrics(),
	}
}

func (processor *Processor) Metrics() *Metrics {
	return processor.metrics
}

func (processor *Processor) SetMaxInFlight(maximum int) {
	if maximum < 1 {
		maximum = 1
	}
	processor.inFlight = make(chan struct{}, maximum)
}

func (processor *Processor) ProcessWebhook(
	ctx context.Context,
	adapterConfig config.AdapterConfig,
	event []byte,
) error {
	if err := config.Validate(adapterConfig); err != nil {
		return err
	}
	started := time.Now()
	defer func() {
		processor.metrics.deliveryLatencyMillis.Add(
			uint64(time.Since(started).Milliseconds()),
		)
		processor.metrics.deliveryLatencyCount.Add(1)
	}()
	select {
	case processor.inFlight <- struct{}{}:
		processor.metrics.activeDeliveries.Add(1)
		defer func() {
			processor.metrics.activeDeliveries.Add(-1)
			<-processor.inFlight
		}()
	case <-ctx.Done():
		return fmt.Errorf("wait for delivery capacity: %w", ctx.Err())
	}
	prepared, err := api.ApplyCloudEventOverrides(event, adapterConfig.CEOverrides)
	if err != nil {
		processor.metrics.receiveFailure.Add(1)
		return &InvalidEventError{Err: fmt.Errorf("prepare CloudEvent: %w", err)}
	}
	eventSource, eventID, err := api.CloudEventIdentity(prepared)
	if err != nil {
		processor.metrics.receiveFailure.Add(1)
		return &InvalidEventError{Err: fmt.Errorf("read CloudEvent identity: %w", err)}
	}
	processor.metrics.receiveSuccess.Add(1)
	deliveryCtx, cancel := context.WithTimeout(ctx, 25*time.Second)
	defer cancel()
	if err := processor.sink.Send(deliveryCtx, adapterConfig.SinkURI, prepared); err != nil {
		processor.metrics.sinkFailure.Add(1)
		return fmt.Errorf(
			"deliver CloudEvent source=%q id=%q: %w",
			eventSource,
			eventID,
			err,
		)
	}
	processor.metrics.sinkSuccess.Add(1)
	return nil
}
