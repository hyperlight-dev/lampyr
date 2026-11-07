"""Declarative Event Grid manifests; no deployment or image-building operations."""

from pathlib import Path

import yaml


SYSTEM = "lampyr-system"
SECRET = "eventgrid-webhook-auth"
WEBHOOK = "eventgrid-adapter-webhook"
KEY_DIRECTORY = "/var/run/lampyr-eventgrid/auth"
KEY_PATH = "/var/run/lampyr-eventgrid/auth/key"
COMPONENTS = (
    ("eventgrid-controller", "controller", "eventGridControllerImage"),
    ("eventgrid-adapter", "adapter", "eventGridAdapterImage"),
)


class EventGrid:
    def __init__(self, templates, fail):
        self.templates = Path(templates)
        self.fail = fail
    def asset(self, path):
        try:
            documents = [
                item for item in yaml.safe_load_all((self.templates / Path(path).name).read_text(encoding="utf-8"))
                if item is not None
            ]
        except (yaml.YAMLError, KeyError, TypeError):
            self.fail("invalid-asset", f"Cannot parse platform asset {path}")
        return {"apiVersion": "v1", "kind": "List", "items": documents}
    def manifests(self, images, bindings=None):
        if not bindings:
            self.fail("invalid-config", "Azure Event Grid manifests require Azure bindings")
        if not isinstance(images, dict) or any(
            not isinstance(images.get(image_key), str) for _, _, image_key in COMPONENTS
        ):
            self.fail("invalid-image", "Event Grid manifests require named controller and adapter images")
        source = self.asset("eventgrid-source.yaml")
        found = set()
        for resource in source["items"]:
            name = resource.get("metadata", {}).get("name")
            if resource.get("kind") == "ServiceAccount" and name == "eventgrid-controller":
                resource["metadata"].setdefault("annotations", {})["azure.workload.identity/client-id"] = bindings["controllerClientId"]
                found.add("serviceaccount")
            if resource.get("kind") != "Deployment":
                continue
            for component, target, image_key in COMPONENTS:
                if name != component:
                    continue
                template = resource["spec"]["template"]
                spec = template["spec"]
                containers = [item for item in spec["containers"] if item["name"] == target]
                if len(containers) != 1 or name in found:
                    self.fail("invalid-asset", f"Missing unique Event Grid container {name}/{target}")
                found.add(name)
                container = containers[0]
                container.update(image=images[image_key], imagePullPolicy="IfNotPresent")
                if name == "eventgrid-controller":
                    template.setdefault("metadata", {}).setdefault("labels", {})["azure.workload.identity/use"] = "true"
                    container["args"] = [
                        "--health-address=:8080",
                        f"--azure-subscription-id={bindings['subscriptionId']}",
                        f"--azure-publisher-resource-group={bindings['publisherResourceGroup']}",
                        f"--azure-webhook-base-url=https://{bindings['webhookHost']}",
                    ]
                else:
                    spec["volumes"] = [{"name": "webhook-auth", "secret": {"secretName": SECRET}}]
                    container["volumeMounts"] = [
                        {"name": "webhook-auth", "mountPath": KEY_DIRECTORY, "readOnly": True},
                    ]
                    container["args"] = [
                        "--health-address=:8080", "--max-in-flight=64", "--webhook-address=:8081",
                        f"--webhook-auth-key-file={KEY_PATH}", f"--azure-front-door-id={bindings['frontDoorId']}",
                    ]
        expected = {name for name, _, _ in COMPONENTS}
        expected.add("serviceaccount")
        if found != expected:
            self.fail("invalid-asset", "Event Grid platform manifest is incomplete")
        source["items"].extend([
            {"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {
                "name": "fiberd-grant", "namespace": "default",
                "annotations": {"azure.workload.identity/client-id": bindings["fiberdClientId"]},
            }},
            {"apiVersion": "v1", "kind": "Service", "metadata": {
                "name": WEBHOOK, "namespace": SYSTEM, "annotations": {
                    "service.beta.kubernetes.io/azure-pip-name": bindings["publicIpName"],
                    "service.beta.kubernetes.io/azure-load-balancer-resource-group": bindings["resourceGroup"],
                    "service.beta.kubernetes.io/azure-allowed-service-tags": "AzureFrontDoor.Backend",
                    "service.beta.kubernetes.io/port_80_health-probe_protocol": "tcp",
                },
            }, "spec": {"type": "LoadBalancer", "selector": {"app.kubernetes.io/name": "eventgrid-adapter"},
                        "ports": [{"name": "webhook", "port": 80, "targetPort": "webhook", "protocol": "TCP"}]}},
        ])
        return source
