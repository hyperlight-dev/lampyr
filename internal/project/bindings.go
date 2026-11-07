// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

package project

import (
	"fmt"
	"net"
	"net/url"
	"regexp"
	"sort"
	"strings"
)

var bindingName = regexp.MustCompile(`^[a-z][a-z0-9_]*$`)
var serviceName = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`)

// Binding names are fully qualified. Kubernetes' ndots:5 would first ask for
// search-suffixed names that were never declared in the network allowlist.
func BindingResolver(source string) (string, error) {
	var servers []string
	for _, line := range strings.Split(source, "\n") {
		fields := strings.Fields(line)
		if len(fields) == 0 || fields[0] != "nameserver" {
			continue
		}
		if len(fields) < 2 || net.ParseIP(fields[1]) == nil {
			return "", fmt.Errorf("invalid Pod DNS nameserver")
		}
		servers = append(servers, "nameserver "+fields[1])
	}
	if len(servers) == 0 {
		return "", fmt.Errorf("service bindings require Pod DNS nameservers")
	}
	return strings.Join(servers, "\n") + "\noptions ndots:1\n", nil
}

// BindingArgs is shared by local homes and deployed snapshot restores. Only
// declared destinations are added to the guest's outbound network policy.
func BindingArgs(bindings map[string]string, namespace string, local map[string]string) ([]string, error) {
	if !serviceName.MatchString(namespace) {
		return nil, fmt.Errorf("invalid binding namespace %q", namespace)
	}
	names := make([]string, 0, len(bindings))
	for name, service := range bindings {
		if !bindingName.MatchString(name) || !serviceName.MatchString(service) {
			return nil, fmt.Errorf("invalid service binding %q: %q", name, service)
		}
		names = append(names, name)
	}
	sort.Strings(names)
	var args []string
	for _, name := range names {
		service := bindings[name]
		address := "http://" + service + "." + namespace + ".svc.cluster.local"
		if local != nil {
			var ok bool
			address, ok = local[service]
			if !ok {
				return nil, fmt.Errorf("binding %s targets missing local service %s", name, service)
			}
		}
		target, err := url.Parse(address)
		if err != nil || target.Scheme != "http" || target.Hostname() == "" || target.User != nil {
			return nil, fmt.Errorf("invalid binding endpoint for %s", name)
		}
		args = append(args, "--env", "LAMPYR_BINDING_"+strings.ToUpper(name)+"="+address, "--net-allow", target.Hostname())
	}
	return args, nil
}
