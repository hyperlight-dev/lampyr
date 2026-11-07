import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
from unittest.mock import Mock, patch

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "common"))
sys.modules["hyperlight"] = types.SimpleNamespace(call=Mock())
spec = importlib.util.spec_from_file_location("kind_provider", Path(__file__).resolve().parents[1] / "provider.py")
provider = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provider)

OWNER = "a" * 32


class ProviderTests(unittest.TestCase):
    def setUp(self):
        self.request = {"protocolVersion": 1, "operation": "apply", "installationId": OWNER,
                        "config": {"name": "test"}}
        self.saved = {"name": "test", "installationId": OWNER, "phase": "ready",
                      "nodes": {"test-control-plane": "container-id"}}
        self.container = {
            "Id": "container-id", "Config": {"Labels": {"io.x-k8s.kind.cluster": "test"}},
            "NetworkSettings": {"Networks": {provider.network_name(self.request): {}}},
        }
        self.registry = {
            "Id": "registry-id", "Config": {"Image": "registry:2", "Labels": {}},
            "HostConfig": {"PortBindings": {"5000/tcp": [{"HostIp": "127.0.0.1", "HostPort": "5002"}]}},
            "NetworkSettings": {"Networks": {}}, "State": {"Running": True},
        }

    def test_unknown_configuration_keys_fail_before_lifecycle_operations(self):
        for operation, handler in (("init", "initialize"), ("apply", "apply"), ("inspect", "inspect"),
                                   ("connect", "connect"), ("delete", "delete")):
            for key in ("registryPorrt", "eventingEnable"):
                request = copy.deepcopy(self.request)
                request.update(operation=operation)
                request["config"][key] = 5500
                with self.subTest(operation=operation, key=key), patch.object(provider, handler) as run:
                    result = provider.invoke(request)
                    self.assertEqual(result["status"], "error")
                    self.assertEqual(result["code"], "invalid-config")
                    self.assertIn(key, result["message"])
                    run.assert_not_called()

    def test_supported_provider_and_shared_configuration_survives_all_operations(self):
        self.request["config"].update({
            "interactive": False, "context": "customer", "deleteCluster": False,
            "images": ["registry/home:v1"], "fiberdImage": "registry/home:v1",
            "routerImage": "registry/router:v1", "domain": "localhost", "httpsPort": 18443,
            "eventing": True, "teardown": True,
            "source": "customer source", "httpPort": 18080, "registryPort": 5500,
        })
        for operation, handler in (("init", "initialize"), ("apply", "apply"), ("inspect", "inspect"),
                                   ("connect", "connect"), ("delete", "delete")):
            self.request["operation"] = operation
            with self.subTest(operation=operation), patch.object(provider, handler, return_value={}) as run:
                self.assertEqual(provider.invoke(self.request)["status"], "ok")
                run.assert_called_once_with(self.request)

    def test_only_packaged_kind_and_docker_can_execute(self):
        with patch.object(provider, "bounded_run", return_value=subprocess.CompletedProcess([], 0, "ok", "")) as run:
            self.assertEqual(provider.tool("kind", "version", request=self.request), "ok")
            self.assertEqual(run.call_args.args[0], ["/usr/local/bin/kind", "version"])
            self.assertEqual(run.call_args.kwargs["env"]["KIND_EXPERIMENTAL_DOCKER_NETWORK"], provider.network_name(self.request))
            for program in ("kubectl", "go", "/bin/sh"):
                with self.subTest(program=program), self.assertRaises(provider.ProviderError):
                    provider.tool(program, "version")

    def test_tool_output_limit_stops_the_process_before_full_capture(self):
        with self.assertRaises(provider.ProviderError) as error:
            provider.bounded_run(
                [sys.executable, "-c", "print('x' * 4096)"],
                timeout=5,
                stdout_limit=128,
                stderr_limit=128,
            )
        self.assertEqual(error.exception.code, "output-limit")

    def test_process_cleanup_kills_descendants_after_leader_exit(self):
        process = Mock(pid=1234)
        process.wait.return_value = 0
        with patch.object(provider.os, "killpg") as kill:
            provider.stop_process(process)
        self.assertEqual(
            kill.call_args_list,
            [
                unittest.mock.call(1234, provider.signal.SIGTERM),
                unittest.mock.call(1234, provider.signal.SIGKILL),
            ],
        )
        self.assertEqual(process.wait.call_count, 2)

    def test_owned_network_and_recorded_container_ids_are_both_required(self):
        with patch.object(provider, "verify_network"), patch.object(
            provider, "tool", side_effect=lambda program, *args, **kwargs:
            "test-control-plane\n" if program == "kind" else json.dumps([self.container])
        ):
            self.assertEqual(provider.verify_owner(self.request, self.saved), self.saved["nodes"])
            for mutation in ("container", "network", "cluster"):
                changed = copy.deepcopy(self.container)
                if mutation == "container":
                    changed["Id"] = "replacement"
                elif mutation == "network":
                    changed["NetworkSettings"]["Networks"] = {}
                else:
                    changed["Config"]["Labels"]["io.x-k8s.kind.cluster"] = "other"
                with self.subTest(mutation=mutation), patch.object(
                    provider, "tool", side_effect=lambda program, *args, **kwargs:
                    "test-control-plane\n" if program == "kind" else json.dumps([changed])
                ), self.assertRaises(provider.ProviderError):
                    provider.verify_owner(self.request, self.saved)

    def test_connect_returns_only_a_standard_kubeconfig(self):
        config = {"apiVersion": "v1", "kind": "Config", "current-context": "kind-test",
                  "contexts": [{"name": "kind-test", "context": {"cluster": "cluster", "user": "user"}}]}
        self.request["config"]["context"] = "customer"
        with patch.object(provider, "verify_owner"), patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "tool", return_value=yaml.safe_dump(config)):
            result = provider.connect(self.request)
        self.assertEqual(result["kind"], "Config")
        self.assertEqual(result["current-context"], "customer")
        self.assertEqual(result["contexts"][0]["context"]["cluster"], "cluster")
        self.assertNotIn("kubeconfig", result)

    def test_initialization_preserves_customer_source_and_requires_owner_labels(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "kind-config.yaml"
            original = yaml.safe_dump({"kind": "Cluster", "apiVersion": "kind.x-k8s.io/v1alpha4",
                                      "nodes": [{"role": "control-plane", "labels": {provider.OWNER_LABEL: OWNER}}]}) + "# customer\n"
            workspace.write_text(original)
            with patch.object(provider, "WORKSPACE", workspace), patch.object(provider.hyperlight, "call") as call:
                provider.initialize(self.request)
                call.assert_not_called()
                self.assertEqual(workspace.read_text(), original)
                workspace.write_text(original.replace(OWNER, "b" * 32))
                with self.assertRaises(provider.ProviderError):
                    provider.initialize(self.request)

    def test_default_ingress_ports_are_dynamically_allocated(self):
        template = Path(__file__).resolve().parents[1] / "templates/kind-config.yaml"
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "kind-config.yaml"
            with patch.object(provider, "WORKSPACE", workspace), \
                 patch.object(provider, "Path", return_value=template), \
                 patch.object(provider.hyperlight, "call") as call:
                provider.initialize(self.request)
        document = yaml.safe_load(call.call_args.args[1]["kind-config.yaml"])
        mappings = document["nodes"][0]["extraPortMappings"]
        self.assertEqual([mapping["hostPort"] for mapping in mappings], [0, 0])

    def test_network_creation_refuses_a_preexisting_unowned_network(self):
        with patch.object(provider, "tool", side_effect=[provider.network_name(self.request), "other-owner"]):
            with self.assertRaises(provider.ProviderError):
                provider.verify_network(self.request, create=True)

    def test_missing_prebuilt_images_fail_before_creating_a_cluster(self):
        self.request["config"]["images"] = ["lampyr.local/missing:v1"]
        for message, code in [("No such image: missing", "missing-image"),
                              ("permission denied", "image-inspect")]:
            with self.subTest(message=message), patch.object(provider, "owned_source", return_value={}), \
                 patch.object(provider, "tool_result", return_value=subprocess.CompletedProcess([], 1, "", message)), \
                 patch.object(provider, "exists") as exists:
                with self.assertRaises(provider.ProviderError) as error:
                    provider.apply(self.request)
                self.assertEqual(error.exception.code, code)
                exists.assert_not_called()

    def test_image_loads_target_one_verified_node_at_a_time(self):
        images = ["lampyr.local/fiberd:v1", "lampyr.local/router:v1"]
        self.request["config"]["images"] = images
        nodes = {
            "test-control-plane": "control-id",
            "test-worker": "worker-id",
            "test-worker2": "worker2-id",
        }
        with patch.object(provider, "owned_source", return_value={}), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "exists", return_value=True), \
             patch.object(provider, "verify_owner", return_value=nodes), \
             patch.object(provider, "save"), \
             patch.object(provider, "ingress_port", return_value=18443), \
             patch.object(provider, "ensure_registry", return_value={}), \
             patch.object(provider, "tool_result", return_value=subprocess.CompletedProcess([], 0, "", "")), \
             patch.object(provider, "tool") as tool:
            self.assertTrue(provider.apply(self.request)["ready"])
        self.assertEqual(tool.call_args_list, [
            unittest.mock.call(
                "kind", "load", "docker-image", "--name", "test",
                "--nodes", node, image, request=self.request,
            )
            for image in images for node in nodes
        ])

    def test_failed_node_image_load_stops_before_further_imports(self):
        self.request["config"]["images"] = ["lampyr.local/fiberd:v1", "lampyr.local/router:v1"]
        nodes = {"test-control-plane": "control-id", "test-worker": "worker-id"}
        failure = provider.ProviderError("tool-failed", "image import failed")
        with patch.object(provider, "owned_source", return_value={}), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "exists", return_value=True), \
             patch.object(provider, "verify_owner", return_value=nodes), \
             patch.object(provider, "save") as save, \
             patch.object(provider, "ensure_registry") as registry, \
             patch.object(provider, "tool_result", return_value=subprocess.CompletedProcess([], 0, "", "")), \
             patch.object(provider, "tool", side_effect=failure) as tool:
            with self.assertRaises(provider.ProviderError) as error:
                provider.apply(self.request)
        self.assertIs(error.exception, failure)
        tool.assert_called_once_with(
            "kind", "load", "docker-image", "--name", "test",
            "--nodes", "test-control-plane", "lampyr.local/fiberd:v1", request=self.request,
        )
        registry.assert_not_called()
        self.assertFalse(any(call.args[1:] == ("runtime",) for call in save.call_args_list))

    def test_retained_delete_verifies_ownership_without_deleting_kind(self):
        self.request["config"]["deleteCluster"] = False
        with patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "exists", return_value=True), \
             patch.object(provider, "verify_owner") as verify, \
             patch.object(provider, "tool") as tool:
            result = provider.delete(self.request)
        self.assertTrue(result["clusterRetained"])
        verify.assert_called_once_with(self.request, self.saved)
        tool.assert_not_called()

    def test_cluster_creation_records_source_digest_before_running_kind(self):
        document = {"kind": "Cluster", "apiVersion": "kind.x-k8s.io/v1alpha4", "nodes": [{}]}
        saves = []
        with patch.object(provider, "owned_source", return_value=document), \
             patch.object(provider, "state", return_value=None), \
             patch.object(provider, "exists", return_value=False), \
             patch.object(provider, "verify_network"), \
             patch.object(provider, "save",
                          side_effect=lambda value, key="cluster": saves.append((key, copy.deepcopy(value)))), \
             patch.object(provider, "tool"), \
             patch.object(provider, "verify_owner", return_value={"test-control-plane": "container-id"}), \
             patch.object(provider, "ingress_port", return_value=18443), \
             patch.object(provider, "ensure_registry", return_value={}):
            provider.apply(self.request)
        creating = saves[0][1]
        expected = provider.hashlib.sha256(
            json.dumps(document, sort_keys=True).encode()
        ).hexdigest()
        self.assertEqual(creating["phase"], "creating")
        self.assertEqual(creating["sourceDigest"], expected)

    def test_retry_recreates_an_incomplete_owned_cluster_before_ready(self):
        document = {"kind": "Cluster", "apiVersion": "kind.x-k8s.io/v1alpha4", "nodes": [{}]}
        digest = provider.hashlib.sha256(
            json.dumps(document, sort_keys=True).encode()
        ).hexdigest()
        saved = {
            "name": "test",
            "installationId": OWNER,
            "phase": "creating",
            "sourceDigest": digest,
        }
        with patch.object(provider, "owned_source", return_value=document), \
             patch.object(provider, "state", return_value=saved), \
             patch.object(provider, "exists", side_effect=[True, False]), \
             patch.object(provider, "verify_network"), \
             patch.object(provider, "verify_owner", return_value=self.saved["nodes"]) as verify, \
             patch.object(provider, "save"), \
             patch.object(provider, "ingress_port", return_value=18443), \
             patch.object(provider, "ensure_registry", return_value={}), \
             patch.object(provider, "tool") as tool:
            provider.apply(self.request)
        self.assertEqual(verify.call_count, 2)
        self.assertIn(
            unittest.mock.call(
                "kind", "delete", "cluster", "--name", "test",
                request=self.request,
            ),
            tool.call_args_list,
        )
        self.assertTrue(any(
            call.args[:3] == ("kind", "create", "cluster")
            for call in tool.call_args_list
        ))

    def test_registry_reuses_an_existing_registry_without_taking_ownership(self):
        name = "existing-registry"

        def docker(program, *args, **kwargs):
            if args[:2] == ("container", "ls"):
                return ""
            if args[0] == "ps":
                return name
            if args[0] == "inspect":
                return json.dumps([self.registry])
            return ""

        with tempfile.TemporaryDirectory() as directory, \
             patch.object(provider, "Path", return_value=Path(directory) / "hosts.toml"), \
             patch.object(provider, "state", return_value=None), patch.object(provider, "save") as save, \
             patch.object(provider, "verify_owner", return_value={}), patch.object(provider, "tool", side_effect=docker) as tool:
            result = provider.ensure_registry(self.request)
        self.assertEqual(result, {"localhost:5002": {"registry": name + ":5000", "plainHttp": True}})
        save.assert_called_once_with({"name": name, "port": 5002, "id": "registry-id", "owned": False}, "platform-registry")
        self.assertFalse(any(call.args[1] in ("run", "start", "rm") for call in tool.call_args_list))
        self.assertIn(unittest.mock.call("docker", "network", "connect", provider.network_name(self.request), name), tool.call_args_list)

    def test_saved_shared_registry_is_reused_and_validated(self):
        saved = {"name": "existing-registry", "port": 5002, "id": "registry-id", "owned": False}
        self.registry["NetworkSettings"]["Networks"][provider.network_name(self.request)] = {}
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(provider, "Path", return_value=Path(directory) / "hosts.toml"), \
             patch.object(provider, "state", return_value=saved), patch.object(provider, "save"), \
             patch.object(provider, "verify_owner", return_value={}), \
             patch.object(provider, "tool", return_value=json.dumps([self.registry])) as tool:
            provider.ensure_registry(self.request)
            tool.assert_called_once_with("docker", "inspect", saved["name"])
            for mutation, code in (("owner", "not-owned"), ("image", "invalid-registry"),
                                   ("stopped", "invalid-registry"), ("id", "ownership-mismatch"),
                                   ("binding", "invalid-config")):
                container = copy.deepcopy(self.registry)
                if mutation == "owner":
                    container["Config"]["Labels"][provider.OWNER_LABEL] = "b" * 32
                elif mutation == "image":
                    container["Config"]["Image"] = "nginx:latest"
                elif mutation == "stopped":
                    container["State"]["Running"] = False
                elif mutation == "id":
                    container["Id"] = "replacement"
                else:
                    container["HostConfig"]["PortBindings"]["5000/tcp"][0]["HostIp"] = "0.0.0.0"
                tool.return_value = json.dumps([container])
                with self.subTest(mutation=mutation), self.assertRaises(provider.ProviderError) as error:
                    provider.ensure_registry(self.request)
                self.assertEqual(error.exception.code, code)

    def test_registry_still_reuses_owned_containers_and_legacy_state(self):
        name = "lampyr-platform-" + OWNER
        saved = {"name": name, "port": 5002, "id": "registry-id"}
        self.registry["Config"]["Labels"][provider.OWNER_LABEL] = OWNER
        self.registry["NetworkSettings"]["Networks"][provider.network_name(self.request)] = {}
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(provider, "Path", return_value=Path(directory) / "hosts.toml"), \
             patch.object(provider, "state", return_value=saved), patch.object(provider, "save") as save, \
             patch.object(provider, "verify_owner", return_value={}), patch.object(provider, "tool", side_effect=[
                 name, json.dumps([self.registry]), "",
             ]) as tool:
            provider.ensure_registry(self.request)
        save.assert_called_once_with({**saved, "owned": True}, "platform-registry")
        self.assertIn(unittest.mock.call("docker", "start", name), tool.call_args_list)

    def test_missing_owned_registry_is_recreated_despite_saved_identity(self):
        name = "lampyr-platform-" + OWNER
        self.registry["Config"]["Labels"][provider.OWNER_LABEL] = OWNER
        self.registry["HostConfig"]["PortBindings"]["5000/tcp"][0]["HostPort"] = "5502"
        self.registry["NetworkSettings"]["Networks"][provider.network_name(self.request)] = {}
        for ownership in ({}, {"owned": True}):
            saved = {"name": name, "port": 5502, "id": "removed-id", **ownership}
            with self.subTest(ownership=ownership), \
                 patch.object(provider, "Path") as path, \
                 patch.object(provider, "state", side_effect=lambda key="cluster":
                              saved if key == "platform-registry" else self.saved), \
                 patch.object(provider, "save") as save, \
                 patch.object(provider, "verify_owner", return_value={}) as verify, \
                 patch.object(provider, "tool", side_effect=[
                     "", "registry-id", json.dumps([self.registry]), "",
                 ]) as tool:
                result = provider.ensure_registry(self.request)
                self.assertEqual(result, {"localhost:5502": {"registry": name + ":5000", "plainHttp": True}})
                self.assertEqual(save.call_args, unittest.mock.call(
                    {"name": name, "port": 5502, "id": "registry-id", "owned": True}, "platform-registry",
                ))
                self.assertIn(unittest.mock.call(
                    "docker", "run", "-d", "--name", name, "--label", provider.OWNER_LABEL + "=" + OWNER,
                    "--restart=always", "-p", "127.0.0.1:5502:5000", "registry:2",
                ), tool.call_args_list)
                self.assertIn(unittest.mock.call("docker", "start", name), tool.call_args_list)
                verify.assert_called_once_with(self.request, self.saved)
                path.return_value.write_text.assert_called_once()

    def test_existing_owned_registry_replacement_is_not_adopted(self):
        name = "lampyr-platform-" + OWNER
        for ownership in ({}, {"owned": True}):
            for owner, code in ((OWNER, "ownership-mismatch"), ("b" * 32, "not-owned")):
                saved = {"name": name, "port": 5002, "id": "removed-id", **ownership}
                self.registry["Config"]["Labels"][provider.OWNER_LABEL] = owner
                with self.subTest(ownership=ownership, owner=owner), \
                     patch.object(provider, "Path") as path, \
                     patch.object(provider, "state", return_value=saved), \
                     patch.object(provider, "save") as save, \
                     patch.object(provider, "tool", side_effect=[name, json.dumps([self.registry])]) as tool:
                    with self.assertRaises(provider.ProviderError) as error:
                        provider.ensure_registry(self.request)
                self.assertEqual(error.exception.code, code)
                self.assertEqual([call.args[1] for call in tool.call_args_list], ["container", "inspect"])
                save.assert_not_called()
                path.assert_not_called()

    def test_recreated_registry_still_requires_owner_and_loopback_port(self):
        name = "lampyr-platform-" + OWNER
        saved = {"name": name, "port": 5002, "id": "removed-id", "owned": True}
        for mutation, code in (("owner", "not-owned"), ("port", "invalid-config"),
                               ("address", "invalid-config")):
            container = copy.deepcopy(self.registry)
            container["Config"]["Labels"][provider.OWNER_LABEL] = OWNER
            if mutation == "owner":
                container["Config"]["Labels"][provider.OWNER_LABEL] = "b" * 32
            elif mutation == "port":
                container["HostConfig"]["PortBindings"]["5000/tcp"][0]["HostPort"] = "5003"
            else:
                container["HostConfig"]["PortBindings"]["5000/tcp"][0]["HostIp"] = "0.0.0.0"
            with self.subTest(mutation=mutation), \
                 patch.object(provider, "Path") as path, \
                 patch.object(provider, "state", return_value=saved), \
                 patch.object(provider, "save") as save, \
                 patch.object(provider, "tool", side_effect=["", "registry-id", json.dumps([container])]) as tool:
                with self.assertRaises(provider.ProviderError) as error:
                    provider.ensure_registry(self.request)
                self.assertEqual(error.exception.code, code)
                self.assertEqual([call.args[1] for call in tool.call_args_list], ["container", "run", "inspect"])
                save.assert_not_called()
                path.assert_not_called()

    def test_delete_disconnects_shared_registry_and_removes_only_owned_leftovers(self):
        name = "existing-registry"
        saved = {"name": name, "port": 5002, "id": "registry-id", "owned": False}
        self.registry["NetworkSettings"]["Networks"][provider.network_name(self.request)] = {}
        owned_name = "lampyr-platform-" + OWNER
        leftover = {"Id": "failed-registry-id", "Config": {"Labels": {provider.OWNER_LABEL: OWNER}}}
        with patch.object(provider, "exists", return_value=False), patch.object(provider, "state", return_value=saved), \
             patch.object(provider, "save") as save, patch.object(provider, "verify_network"), \
             patch.object(provider, "tool", side_effect=[
                 name, json.dumps([self.registry]), "", owned_name, json.dumps([leftover]), "",
                 provider.network_name(self.request), "",
             ]) as tool:
            provider.delete(self.request)
        self.assertIn(unittest.mock.call("docker", "network", "disconnect", provider.network_name(self.request), name), tool.call_args_list)
        self.assertIn(unittest.mock.call("docker", "rm", "--force", owned_name), tool.call_args_list)
        self.assertNotIn(unittest.mock.call("docker", "rm", "--force", name), tool.call_args_list)
        self.assertEqual(save.call_args.args[0]["name"], "test")

    def test_delete_recovers_an_owned_registry_created_before_state_was_saved(self):
        name = "lampyr-platform-" + OWNER
        container = {"Config": {"Labels": {provider.OWNER_LABEL: OWNER}}}
        with patch.object(provider, "exists", return_value=False), patch.object(provider, "state", return_value=None), \
             patch.object(provider, "save"), patch.object(provider, "tool", side_effect=[
                 name, json.dumps([container]), "", "",
             ]) as tool:
            provider.delete(self.request)
            self.assertIn(unittest.mock.call("docker", "rm", "--force", name), tool.call_args_list)
