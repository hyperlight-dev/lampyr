import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import test_provider as fixtures

OWNER, PRINCIPAL, provider = fixtures.OWNER, fixtures.PRINCIPAL, fixtures.provider
NODE_SETUP_IMAGE = "registry/node-setup@sha256:" + "c" * 64


class ManifestTests(unittest.TestCase):
    def setUp(self):
        fixtures.ProviderTests.setUp(self)
        self.outputs = dict(provider.deployment_scope(self.parameters, self.request),
                            sourceDigest="f" * 64, eventing={})
        group = provider.resource_id(self.parameters, group="dedicated")
        self.inventory = {
            "resourceGroup": "dedicated", "resourceGroupId": group,
            "publicIpId": group + "/providers/Microsoft.Network/publicIPAddresses/webhook",
            "publicIpName": "webhook", "webhookHost": "lampyr-example.azurefd.net",
            "frontDoorId": PRINCIPAL, "controllerPrincipalId": PRINCIPAL,
            "controllerClientId": PRINCIPAL, "fiberdClientId": PRINCIPAL,
            "controllerIdentityId": group + "/providers/Microsoft.ManagedIdentity/userAssignedIdentities/controller",
            "fiberdIdentityId": group + "/providers/Microsoft.ManagedIdentity/userAssignedIdentities/fiberd",
            "registryId": group + "/providers/Microsoft.ContainerRegistry/registries/lampyrtest",
            "registryLoginServer": "lampyrtest.azurecr.io", "publisherResourceGroup": "shared",
            "publisherRoleAssignments": [
                provider.resource_id(self.parameters, "providers", "Microsoft.Authorization", "roleAssignments", PRINCIPAL),
            ],
        }

    def test_invalid_configuration_fails_before_cloud_authentication(self):
        for value in ({"subscriptionId": "bad"}, {"nodeCount": True}, {"context": ""},
                      {"resourceGroupName": "bad/group"}, {"eventing": "true"}):
            request = copy.deepcopy(self.request)
            request["config"].update(value)
            with self.subTest(value=value), patch.object(provider, "authenticate") as auth:
                self.assertEqual(provider.invoke(request)["code"], "invalid-config")
                auth.assert_not_called()

    def test_images_must_be_prepublished_digests_before_provisioning(self):
        with patch.object(provider.hyperlight, "call", return_value={}):
            for image in (None, "registry/node-setup:tag", "registry/node-setup@sha256:bad"):
                self.request["config"]["nodeSetupImage"] = image
                with self.subTest(node_setup=image), self.assertRaises(provider.ProviderError):
                    provider.runtime_images(self.request, False)
            self.request["config"]["nodeSetupImage"] = NODE_SETUP_IMAGE
            self.assertEqual(provider.runtime_images(self.request, False), {
                "nodeSetupImage": NODE_SETUP_IMAGE,
            })
            controller_key, adapter_key = "eventGridControllerImage", "eventGridAdapterImage"
            valid_images = {
                controller_key: "registry/controller@sha256:" + "a" * 64,
                adapter_key: "registry/adapter@sha256:" + "b" * 64,
            }
            for key, invalid in ((controller_key, "registry/controller:latest"),
                                 (adapter_key, "registry/adapter@sha256:bad")):
                self.request["config"].update(valid_images)
                self.request["config"][key] = invalid
                with self.subTest(invalid_image=key), self.assertRaises(provider.ProviderError) as error:
                    provider.runtime_images(self.request, True)
                self.assertIn(key, str(error.exception))
            for missing, supplied, component in (
                (controller_key, adapter_key, "adapter"),
                (adapter_key, controller_key, "controller"),
            ):
                self.request["config"].pop(controller_key, None)
                self.request["config"].pop(adapter_key, None)
                self.request["config"][supplied] = f"registry/{component}@sha256:" + "a" * 64
                with self.subTest(missing=missing), self.assertRaises(provider.ProviderError) as error:
                    provider.runtime_images(self.request, True)
                self.assertIn(missing, str(error.exception))
            images = valid_images
            self.request["config"].update(images)
            self.assertEqual(provider.runtime_images(self.request, True), {
                "nodeSetupImage": NODE_SETUP_IMAGE,
                **images,
            })
            self.request["config"].pop(controller_key)
            self.request["config"].pop(adapter_key)
            with patch.object(provider.hyperlight, "call", return_value={
                "nodeSetupImage": NODE_SETUP_IMAGE,
                "eventGridImages": list(images.values()),
            }), self.assertRaises(provider.ProviderError) as error:
                provider.runtime_images(self.request, True)
            self.assertIn(controller_key, str(error.exception))

    def test_node_setup_is_a_manifest_not_host_commands(self):
        root = Path(__file__).resolve().parents[3]
        with tempfile.TemporaryDirectory() as directory:
            templates = Path(directory)
            source = root / "providers/azure/templates/aks-kvm-setup.yaml"
            (templates / source.name).write_bytes(source.read_bytes())
            with patch.object(provider, "TEMPLATES", templates), patch.object(provider.hyperlight, "call") as host:
                plan = provider.runtime_manifests(
                    self.request, self.parameters, self.outputs, {
                        "nodeSetupImage": NODE_SETUP_IMAGE,
                    },
                )
                host.assert_not_called()
                changed = {
                    "nodeSetupImage": NODE_SETUP_IMAGE.replace("c" * 64, "d" * 64),
                }
                changed_plan = provider.runtime_manifests(
                    self.request, self.parameters, self.outputs, changed,
                )
            self.assertEqual([item["kind"] for item in plan["before"]], ["DaemonSet"])
            # Node setup only loads KVM modules, which a kept cluster has
            # no reason to unload, so there is nothing to clean up.
            self.assertEqual(plan["cleanup"], [])
            install = plan["before"][0]["spec"]["template"]
            self.assertEqual(install["spec"]["initContainers"][0]["image"], NODE_SETUP_IMAGE)
            self.assertEqual(install["spec"]["initContainers"][0]["args"], ["install"])
            self.assertNotIn("env", install["spec"]["initContainers"][0])
            self.assertTrue(install["spec"]["initContainers"][0]["securityContext"]["privileged"])
            self.assertEqual(install["spec"]["initContainers"][0]["volumeMounts"], [{
                "name": "host-root",
                "mountPath": "/host",
                "readOnly": True,
            }])
            self.assertEqual(install["spec"]["volumes"], [{
                "name": "host-root",
                "hostPath": {"path": "/", "type": "Directory"},
            }])
            self.assertNotEqual(
                install["metadata"]["annotations"]["lampyr.dev/node-preparation"],
                changed_plan["before"][0]["spec"]["template"]["metadata"]["annotations"][
                    "lampyr.dev/node-preparation"
                ],
            )
            self.assertEqual(plan["after"], [])
            # fiberd-k8s-controller refuses a grant whose Pod template has
            # an AppArmor annotation, so the provider adds only the label.
            self.assertEqual(
                plan["options"]["homePodMetadata"],
                {"labels": {"azure.workload.identity/use": "true"}},
            )

    def test_node_setup_entrypoint_only_prepares_kvm(self):
        root = Path(__file__).resolve().parents[3]
        script = root / "providers/azure/node-setup/node-setup.sh"
        with tempfile.TemporaryDirectory() as directory:
            temporary = Path(directory)
            host = temporary / "host"
            (host / "dev").mkdir(parents=True)
            calls = temporary / "chroot-calls"
            chroot = temporary / "chroot"
            # modprobe succeeds but no /dev/kvm appears, as on a node size
            # without nested virtualization.
            chroot.write_text("#!/bin/sh\n" f"printf '%s\\n' \"$*\" >> {calls}\n")
            chroot.chmod(0o755)
            environment = dict(os.environ, HOST_ROOT=str(host), CHROOT=str(chroot))
            failed = subprocess.run([script, "install"], env=environment,
                                    capture_output=True, text=True)
            self.assertEqual(failed.returncode, 1)
            self.assertIn("/dev/kvm is unavailable", failed.stderr)
            self.assertIn("modprobe kvm", calls.read_text())
            calls.unlink()
            (host / "dev/kvm").symlink_to("/dev/null")
            subprocess.run([script, "install"], env=environment, check=True)
            # A node that already has /dev/kvm is left alone.
            self.assertFalse(calls.exists())
            self.assertEqual(subprocess.run([script, "remove"], env=environment,
                                            capture_output=True).returncode, 2)

    def test_eventing_inventory_cannot_escape_its_owned_resource_scope(self):
        output = dict(self.outputs, eventing=self.inventory)
        self.assertEqual(provider.eventing_inventory(self.parameters, self.request, output), self.inventory)
        for field in ("publicIpId", "registryId", "resourceGroupId"):
            changed = copy.deepcopy(output)
            changed["eventing"][field] = "/subscriptions/other/resourceGroups/unowned"
            with self.subTest(field=field), self.assertRaises(provider.ProviderError):
                provider.eventing_inventory(self.parameters, self.request, changed)

    def test_cloud_runtime_uses_published_images_and_workload_identity_without_host_calls(self):
        root = Path(__file__).resolve().parents[3]
        with tempfile.TemporaryDirectory() as directory:
            templates = Path(directory)
            for source in (root / "providers/azure/templates/aks-kvm-setup.yaml",
                           root / "providers/azure/eventgrid/eventgrid-source.yaml"):
                (templates / source.name).write_bytes(source.read_bytes())
            images = {
                "eventGridControllerImage": "registry/controller@sha256:" + "a" * 64,
                "eventGridAdapterImage": "registry/adapter@sha256:" + "b" * 64,
            }
            with patch.object(provider, "TEMPLATES", templates), patch.object(provider.hyperlight, "call") as host:
                plan = provider.runtime_manifests(self.request, self.parameters,
                                                 dict(self.outputs, eventing=self.inventory), {
                                                     "nodeSetupImage": NODE_SETUP_IMAGE,
                                                     **images,
                                                 })
                host.assert_not_called()
        deployments = [item for item in plan["after"] if item["kind"] == "Deployment"]
        self.assertEqual(
            [item["spec"]["template"]["spec"]["containers"][0]["image"] for item in deployments],
            [images["eventGridControllerImage"], images["eventGridAdapterImage"]],
        )
        self.assertEqual(plan["options"]["metadata"]["subscriptionId"], self.parameters["subscriptionId"])
        secrets = [item for item in plan["after"] if item["kind"] == "Secret"]
        self.assertEqual(secrets, [{
            "apiVersion": "v1", "kind": "Secret",
            "metadata": {
                "name": "eventgrid-webhook-auth", "namespace": "lampyr-system",
                "annotations": {"lampyr.dev/create-only": "true"},
            },
            "type": "Opaque",
        }])
        controller, adapter = deployments
        self.assertNotIn("volumes", controller["spec"]["template"]["spec"])
        self.assertNotIn("volumeMounts", controller["spec"]["template"]["spec"]["containers"][0])
        self.assertNotIn("--azure-webhook-auth-key-file",
                         " ".join(controller["spec"]["template"]["spec"]["containers"][0]["args"]))
        self.assertEqual(adapter["spec"]["template"]["spec"]["volumes"][0]["secret"]["secretName"],
                         "eventgrid-webhook-auth")
        self.assertEqual(adapter["spec"]["template"]["spec"]["containers"][0]["volumeMounts"], [{
            "name": "webhook-auth",
            "mountPath": "/var/run/lampyr-eventgrid/auth",
            "readOnly": True,
        }])
        roles = [item for item in plan["after"]
                 if item["kind"] == "Role" and item["metadata"]["name"] == "lampyr-eventgrid-controller-webhook-auth"]
        self.assertEqual(len(roles), 1)
        self.assertEqual(roles[0]["rules"], [{
            "apiGroups": [""], "resources": ["secrets"],
            "resourceNames": ["eventgrid-webhook-auth"], "verbs": ["get", "patch"],
        }])
        adapter_roles = [
            item for item in plan["after"]
            if item["kind"] == "Role"
            and item["metadata"]["name"] == "lampyr-eventgrid-adapter"
        ]
        self.assertEqual(len(adapter_roles), 1)
        self.assertIn("create", adapter_roles[0]["rules"][0]["verbs"])
        self.assertLess(len(json.dumps([{"key": "runtime", "value": plan}]).encode()), 64 * 1024)
