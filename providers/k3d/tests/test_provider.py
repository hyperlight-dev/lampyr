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
spec = importlib.util.spec_from_file_location("k3d_provider", Path(__file__).resolve().parents[1] / "provider.py")
provider = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provider)

OWNER = "a" * 32


class ProviderTests(unittest.TestCase):
    def setUp(self):
        self.request = {"protocolVersion": 1, "operation": "apply",
                        "installationId": OWNER, "config": {"name": "test"}}
        self.node = "k3d-test-server-0"
        self.saved = {"name": "test", "installationId": OWNER, "phase": "ready",
                      "nodes": {self.node: "container-id"}}
        self.container = {
            "Id": "container-id",
            "Config": {"Labels": {"k3d.cluster": "test", provider.OWNER_LABEL: OWNER}},
            "NetworkSettings": {"Networks": {provider.network_name(self.request): {}},
                                "Ports": {"30443/tcp": [{"HostIp": "127.0.0.1", "HostPort": "18443"}]}},
        }
        self.source = (Path(__file__).resolve().parents[1] / "templates/k3d-config.yaml").read_text()
        for key, value in {"NAME": "test", "INSTALLATION_ID": OWNER,
                           "NETWORK": provider.network_name(self.request),
                           "HTTP_PORT": "0", "HTTPS_PORT": "0"}.items():
            self.source = self.source.replace("__" + key + "__", value)

    def test_config_rejects_unknown_fields_and_invalid_values_before_tools(self):
        for key, value in (("registryPorrt", 5002), ("httpPort", True), ("registryPort", 0),
                           ("httpsPort", 65536), ("name", "-bad"), ("images", ["--help"]),
                           ("context", "-bad"), ("eventing", "yes"), ("name", "a" * 33)):
            for operation in ("init", "apply", "inspect", "connect", "delete"):
                request = copy.deepcopy(self.request)
                request["config"][key] = value
                request["operation"] = operation
                with self.subTest(key=key, operation=operation), patch.object(provider, "tool") as tool:
                    result = provider.invoke(request)
                    self.assertEqual(result["code"], "invalid-config")
                    tool.assert_not_called()

    def test_only_packaged_k3d_and_docker_can_execute(self):
        with patch.object(provider, "bounded_run", return_value=subprocess.CompletedProcess([], 0, "ok", "")) as run:
            self.assertEqual(provider.tool("k3d", "version"), "ok")
            self.assertEqual(run.call_args.args[0], ["/usr/local/bin/k3d", "version"])
            for command in ("kind", "kubectl", "/bin/sh"):
                with self.assertRaises(provider.ProviderError):
                    provider.tool(command)

    def test_template_has_kvm_k3s_settings_and_dynamic_loopback_ingress(self):
        document = provider.configuration(self.source, self.request)
        self.assertEqual(document["volumes"][0]["volume"], "/dev/kvm:/dev/kvm:ro")
        self.assertEqual(document["ports"][1]["port"], "127.0.0.1:0:30443")
        self.assertEqual(document["ports"][1]["nodeFilters"], ["server:0:direct"])
        self.assertEqual(document["kubeAPI"]["hostIP"], "127.0.0.1")
        args = document["options"]["k3s"]["extraArgs"]
        self.assertIn({"arg": "--disable=traefik", "nodeFilters": ["server:*"]}, args)
        self.assertIn({"arg": "--disable=servicelb", "nodeFilters": ["server:*"]}, args)

    def test_source_must_publish_ingress_on_loopback_before_creation(self):
        provider.configuration(self.source, self.request)
        document = yaml.safe_load(self.source)
        document["ports"][1]["port"] = "127.0.0.1:0:30443/tcp"
        provider.configuration(yaml.safe_dump(document), self.request)
        for ports in ([], [{"port": "0.0.0.0:0:30443"}], [{"port": "127.0.0.1:0:30080"}]):
            document["ports"] = ports
            with self.subTest(ports=ports), self.assertRaises(provider.ProviderError) as error:
                provider.configuration(yaml.safe_dump(document), self.request)
            self.assertEqual(error.exception.code, "invalid-source")

    def test_initialization_preserves_customer_source_and_validates_ownership(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "k3d-config.yaml"
            source.write_text(self.source + "\n# customer change\n")
            with patch.object(provider, "WORKSPACE", source), patch.object(provider.hyperlight, "call") as call:
                provider.initialize(self.request)
                self.assertTrue(source.read_text().endswith("# customer change\n"))
                call.assert_not_called()
                source.write_text(self.source.replace(OWNER, "b" * 32))
                with self.assertRaises(provider.ProviderError):
                    provider.initialize(self.request)

    def test_initial_custom_source_cannot_silently_change_cluster_name(self):
        self.request["config"]["source"] = self.source.replace("name: test", "name: another")
        with patch.object(provider, "WORKSPACE") as workspace, \
             patch.object(provider.hyperlight, "call") as call:
            workspace.exists.return_value = False
            with self.assertRaises(provider.ProviderError):
                provider.initialize(self.request)
            call.assert_not_called()

    def test_unowned_or_replaced_nodes_are_never_adopted(self):
        for mutation in ("state", "id", "owner", "network", "cluster"):
            container = copy.deepcopy(self.container)
            saved = copy.deepcopy(self.saved)
            if mutation == "state":
                saved = None
            elif mutation == "id":
                container["Id"] = "replacement"
            elif mutation == "owner":
                container["Config"]["Labels"][provider.OWNER_LABEL] = "b" * 32
            elif mutation == "cluster":
                container["Config"]["Labels"]["k3d.cluster"] = "other"
            else:
                container["NetworkSettings"]["Networks"] = {}
            with self.subTest(mutation=mutation), patch.object(provider, "verify_network"), \
                 patch.object(provider, "tool", return_value=json.dumps([container])):
                with self.assertRaises(provider.ProviderError):
                    provider.verify_owner(self.request, saved, [self.node])

    def test_delete_accepts_survivors_of_an_interrupted_delete_but_not_new_nodes(self):
        self.saved["nodes"] = {self.node: "container-id", "k3d-test-agent-0": "agent-id"}
        with patch.object(provider, "verify_network"), \
             patch.object(provider, "tool", return_value=json.dumps([self.container])):
            self.assertEqual(provider.verify_owner(self.request, self.saved, [self.node], partial=True),
                             {self.node: "container-id"})
            with self.assertRaises(provider.ProviderError):
                provider.verify_owner(self.request, self.saved, [self.node])
        with patch.object(provider, "verify_network"), \
             patch.object(provider, "tool", return_value=json.dumps([self.container])):
            # An unrecorded tools container is not adopted either.
            with self.assertRaises(provider.ProviderError):
                provider.verify_owner(self.request, self.saved, [self.node, "k3d-test-tools"], partial=True)
        self.container["Id"] = "replaced-id"
        with patch.object(provider, "verify_network"), \
             patch.object(provider, "tool", return_value=json.dumps([self.container])):
            with self.assertRaises(provider.ProviderError):
                provider.verify_owner(self.request, self.saved, [self.node], partial=True)

    def test_uninstall_inspection_accepts_survivors_of_an_interrupted_delete(self):
        self.saved["nodes"] = {self.node: "container-id", "k3d-test-agent-0": "agent-id"}
        with patch.object(provider, "containers", return_value=[self.node]), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "verify_network"), \
             patch.object(provider, "tool", return_value=json.dumps([self.container])):
            with self.assertRaises(provider.ProviderError):
                provider.inspect(self.request)
            self.request["config"]["teardown"] = True
            self.assertTrue(provider.inspect(self.request)["exists"])

    def test_network_ownership_is_required_even_when_name_matches(self):
        with patch.object(provider, "tool", side_effect=[provider.network_name(self.request), "someone-else"]):
            with self.assertRaises(provider.ProviderError):
                provider.verify_network(self.request, create=True)

    def test_connect_returns_plain_kubeconfig_with_selected_context(self):
        document = {"apiVersion": "v1", "kind": "Config", "current-context": "k3d-test",
                    "clusters": [{"name": "test", "cluster": {"server": "https://127.0.0.1:0"}}],
                    "contexts": [{"name": "k3d-test", "context": {"cluster": "test", "user": "test"}}]}
        self.request["config"]["context"] = "customer"
        with patch.object(provider, "containers", return_value=[self.node]), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "verify_owner"), \
             patch.object(provider, "published_port", return_value=54321), \
             patch.object(provider, "tool", return_value=yaml.safe_dump(document)):
            result = provider.connect(self.request)
        self.assertEqual(result["kind"], "Config")
        self.assertEqual(result["current-context"], "customer")
        self.assertEqual(result["contexts"][0]["name"], "customer")
        self.assertEqual(result["clusters"][0]["cluster"]["server"], "https://127.0.0.1:54321")
        self.assertNotIn("kubeconfig", result)

    def test_retained_uninstall_does_not_delete_registry_network_or_nodes(self):
        self.request["config"]["deleteCluster"] = False
        with patch.object(provider, "containers", return_value=[self.node]), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "verify_owner") as verify, patch.object(provider, "tool") as tool:
            self.assertTrue(provider.delete(self.request)["clusterRetained"])
            verify.assert_called_once()
            tool.assert_not_called()

    def test_changed_source_is_refused_before_registry_or_image_import(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "k3d-config.yaml"
            source.write_text(self.source)
            self.saved["sourceDigest"] = "old"
            with patch.object(provider, "WORKSPACE", source), \
                 patch.object(provider, "containers", return_value=[self.node]), \
                 patch.object(provider, "state", return_value=self.saved), \
                 patch.object(provider, "verify_owner"), patch.object(provider, "registry") as registry:
                with self.assertRaises(provider.ProviderError) as error:
                    provider.apply(self.request)
                self.assertEqual(error.exception.code, "changed-source")
                registry.assert_not_called()

    def test_registry_refuses_replaced_or_unowned_container(self):
        registry_name = provider.registry_name(self.request)
        for saved, owner in (({"id": "old", "port": 5002}, OWNER), (None, "b" * 32)):
            container = {"Id": "registry-id", "Config": {"Labels": {provider.OWNER_LABEL: owner}}}
            with patch.object(provider, "state", return_value=saved), \
                 patch.object(provider, "tool", side_effect=[registry_name, json.dumps([container])]):
                with self.assertRaises(provider.ProviderError):
                    provider.registry(self.request)

    def test_missing_registry_and_volume_are_recreated_despite_saved_identity(self):
        registry_name = provider.registry_name(self.request)
        network = provider.network_name(self.request)
        saved = {"platform-registry": {"name": registry_name, "id": "old-id", "port": 5502}}
        def state(key="cluster"):
            return saved.get(key)
        def save(value, key="cluster"):
            saved[key] = value
        created = {"Id": "created-id", "Config": {"Labels": {provider.OWNER_LABEL: OWNER}},
                   "HostConfig": {"PortBindings": {"5000/tcp": [{"HostIp": "127.0.0.1", "HostPort": "5502"}]}},
                   "NetworkSettings": {"Networks": {network: {}}}}
        failed_run = provider.ProviderError("tool-failed", "port is already allocated")
        with patch.object(provider, "state", side_effect=state), \
             patch.object(provider, "save", side_effect=save):
            # docker run creates the container but fails to start it.
            with patch.object(provider, "tool", side_effect=["", failed_run]), \
                 self.assertRaises(provider.ProviderError):
                provider.registry(self.request, create=True)
            self.assertEqual(saved["platform-registry"], {"name": registry_name, "id": None, "port": 5502})
            # The next install adopts its own container on the saved port.
            with patch.object(provider, "tool", side_effect=[registry_name, json.dumps([created]), ""]):
                self.assertEqual(provider.registry(self.request, create=True), registry_name)
            self.assertEqual(saved["platform-registry"], {"name": registry_name, "id": "created-id", "port": 5502})
        volume = {"Name": "k3d-test-images", "CreatedAt": "new", "Labels": {provider.OWNER_LABEL: OWNER}}
        stale = {"name": "k3d-test-images", "createdAt": "old"}
        saved = {"image-volume": stale}
        with patch.object(provider, "state", side_effect=state), \
             patch.object(provider, "save", side_effect=save):
            with patch.object(provider, "tool", side_effect=["", provider.ProviderError("tool-failed", "create")]), \
                 self.assertRaises(provider.ProviderError):
                provider.image_volume(self.request, create=True)
            self.assertIsNone(saved["image-volume"])
            # The next install adopts the volume the failed call may have created.
            with patch.object(provider, "tool", side_effect=["k3d-test-images", json.dumps([volume])]):
                self.assertEqual(provider.image_volume(self.request, create=True), "k3d-test-images")
            saved["image-volume"] = stale
            with patch.object(provider, "tool", side_effect=["", "", json.dumps([volume])]) as tool:
                self.assertEqual(provider.image_volume(self.request, create=True), "k3d-test-images")
                self.assertIn("app=k3d", tool.call_args_list[1].args)
        self.assertEqual(saved["image-volume"], {"name": "k3d-test-images", "createdAt": "new"})

    def test_reinstall_keeps_saved_registry_port_without_repeating_configuration(self):
        with patch.object(provider, "state", return_value={"port": 5502}):
            self.assertEqual(provider.registry_port(self.request), 5502)
            self.request["config"]["registryPort"] = 6502
            self.assertEqual(provider.registry_port(self.request), 6502)

    def test_volume_refuses_unowned_and_replaced_resources(self):
        name = "k3d-test-images"
        volume = {"Name": name, "CreatedAt": "new", "Labels": {provider.OWNER_LABEL: OWNER}}
        for saved, owner in (({"name": name, "createdAt": "old"}, OWNER), (None, "b" * 32)):
            volume["Labels"][provider.OWNER_LABEL] = owner
            with self.subTest(owner=owner), patch.object(provider, "state", return_value=saved), \
                 patch.object(provider, "tool", side_effect=[name, json.dumps([volume])]):
                with self.assertRaises(provider.ProviderError):
                    provider.image_volume(self.request)

    def test_full_delete_validates_all_resources_before_deleting_anything(self):
        with patch.object(provider, "containers", return_value=[self.node]), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "verify_owner"), \
             patch.object(provider, "registry", side_effect=provider.ProviderError("not-owned", "foreign registry")), \
             patch.object(provider, "tool") as tool:
            with self.assertRaises(provider.ProviderError):
                provider.delete(self.request)
            tool.assert_not_called()

    def test_delete_refuses_when_k3d_would_remove_a_foreign_volume(self):
        def tool(*args):
            if args[:3] == ("docker", "volume", "ls"):
                self.assertEqual(args[3:], ("--filter", "label=app=k3d", "--filter", "label=k3d.cluster=test",
                                            "--format", "{{.Name}}"))
                return "k3d-test-images\nk3d-test-other\n"
            if args[:3] == ("docker", "network", "ls"):
                return ""
            self.fail(f"unexpected tool call {args}")
        with patch.object(provider, "containers", return_value=[self.node]), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "verify_owner"), \
             patch.object(provider, "registry", return_value=None), \
             patch.object(provider, "image_volume", return_value="k3d-test-images"), \
             patch.object(provider, "tool", side_effect=tool):
            with self.assertRaises(provider.ProviderError) as error:
                provider.delete(self.request)
            self.assertEqual(error.exception.code, "not-owned")

    def test_missing_image_fails_before_provisioning(self):
        self.request["config"]["images"] = ["lampyr.local/missing:test"]
        with patch.object(provider, "WORKSPACE") as source, \
             patch.object(provider, "tool_result", return_value=subprocess.CompletedProcess([], 1, "", "No such image: missing")), \
             patch.object(provider, "containers") as containers:
            source.read_text.return_value = self.source
            with self.assertRaises(provider.ProviderError) as error:
                provider.apply(self.request)
            self.assertEqual(error.exception.code, "missing-image")
            containers.assert_not_called()

    def test_local_images_must_be_referenced_by_tag(self):
        self.request["config"]["images"] = ["lampyr.local/fiberd@sha256:" + "0" * 64]
        with patch.object(provider, "WORKSPACE") as source, \
             patch.object(provider, "tool_result", return_value=subprocess.CompletedProcess([], 0, "", "")), \
             patch.object(provider, "state", return_value=None), \
             patch.object(provider, "containers", return_value=[]), \
             patch.object(provider, "verify_network"), \
             patch.object(provider, "image_volume"), \
             patch.object(provider, "registry", return_value="owned-registry"), \
             patch.object(provider, "save"), \
             patch.object(provider, "tool") as tool:
            source.read_text.return_value = self.source
            with self.assertRaises(provider.ProviderError) as error:
                provider.apply(self.request)
            self.assertEqual(error.exception.code, "missing-image")
            self.assertNotIn(unittest.mock.call("k3d", "cluster", "create"), tool.call_args_list)

    def test_published_port_rejects_non_loopback_bindings(self):
        with patch.object(provider, "tool", return_value=json.dumps([self.container])):
            self.assertEqual(provider.ingress_port([self.node]), 18443)
        self.container["NetworkSettings"]["Ports"]["30443/tcp"][0]["HostIp"] = "0.0.0.0"
        with patch.object(provider, "tool", return_value=json.dumps([self.container])):
            with self.assertRaises(provider.ProviderError):
                provider.ingress_port([self.node])

    def test_partial_retry_removes_only_owned_cluster_and_preserves_source(self):
        document = provider.configuration(self.source, self.request)
        self.saved.update(phase="creating", sourceDigest=provider.hashlib.sha256(
            json.dumps(document, sort_keys=True).encode()).hexdigest())
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "k3d-config.yaml"
            effective = Path(directory) / "effective.yaml"
            source.write_text(self.source)
            with patch.object(provider, "WORKSPACE", source), \
                 patch.object(provider, "Path", return_value=effective), \
                 patch.object(provider, "state", side_effect=lambda key="cluster": self.saved if key == "cluster" else None), \
                 patch.object(provider, "containers", side_effect=[[self.node], [], [self.node]]), \
                 patch.object(provider, "verify_network"), \
                 patch.object(provider, "image_volume", side_effect=["k3d-test-images", None, "k3d-test-images"]), \
                 patch.object(provider, "registry", return_value="owned-registry"), \
                 patch.object(provider, "verify_owner", return_value=self.saved["nodes"]), \
                 patch.object(provider, "ingress_port", return_value=18443), \
                 patch.object(provider, "save") as save, \
                 patch.object(provider, "tool", return_value="") as tool:
                self.assertTrue(provider.apply(self.request)["ready"])
                tool.assert_any_call("k3d", "cluster", "delete", "test")
                save.assert_any_call(None, "image-volume")
            self.assertEqual(source.read_text(), self.source)

    def test_partial_retry_accepts_a_corrected_source(self):
        self.saved.update(phase="creating", sourceDigest="digest-of-the-failed-source")
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "k3d-config.yaml"
            source.write_text(self.source)
            with patch.object(provider, "WORKSPACE", source), \
                 patch.object(provider, "Path", return_value=Path(directory) / "effective.yaml"), \
                 patch.object(provider, "state", side_effect=lambda key="cluster": self.saved if key == "cluster" else None), \
                 patch.object(provider, "containers", side_effect=[[self.node], [], [self.node]]), \
                 patch.object(provider, "verify_network"), \
                 patch.object(provider, "image_volume", side_effect=["k3d-test-images", None, "k3d-test-images"]), \
                 patch.object(provider, "registry", return_value="owned-registry"), \
                 patch.object(provider, "verify_owner", return_value=self.saved["nodes"]), \
                 patch.object(provider, "ingress_port", return_value=18443), \
                 patch.object(provider, "save"), \
                 patch.object(provider, "tool", return_value="") as tool:
                self.assertTrue(provider.apply(self.request)["ready"])
                tool.assert_any_call("k3d", "cluster", "delete", "test")

    def test_import_and_registry_configuration_do_not_mutate_source(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "k3d-config.yaml"
            effective = Path(directory) / "effective.yaml"
            source.write_text(self.source)
            self.request["config"]["images"] = ["lampyr.local/fiberd:test", "example.test/router@sha256:" + "0" * 64]
            saves = []
            with patch.object(provider, "WORKSPACE", source), \
                 patch.object(provider, "Path", return_value=effective), \
                 patch.object(provider, "state", return_value=None), \
                 patch.object(provider, "containers", side_effect=[[], [self.node]]), \
                 patch.object(provider, "verify_network"), \
                 patch.object(provider, "image_volume"), \
                 patch.object(provider, "registry", return_value="owned-registry"), \
                 patch.object(provider, "verify_owner", return_value=self.saved["nodes"]), \
                 patch.object(provider, "ingress_port", return_value=18443), \
                 patch.object(provider, "tool_result", return_value=subprocess.CompletedProcess([], 0, "", "")), \
                 patch.object(provider, "save", side_effect=lambda value, key="cluster": saves.append((key, copy.deepcopy(value)))), \
                 patch.object(provider, "tool", return_value="") as tool:
                self.assertTrue(provider.apply(self.request)["ready"])
            self.assertEqual(source.read_text(), self.source)
            config = yaml.safe_load(yaml.safe_load(effective.read_text())["registries"]["config"])
            self.assertEqual(config["mirrors"]["localhost:5002"]["endpoint"], ["http://owned-registry:5000"])
            self.assertEqual(saves[0][1]["phase"], "creating")
            self.assertIn("sourceDigest", saves[0][1])
            self.assertEqual(saves[-1][1]["options"]["httpsPort"], 18443)
            tool.assert_any_call("k3d", "image", "import", "--mode", "direct", "--cluster", "test", "lampyr.local/fiberd:test")

    def test_digest_only_images_are_not_imported(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "k3d-config.yaml"
            effective = Path(directory) / "effective.yaml"
            source.write_text(self.source)
            self.request["config"]["images"] = ["example.test/router@sha256:" + "0" * 64]
            saves = []
            with patch.object(provider, "WORKSPACE", source), \
                 patch.object(provider, "Path", return_value=effective), \
                 patch.object(provider, "state", return_value=None), \
                 patch.object(provider, "containers", side_effect=[[], [self.node]]), \
                 patch.object(provider, "verify_network"), \
                 patch.object(provider, "image_volume"), \
                 patch.object(provider, "registry", return_value="owned-registry"), \
                 patch.object(provider, "verify_owner", return_value=self.saved["nodes"]), \
                 patch.object(provider, "ingress_port", return_value=18443), \
                 patch.object(provider, "tool_result", return_value=subprocess.CompletedProcess([], 0, "", "")), \
                 patch.object(provider, "save", side_effect=lambda value, key="cluster": saves.append((key, copy.deepcopy(value)))), \
                 patch.object(provider, "tool", return_value="") as tool:
                self.assertTrue(provider.apply(self.request)["ready"])
            self.assertFalse(any(call.args[:3] == ("k3d", "image", "import") for call in tool.call_args_list))


if __name__ == "__main__":
    unittest.main()
