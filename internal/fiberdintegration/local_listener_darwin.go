// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//go:build darwin

package fiberdintegration

import (
	"context"
	"errors"
	"os/exec"
	"strconv"
	"strings"
)

func processOwnsLocalListener(ctx context.Context, pid int, port string) (bool, error) {
	output, err := exec.CommandContext(
		ctx,
		"/usr/sbin/lsof", "-nP", "-a",
		"-p", strconv.Itoa(pid),
		"-iTCP:"+port, "-sTCP:LISTEN", "-Fn",
	).Output()
	if err != nil {
		var exit *exec.ExitError
		if errors.As(err, &exit) && exit.ExitCode() == 1 {
			return false, nil
		}
		return false, err
	}
	return strings.Contains(string(output), ":"+port), nil
}
