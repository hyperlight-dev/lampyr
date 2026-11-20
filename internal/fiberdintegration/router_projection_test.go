// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//go:build !windows

package fiberdintegration

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"

	"github.com/hyperlight-dev/lampyr/internal/project"
)

func TestRevisionReloadRetriesProjectedVolumeGenerationChange(t *testing.T) {
	router, client, revision := routerFixture(t, project.ExecutionReuse)
	response := httptest.NewRecorder()
	router.ServeHTTP(response, revisionRequest("GET", "/test", revision))
	if response.Code != http.StatusOK {
		t.Fatalf("initial request = %d", response.Code)
	}
	root := t.TempDir()
	const (
		legacyGeneration  = "..2026_11_20_10_00_00"
		currentGeneration = "..2026_11_20_10_00_01"
		legacyName        = "revision-default--demo-1234.json"
		currentName       = "revision-default_demo-1234.json"
	)
	for generation, name := range map[string]string{
		legacyGeneration:  legacyName,
		currentGeneration: currentName,
	} {
		directory := filepath.Join(root, generation)
		if err := os.Mkdir(directory, 0o700); err != nil {
			t.Fatal(err)
		}
		writeReloadRoute(t, directory, name, revision)
	}
	if err := os.Symlink(legacyGeneration, filepath.Join(root, "..data")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(filepath.Join("..data", legacyName), filepath.Join(root, legacyName)); err != nil {
		t.Fatal(err)
	}
	router.routesDir = root
	switched := false
	readFile := func(path string) ([]byte, error) {
		if !switched {
			switched = true
			if err := os.Symlink(currentGeneration, filepath.Join(root, "..data_tmp")); err != nil {
				t.Fatal(err)
			}
			if err := os.Rename(filepath.Join(root, "..data_tmp"), filepath.Join(root, "..data")); err != nil {
				t.Fatal(err)
			}
			if err := os.Remove(filepath.Join(root, legacyName)); err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink(filepath.Join("..data", currentName), filepath.Join(root, currentName)); err != nil {
				t.Fatal(err)
			}
			if err := os.RemoveAll(filepath.Join(root, legacyGeneration)); err != nil {
				t.Fatal(err)
			}
		}
		return os.ReadFile(path)
	}
	if err := router.reload(readFile); err != nil {
		t.Fatal(err)
	}
	router.releases.Wait()
	checkReloadReadiness(t, router, revision, http.StatusOK)
	if router.pools["default/demo-1234"] == nil || len(client.released) != 0 {
		t.Fatalf("generation change drained the live pool: releases = %v, pools = %v", client.released, router.pools)
	}
}
