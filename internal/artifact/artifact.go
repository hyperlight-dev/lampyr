// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package artifact

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"

	"github.com/hyperlight-dev/lampyr/internal/project"
)

const (
	SchemaVersion = "lampyr.dev/artifact/v1alpha2"
	BuildType     = "https://lampyr.dev/build/snapshot/v1"
)

type Metadata struct {
	SchemaVersion string            `json:"schemaVersion"`
	RevisionID    string            `json:"revisionId"`
	Project       project.Manifest  `json:"project"`
	Target        Target            `json:"target"`
	Source        Source            `json:"source"`
	Runtime       map[string]string `json:"runtime"`
	Artifacts     map[string]string `json:"artifacts"`
	Provenance    Provenance        `json:"provenance"`
}

type Target struct {
	OS                 string `json:"os"`
	Arch               string `json:"arch"`
	Hypervisor         string `json:"hypervisor"`
	CompatibilityClass string `json:"compatibilityClass"`
}

type Source struct {
	Digest string `json:"digest"`
}

type Provenance struct {
	BuildType string `json:"buildType"`
	Builder   string `json:"builder"`
}

type Result struct {
	Directory  string
	Metadata   Metadata
	MetadataID string
	Image      string
}

func SnapshotCPUVendor(directory string) (string, error) {
	data, err := os.ReadFile(filepath.Join(directory, "index.json"))
	if err != nil {
		return "", fmt.Errorf("read snapshot index: %w", err)
	}
	var index struct {
		Manifests []struct {
			Annotations map[string]string `json:"annotations"`
		} `json:"manifests"`
	}
	if err := json.Unmarshal(data, &index); err != nil {
		return "", fmt.Errorf("decode snapshot index: %w", err)
	}
	for _, manifest := range index.Manifests {
		if vendor := strings.TrimSpace(
			manifest.Annotations["dev.hyperlight.snapshot.cpu.vendor"],
		); vendor != "" {
			return vendor, nil
		}
	}
	return "", fmt.Errorf("snapshot CPU vendor annotation is missing")
}

func CreateMetadata(
	manifest project.Manifest,
	target Target,
	sourceDigest string,
	runtime map[string]string,
	artifacts map[string]string,
) (Metadata, []byte, error) {
	revisionID, err := RevisionID(manifest, target, sourceDigest, runtime, artifacts)
	if err != nil {
		return Metadata{}, nil, err
	}

	metadata := Metadata{
		SchemaVersion: SchemaVersion,
		RevisionID:    revisionID,
		Project:       manifest,
		Target:        target,
		Source:        Source{Digest: sourceDigest},
		Runtime:       runtime,
		Artifacts:     artifacts,
		Provenance: Provenance{
			BuildType: BuildType,
			Builder:   "ly",
		},
	}
	data, err := json.MarshalIndent(metadata, "", "  ")
	if err != nil {
		return Metadata{}, nil, err
	}
	return metadata, append(data, '\n'), nil
}

func RevisionID(
	manifest project.Manifest,
	target Target,
	sourceDigest string,
	runtime map[string]string,
	artifacts map[string]string,
) (string, error) {
	revisionInput, err := json.Marshal(struct {
		Project   project.Manifest  `json:"project"`
		Target    Target            `json:"target"`
		Source    string            `json:"source"`
		Runtime   map[string]string `json:"runtime"`
		Artifacts map[string]string `json:"artifacts"`
	}{
		Project: manifest,
		Target:  target,
		Source:  sourceDigest,
		Runtime: runtime,
		Artifacts: map[string]string{
			"kernel": artifacts["kernel"],
			"rootfs": artifacts["rootfs"],
		},
	})
	if err != nil {
		return "", err
	}
	revisionHash := sha256.Sum256(revisionInput)
	return hex.EncodeToString(revisionHash[:]), nil
}

func VerifyDirectory(directory string) (Metadata, error) {
	metadataJSON, err := os.ReadFile(filepath.Join(directory, "metadata.json"))
	if err != nil {
		return Metadata{}, err
	}

	var metadata Metadata
	if err := json.Unmarshal(metadataJSON, &metadata); err != nil {
		return Metadata{}, fmt.Errorf("decode metadata: %w", err)
	}
	if metadata.SchemaVersion != SchemaVersion {
		return Metadata{}, fmt.Errorf("unsupported artifact schema %q", metadata.SchemaVersion)
	}

	snapshotDigest, err := DirectoryDigest(filepath.Join(directory, "snapshot"))
	if err != nil {
		return Metadata{}, err
	}
	if snapshotDigest != metadata.Artifacts["snapshot"] {
		return Metadata{}, fmt.Errorf("snapshot digest does not match metadata")
	}
	kernelDigest, err := FileDigest(filepath.Join(directory, "kernel"))
	if err != nil {
		return Metadata{}, err
	}
	if kernelDigest != metadata.Artifacts["kernel"] {
		return Metadata{}, fmt.Errorf("kernel digest does not match metadata")
	}
	revisionID, err := RevisionID(
		metadata.Project,
		metadata.Target,
		metadata.Source.Digest,
		metadata.Runtime,
		metadata.Artifacts,
	)
	if err != nil {
		return Metadata{}, err
	}
	if revisionID != metadata.RevisionID {
		return Metadata{}, fmt.Errorf("revision ID does not match metadata")
	}
	return metadata, nil
}

func FileDigest(path string) (string, error) {
	file, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer file.Close()

	hash := sha256.New()
	if _, err := io.Copy(hash, file); err != nil {
		return "", err
	}
	return "sha256:" + hex.EncodeToString(hash.Sum(nil)), nil
}

func DirectoryDigest(root string, ignored ...string) (string, error) {
	ignore := make(map[string]struct{}, len(ignored))
	for _, name := range ignored {
		ignore[filepath.Clean(name)] = struct{}{}
	}

	type digestEntry struct {
		path string
		mode os.FileMode
		data []byte
	}
	var entries []digestEntry
	err := filepath.WalkDir(root, func(path string, entry os.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}
		relative, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		if relative == "." {
			return nil
		}
		for name := range ignore {
			if relative == name || strings.HasPrefix(relative, name+string(filepath.Separator)) {
				if entry.IsDir() {
					return filepath.SkipDir
				}
				return nil
			}
		}
		info, err := entry.Info()
		if err != nil {
			return err
		}
		switch {
		case entry.Type().IsRegular():
			data, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			entries = append(entries, digestEntry{path: relative, mode: info.Mode(), data: data})
		case entry.Type()&os.ModeSymlink != 0:
			target, err := os.Readlink(path)
			if err != nil {
				return err
			}
			entries = append(entries, digestEntry{
				path: relative,
				mode: info.Mode(),
				data: []byte(target),
			})
		}
		return nil
	})
	if err != nil {
		return "", err
	}
	sort.Slice(entries, func(i, j int) bool {
		return entries[i].path < entries[j].path
	})

	hash := sha256.New()
	for _, entry := range entries {
		fmt.Fprintf(
			hash,
			"%s\x00%o\x00%d\x00",
			filepath.ToSlash(entry.path),
			entry.mode,
			len(entry.data),
		)
		hash.Write(entry.data)
	}
	return "sha256:" + hex.EncodeToString(hash.Sum(nil)), nil
}

func GitRevision(ctx context.Context, directory string) string {
	command := exec.CommandContext(ctx, "git", "-C", directory, "rev-parse", "HEAD")
	data, err := command.Output()
	if err != nil {
		return "unknown"
	}
	return strings.TrimSpace(string(data))
}
