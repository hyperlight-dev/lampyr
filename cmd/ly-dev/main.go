// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/hyperlight-dev/lampyr/internal/fiberdintegration"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "ly dev:", err)
		os.Exit(1)
	}
}

func run() error {
	if len(os.Args) == 2 && os.Args[1] == "--protocol-version" {
		fmt.Println("3")
		return nil
	}
	if len(os.Args) != 2 {
		return fmt.Errorf("usage: ly-dev PLAN.json (internal ly helper)")
	}
	file, err := os.Open(os.Args[1])
	if err != nil {
		return err
	}
	defer file.Close()
	var config fiberdintegration.LocalConfig
	decoder := json.NewDecoder(file)
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&config); err != nil {
		return err
	}
	ctx, cancel := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancel()
	return fiberdintegration.RunLocal(ctx, config)
}
