// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package project

import (
	"fmt"
	"os"
	"time"

	"gopkg.in/yaml.v3"
)

const (
	APIVersion    = "lampyr.dev/v1alpha1"
	Kind          = "Project"
	RuntimeNode   = "nodejs"
	RuntimePython = "python"
	RuntimeGo     = "go"
	RuntimeRust   = "rust"
	RuntimeDotnet = "dotnet"
)

type Manifest struct {
	APIVersion  string            `json:"apiVersion" yaml:"apiVersion"`
	Kind        string            `json:"kind" yaml:"kind"`
	Name        string            `json:"name" yaml:"name"`
	Runtime     string            `json:"runtime" yaml:"runtime"`
	Application *Application      `json:"application,omitempty" yaml:"application,omitempty"`
	Command     string            `json:"command" yaml:"command"`
	Protocols   Protocols         `json:"protocols" yaml:"protocols"`
	Events      Events            `json:"events" yaml:"events"`
	Resources   Resources         `json:"resources" yaml:"resources"`
	Execution   *ExecutionPolicy  `json:"execution,omitempty" yaml:"execution,omitempty"`
	Bindings    map[string]string `json:"bindings,omitempty" yaml:"bindings,omitempty"`
}

// Application is retained in artifact identity alongside the effective command.
type Application struct {
	Type    string `json:"type" yaml:"type"`
	Command string `json:"command,omitempty" yaml:"command,omitempty"`
}

const (
	ExecutionFresh = "fresh-per-request"
	ExecutionReuse = "reuse"
)

type ExecutionPolicy struct {
	Mode        string `json:"mode" yaml:"mode"`
	IdleTimeout string `json:"idleTimeout,omitempty" yaml:"idleTimeout,omitempty"`
	IdleAction  string `json:"idleAction,omitempty" yaml:"idleAction,omitempty"`
}

func (m Manifest) EffectiveExecution() (ExecutionPolicy, error) {
	policy := ExecutionPolicy{Mode: ExecutionFresh}
	if m.Execution != nil {
		policy = *m.Execution
		if policy.Mode == "" {
			policy.Mode = ExecutionFresh
		}
	}
	switch policy.Mode {
	case ExecutionFresh:
		if policy.IdleTimeout != "" || policy.IdleAction != "" {
			return ExecutionPolicy{}, fmt.Errorf("execution idle settings are only valid with mode reuse")
		}
	case ExecutionReuse:
		if policy.IdleTimeout == "" {
			policy.IdleTimeout = "30s"
		}
		if policy.IdleAction == "" {
			policy.IdleAction = "release"
		}
		if policy.IdleAction != "release" {
			return ExecutionPolicy{}, fmt.Errorf("execution.idleAction must be release; park/resume is not supported")
		}
		idle, err := time.ParseDuration(policy.IdleTimeout)
		if err != nil || idle <= 0 {
			return ExecutionPolicy{}, fmt.Errorf("execution.idleTimeout must be a positive duration")
		}
	default:
		return ExecutionPolicy{}, fmt.Errorf("execution.mode must be %q or %q; park/resume is not supported", ExecutionFresh, ExecutionReuse)
	}
	return policy, nil
}

type Protocols struct {
	HTTP      bool `json:"http" yaml:"http"`
	Streaming bool `json:"streaming" yaml:"streaming"`
	WebSocket bool `json:"websocket" yaml:"websocket"`
	Events    bool `json:"events" yaml:"events"`
	// GRPC marks a service whose server speaks HTTP/2 without TLS (h2c), as
	// gRPC servers do: all its requests then reach it as h2c.
	GRPC bool `json:"grpc,omitempty" yaml:"grpc,omitempty"`
}

type Events struct {
	MaxRetries     int    `json:"maxRetries" yaml:"maxRetries"`
	InitialBackoff string `json:"initialBackoff" yaml:"initialBackoff"`
	AttemptTimeout string `json:"attemptTimeout" yaml:"attemptTimeout"`
	DeadLetter     string `json:"deadLetter" yaml:"deadLetter"`
}

type Resources struct {
	Memory string `json:"memory" yaml:"memory"`
	CPU    string `json:"cpu" yaml:"cpu"`
}

func DefaultNode(name string) Manifest {
	return Default(name, RuntimeNode)
}

func Default(name, runtime string) Manifest {
	switch runtime {
	case RuntimeNode:
		return Manifest{
			APIVersion: APIVersion,
			Kind:       Kind,
			Name:       name,
			Runtime:    RuntimeNode,
			Command:    "/entrypoint.js",
			Protocols: Protocols{
				HTTP:      true,
				Streaming: true,
				WebSocket: true,
				Events:    true,
			},
			Events: Events{
				MaxRetries:     3,
				InitialBackoff: "100ms",
				AttemptTimeout: "30s",
				DeadLetter:     ".lampyr/dead-letter.jsonl",
			},
			Resources: Resources{
				Memory: "512Mi",
				CPU:    "250m",
			},
		}
	case RuntimePython:
		return Manifest{
			APIVersion: APIVersion,
			Kind:       Kind,
			Name:       name,
			Runtime:    RuntimePython,
			Command:    "/entrypoint.py",
			Protocols: Protocols{
				HTTP:   true,
				Events: true,
			},
			Events: Events{
				MaxRetries:     3,
				InitialBackoff: "100ms",
				AttemptTimeout: "30s",
				DeadLetter:     ".lampyr/dead-letter.jsonl",
			},
			Resources: Resources{
				Memory: "256Mi",
				CPU:    "250m",
			},
		}
	case RuntimeGo:
		return Manifest{
			APIVersion: APIVersion,
			Kind:       Kind,
			Name:       name,
			Runtime:    RuntimeGo,
			Command:    "/entrypoint",
			Protocols: Protocols{
				HTTP:      true,
				Streaming: true,
				Events:    true,
			},
			Events: Events{
				MaxRetries:     3,
				InitialBackoff: "100ms",
				AttemptTimeout: "30s",
				DeadLetter:     ".lampyr/dead-letter.jsonl",
			},
			Resources: Resources{
				Memory: "128Mi",
				CPU:    "250m",
			},
		}
	case RuntimeRust:
		return Manifest{
			APIVersion: APIVersion,
			Kind:       Kind,
			Name:       name,
			Runtime:    RuntimeRust,
			Command:    "/entrypoint",
			Protocols: Protocols{
				HTTP:      true,
				Streaming: true,
			},
			Resources: Resources{
				Memory: "64Mi",
				CPU:    "250m",
			},
		}
	case RuntimeDotnet:
		return Manifest{
			APIVersion: APIVersion,
			Kind:       Kind,
			Name:       name,
			Runtime:    RuntimeDotnet,
			Command:    "/app/LampyrService",
			Protocols: Protocols{
				HTTP:      true,
				Streaming: true,
				Events:    true,
			},
			Events: Events{
				MaxRetries:     3,
				InitialBackoff: "100ms",
				AttemptTimeout: "30s",
				DeadLetter:     ".lampyr/dead-letter.jsonl",
			},
			Resources: Resources{
				Memory: "256Mi",
				CPU:    "250m",
			},
		}
	default:
		return Manifest{Runtime: runtime}
	}
}

func Load(path string) (Manifest, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return Manifest{}, err
	}

	var manifest Manifest
	if err := yaml.Unmarshal(data, &manifest); err != nil {
		return Manifest{}, fmt.Errorf("decode project manifest: %w", err)
	}
	if err := manifest.Validate(); err != nil {
		return Manifest{}, err
	}
	return manifest, nil
}

func Write(path string, manifest Manifest) error {
	if err := manifest.Validate(); err != nil {
		return err
	}
	data, err := yaml.Marshal(manifest)
	if err != nil {
		return fmt.Errorf("encode project manifest: %w", err)
	}
	return os.WriteFile(path, data, 0o644)
}

func (m Manifest) Validate() error {
	switch {
	case m.APIVersion != APIVersion:
		return fmt.Errorf("apiVersion must be %q", APIVersion)
	case m.Kind != Kind:
		return fmt.Errorf("kind must be %q", Kind)
	case m.Name == "":
		return fmt.Errorf("name is required")
	case m.Runtime != RuntimeNode &&
		m.Runtime != RuntimePython &&
		m.Runtime != RuntimeGo &&
		m.Runtime != RuntimeRust &&
		m.Runtime != RuntimeDotnet:
		return fmt.Errorf("runtime %q is not supported by this preview", m.Runtime)
	case m.Command == "":
		return fmt.Errorf("command is required")
	case m.Resources.Memory == "":
		return fmt.Errorf("resources.memory is required")
	case m.Resources.CPU == "":
		return fmt.Errorf("resources.cpu is required")
	case m.Protocols.GRPC && m.Protocols.WebSocket:
		return fmt.Errorf("protocols.grpc cannot be combined with protocols.websocket: a gRPC service receives every request as h2c")
	case m.Protocols.Events && m.Events.MaxRetries < 0:
		return fmt.Errorf("events.maxRetries cannot be negative")
	case m.Protocols.Events && m.Events.MaxRetries > 10:
		return fmt.Errorf("events.maxRetries cannot exceed 10")
	case m.Protocols.Events && m.Events.InitialBackoff == "":
		return fmt.Errorf("events.initialBackoff is required")
	case m.Protocols.Events && m.Events.DeadLetter == "":
		return fmt.Errorf("events.deadLetter is required")
	default:
		if _, err := BindingArgs(m.Bindings, "default", nil); err != nil {
			return err
		}
		_, err := m.EffectiveExecution()
		return err
	}
}
