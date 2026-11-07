// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//go:build linux

package fiberdintegration

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

func processOwnsLocalListener(_ context.Context, pid int, port string) (bool, error) {
	inodes := make(map[string]bool)
	files, err := os.ReadDir(filepath.Join("/proc", strconv.Itoa(pid), "fd"))
	if err != nil {
		return false, err
	}
	for _, file := range files {
		target, err := os.Readlink(filepath.Join("/proc", strconv.Itoa(pid), "fd", file.Name()))
		if err != nil {
			if os.IsNotExist(err) {
				continue
			}
			return false, err
		}
		if strings.HasPrefix(target, "socket:[") {
			inodes[target] = true
		}
	}
	number, err := strconv.Atoi(port)
	if err != nil {
		return false, err
	}
	suffix := fmt.Sprintf(":%04X", number)
	for _, table := range []string{"tcp", "tcp6"} {
		file, err := os.Open("/proc/net/" + table)
		if err != nil {
			if table == "tcp6" && os.IsNotExist(err) {
				continue
			}
			return false, err
		}
		scanner := bufio.NewScanner(file)
		for scanner.Scan() {
			fields := strings.Fields(scanner.Text())
			if len(fields) > 9 && fields[3] == "0A" &&
				strings.HasSuffix(fields[1], suffix) && inodes["socket:["+fields[9]+"]"] {
				_ = file.Close()
				return true, nil
			}
		}
		err = scanner.Err()
		_ = file.Close()
		if err != nil {
			return false, err
		}
	}
	return false, nil
}
