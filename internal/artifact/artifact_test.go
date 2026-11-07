// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package artifact

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/hyperlight-dev/lampyr/internal/project"
)

func TestMetadataRevisionIsReproducible(t *testing.T) {
	manifest := project.DefaultNode("hello")
	target := Target{
		OS:                 "linux",
		Arch:               "amd64",
		Hypervisor:         "kvm",
		CompatibilityClass: "linux-amd64-kvm",
	}
	runtime := map[string]string{"hyperlightUnikraft": "abc", "unikraft": "def"}
	artifacts := map[string]string{"snapshot": "sha256:123"}

	first, firstJSON, err := CreateMetadata(manifest, target, "sha256:source", runtime, artifacts)
	if err != nil {
		t.Fatal(err)
	}
	second, secondJSON, err := CreateMetadata(manifest, target, "sha256:source", runtime, artifacts)
	if err != nil {
		t.Fatal(err)
	}
	if first.RevisionID != second.RevisionID {
		t.Fatalf("revision IDs differ: %s != %s", first.RevisionID, second.RevisionID)
	}
	if string(firstJSON) != string(secondJSON) {
		t.Fatal("metadata JSON is not reproducible")
	}
}

func TestApplicationModelParticipatesInRevisionIdentity(t *testing.T) {
	for _, application := range []*project.Application{
		{Type: "handler"},
		{Type: "standalone", Command: "/app/service"},
	} {
		t.Run(application.Type, func(t *testing.T) {
			manifest := project.DefaultNode("hello")
			manifest.Application = application
			target := Target{OS: "linux", Arch: "amd64", Hypervisor: "kvm", CompatibilityClass: "linux-amd64-kvm"}
			metadata, encoded, err := CreateMetadata(manifest, target, "sha256:source", nil, nil)
			if err != nil {
				t.Fatal(err)
			}
			var decoded Metadata
			if err := json.Unmarshal(encoded, &decoded); err != nil {
				t.Fatal(err)
			}
			if decoded.Project.Application == nil || *decoded.Project.Application != *application {
				t.Fatal("application model/command were lost in artifact metadata")
			}
			revision, err := RevisionID(decoded.Project, target, "sha256:source", nil, nil)
			if err != nil || revision != metadata.RevisionID {
				t.Fatalf("roundtrip changed revision identity: %s (%v)", revision, err)
			}
			decoded.Project.Application = nil
			legacy, err := RevisionID(decoded.Project, target, "sha256:source", nil, nil)
			if err != nil || legacy == revision {
				t.Fatal("application model must participate in revision identity")
			}
		})
	}
}

func TestStandaloneRevisionMatchesRustFixture(t *testing.T) {
	data, err := os.ReadFile("../../tests/artifacts/standalone.json")
	if err != nil {
		t.Fatal(err)
	}
	var metadata Metadata
	if err := json.Unmarshal(data, &metadata); err != nil {
		t.Fatal(err)
	}
	revision, err := RevisionID(metadata.Project, metadata.Target, metadata.Source.Digest, metadata.Runtime, metadata.Artifacts)
	if err != nil || revision != metadata.RevisionID {
		t.Fatalf("standalone revision %s does not match shared fixture %s: %v", revision, metadata.RevisionID, err)
	}
}

func TestVerifyDirectory(t *testing.T) {
	directory := t.TempDir()
	snapshot := filepath.Join(directory, "snapshot")
	if err := os.Mkdir(snapshot, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(snapshot, "state"), []byte("snapshot"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, "kernel"), []byte("kernel"), 0o755); err != nil {
		t.Fatal(err)
	}
	snapshotDigest, err := DirectoryDigest(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	kernelDigest, err := FileDigest(filepath.Join(directory, "kernel"))
	if err != nil {
		t.Fatal(err)
	}
	_, metadata, err := CreateMetadata(
		project.DefaultNode("hello"),
		Target{
			OS:                 "linux",
			Arch:               "amd64",
			Hypervisor:         "kvm",
			CompatibilityClass: "linux-amd64-kvm",
		},
		"sha256:source",
		map[string]string{},
		map[string]string{"snapshot": snapshotDigest, "kernel": kernelDigest},
	)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, "metadata.json"), metadata, 0o644); err != nil {
		t.Fatal(err)
	}

	if _, err := VerifyDirectory(directory); err != nil {
		t.Fatal(err)
	}
	var altered Metadata
	if err := json.Unmarshal(metadata, &altered); err != nil {
		t.Fatal(err)
	}
	altered.Project.Name = "other"
	alteredJSON, err := json.Marshal(altered)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, "metadata.json"), alteredJSON, 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := VerifyDirectory(directory); err == nil {
		t.Fatal("metadata with a mismatched revision ID verified")
	}
	if err := os.WriteFile(filepath.Join(directory, "metadata.json"), metadata, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(snapshot, "state"), []byte("modified"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := VerifyDirectory(directory); err == nil {
		t.Fatal("modified snapshot verified")
	}
}

func TestSnapshotCPUVendor(t *testing.T) {
	directory := t.TempDir()
	index := []byte(`{
  "schemaVersion": 2,
  "manifests": [{
    "annotations": {
      "dev.hyperlight.snapshot.cpu.vendor": "GenuineIntel"
    }
  }]
}`)
	if err := os.WriteFile(
		filepath.Join(directory, "index.json"),
		index,
		0o644,
	); err != nil {
		t.Fatal(err)
	}
	vendor, err := SnapshotCPUVendor(directory)
	if err != nil {
		t.Fatal(err)
	}
	if vendor != "GenuineIntel" {
		t.Fatalf("vendor = %q, want GenuineIntel", vendor)
	}
}

func TestDirectoryDigestIncludesModesAndSymlinks(t *testing.T) {
	directory := t.TempDir()
	file := filepath.Join(directory, "file")
	if err := os.WriteFile(file, []byte("content"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("file", filepath.Join(directory, "link")); err != nil {
		t.Fatal(err)
	}
	first, err := DirectoryDigest(directory)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(file, 0o755); err != nil {
		t.Fatal(err)
	}
	second, err := DirectoryDigest(directory)
	if err != nil {
		t.Fatal(err)
	}
	if first == second {
		t.Fatal("mode change did not change directory digest")
	}
	if err := os.Remove(filepath.Join(directory, "link")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("other", filepath.Join(directory, "link")); err != nil {
		t.Fatal(err)
	}
	third, err := DirectoryDigest(directory)
	if err != nil {
		t.Fatal(err)
	}
	if second == third {
		t.Fatal("symlink target change did not change directory digest")
	}
}
