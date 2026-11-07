// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package api

import (
	"bytes"
	"encoding/json"
	"fmt"
	"net/url"
	"strconv"
	"time"
)

func ValidateCloudEvent(data []byte) error {
	var event map[string]json.RawMessage
	if err := json.Unmarshal(data, &event); err != nil {
		return fmt.Errorf("decode CloudEvent: %w", err)
	}
	required := make(map[string]string, 4)
	for _, attribute := range []string{"specversion", "id", "source", "type"} {
		value, err := cloudEventString(event, attribute, true)
		if err != nil {
			return err
		}
		required[attribute] = value
	}
	if required["specversion"] != "1.0" {
		return fmt.Errorf("CloudEvent specversion must be 1.0")
	}
	if _, err := url.Parse(required["source"]); err != nil {
		return fmt.Errorf("CloudEvent source must be a URI-reference: %w", err)
	}
	for _, attribute := range []string{
		"subject",
		"datacontenttype",
		"dataschema",
	} {
		if _, err := cloudEventString(event, attribute, false); err != nil {
			return err
		}
	}
	if value, found := event["time"]; found {
		timestamp, err := rawString(value)
		if err != nil {
			return fmt.Errorf("CloudEvent attribute %q must be a string", "time")
		}
		if _, err := time.Parse(time.RFC3339Nano, timestamp); err != nil {
			return fmt.Errorf("CloudEvent time must be RFC3339: %w", err)
		}
	}
	if value, found := event["data_base64"]; found {
		if _, err := rawString(value); err != nil {
			return fmt.Errorf("CloudEvent attribute %q must be a string", "data_base64")
		}
	}
	standard := map[string]struct{}{
		"specversion": {}, "id": {}, "source": {}, "type": {},
		"subject": {}, "time": {}, "datacontenttype": {},
		"dataschema": {}, "data": {}, "data_base64": {},
	}
	for name, raw := range event {
		if _, found := standard[name]; found {
			continue
		}
		if err := ValidateExtensionName(name); err != nil {
			return err
		}
		if err := validateCloudEventExtensionValue(name, raw); err != nil {
			return err
		}
	}
	return nil
}

func cloudEventString(
	event map[string]json.RawMessage,
	attribute string,
	required bool,
) (string, error) {
	raw, found := event[attribute]
	if !found {
		if required {
			return "", fmt.Errorf("CloudEvent attribute %q is required", attribute)
		}
		return "", nil
	}
	value, err := rawString(raw)
	if err != nil {
		return "", fmt.Errorf("CloudEvent attribute %q must be a string", attribute)
	}
	if required && value == "" {
		return "", fmt.Errorf("CloudEvent attribute %q is required", attribute)
	}
	return value, nil
}

func rawString(raw json.RawMessage) (string, error) {
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		return "", err
	}
	return value, nil
}

func validateCloudEventExtensionValue(
	name string,
	raw json.RawMessage,
) error {
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return fmt.Errorf("decode CloudEvent extension %q: %w", name, err)
	}

	switch value := value.(type) {
	case string, bool:
		return nil
	case json.Number:
		if _, err := strconv.ParseInt(value.String(), 10, 64); err != nil {
			return fmt.Errorf(
				"CloudEvent extension %q number must be an integer",
				name,
			)
		}
		return nil
	default:
		return fmt.Errorf(
			"CloudEvent extension %q must be a string, boolean, or integer",
			name,
		)
	}
}

func ValidateExtensionValue(name string, value any) error {
	encoded, err := json.Marshal(value)
	if err != nil {
		return fmt.Errorf("encode CloudEvent extension %q: %w", name, err)
	}
	return validateCloudEventExtensionValue(name, encoded)
}

func ApplyCloudEventOverrides(data []byte, overrides map[string]any) ([]byte, error) {
	if len(overrides) == 0 {
		if err := ValidateCloudEvent(data); err != nil {
			return nil, err
		}
		return data, nil
	}
	var event map[string]json.RawMessage
	if err := json.Unmarshal(data, &event); err != nil {
		return nil, fmt.Errorf("decode CloudEvent: %w", err)
	}
	if err := ValidateCloudEvent(data); err != nil {
		return nil, err
	}
	for name, value := range overrides {
		if err := ValidateExtensionName(name); err != nil {
			return nil, err
		}
		if err := ValidateExtensionValue(name, value); err != nil {
			return nil, err
		}
		encoded, err := json.Marshal(value)
		if err != nil {
			return nil, fmt.Errorf("encode extension %q: %w", name, err)
		}
		event[name] = encoded
	}
	encoded, err := json.Marshal(event)
	if err != nil {
		return nil, fmt.Errorf("encode CloudEvent: %w", err)
	}
	return encoded, nil
}

func CloudEventIdentity(data []byte) (string, string, error) {
	var event struct {
		Source string `json:"source"`
		ID     string `json:"id"`
	}
	if err := json.Unmarshal(data, &event); err != nil {
		return "", "", fmt.Errorf("decode CloudEvent identity: %w", err)
	}
	if event.Source == "" || event.ID == "" {
		return "", "", fmt.Errorf("CloudEvent source and id are required")
	}
	return event.Source, event.ID, nil
}
