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


sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "eventgrid"))
sys.modules["hyperlight"] = types.SimpleNamespace(call=Mock())
spec = importlib.util.spec_from_file_location("azure_provider", Path(__file__).resolve().parents[1] / "provider.py")
provider = importlib.util.module_from_spec(spec)
spec.loader.exec_module(provider)

OWNER = "a" * 32
PRINCIPAL = "11111111-1111-1111-1111-111111111111"


class ProviderTests(unittest.TestCase):
    def test_compilation_reports_progress_before_and_after_the_command(self):
        events = []
        with patch.object(provider, "progress", side_effect=lambda message: events.append(message)), \
             patch.object(provider, "az", side_effect=lambda *args: events.append(args)):
            provider.compile_template("main.bicep", Path("main.json"))
        self.assertEqual(events, [
            "Compiling Azure infrastructure template main.bicep...",
            ("bicep", "build", "--file", "/workspace/main.bicep", "--outfile", "main.json"),
            "Compiled Azure infrastructure template main.bicep.",
        ])

    def test_failed_compilation_does_not_report_completion(self):
        with patch.object(provider, "progress") as progress, \
             patch.object(provider, "az", side_effect=provider.ProviderError("azure-command", "compile failed")):
            with self.assertRaises(provider.ProviderError):
                provider.compile_template("main.bicep", Path("main.json"))
        progress.assert_called_once_with("Compiling Azure infrastructure template main.bicep...")

    def setUp(self):
        self.parameters = {
            "installationId": OWNER, "tenantId": "tenant", "subscriptionId": "subscription",
            "clusterName": "test", "resourceGroupName": "shared", "eventingResourceGroup": "dedicated",
            "eventingEnabled": False, "nodeCount": 1,
        }
        self.auth = {
            "tenantId": "tenant", "subscriptionId": "subscription",
            "identity": "test-identity", "identityType": "user",
        }
        self.saved = {
            "installationId": OWNER, "parameters": copy.deepcopy(self.parameters),
            "auth": self.auth, "phase": "ready", "deployment": "owned-deployment",
        }
        self.request = {
            "protocolVersion": 1, "operation": "apply", "installationId": OWNER,
            "config": {
                "name": "test",
                "interactive": False,
                "nodeSetupImage": "registry/node-setup@sha256:" + "c" * 64,
            },
        }

    def test_unknown_configuration_keys_fail_before_lifecycle_operations(self):
        for operation, handler in (("init", "initialize"), ("apply", "apply"), ("inspect", "inspect"),
                                   ("connect", "connect"), ("delete", "delete")):
            for key in ("nodeCout", "eventingEnable"):
                request = copy.deepcopy(self.request)
                request.update(operation=operation)
                request["config"][key] = True
                with self.subTest(operation=operation, key=key), patch.object(provider, handler) as run:
                    result = provider.invoke(request)
                    self.assertEqual(result["status"], "error")
                    self.assertEqual(result["code"], "invalid-config")
                    self.assertIn(key, result["message"])
                    run.assert_not_called()

    def test_azure_rejects_unsupported_https_ports_before_lifecycle_operations(self):
        self.request["config"]["httpsPort"] = 8443
        with patch.object(provider, "apply") as apply:
            result = provider.invoke(self.request)
        self.assertEqual(result["code"], "invalid-config")
        self.assertIn("only httpsPort 443", result["message"])
        apply.assert_not_called()

    def test_ordered_event_grid_image_setting_is_rejected(self):
        self.request["config"]["eventGridImages"] = [
            "registry/controller@sha256:" + "a" * 64,
            "registry/adapter@sha256:" + "b" * 64,
        ]
        with patch.object(provider, "apply") as apply:
            result = provider.invoke(self.request)
        self.assertEqual(result["code"], "invalid-config")
        self.assertIn("eventGridImages", result["message"])
        apply.assert_not_called()

    def test_supported_provider_and_shared_configuration_survives_all_operations(self):
        self.request["config"].update({
            "context": "customer", "deleteCluster": False, "images": ["registry/home:v1"],
            "fiberdImage": "registry/home:v1", "routerImage": "registry/router:v1",
            "domain": "apps.example.com", "httpsPort": 443, "eventing": True,
            "subscriptionId": PRINCIPAL,
            "tenantId": PRINCIPAL, "resourceGroupName": "shared", "eventingResourceGroup": "dedicated",
            "location": "westus3", "nodeVmSize": "Standard_D4s_v5", "nodeCount": 1,
            "kubernetesVersion": "1.35.8", "authenticationMode": "deviceCode",
            "nodeSetupImage": "registry/node-setup@sha256:" + "c" * 64,
            "eventGridControllerImage": "registry/controller@sha256:" + "a" * 64,
            "eventGridAdapterImage": "registry/adapter@sha256:" + "b" * 64,
            "eventSources": {
                "blob-created": {
                    "storageAccount": "images",
                    "container": "uploads",
                    "eventTypes": ["Microsoft.Storage.BlobCreated"],
                },
            },
            "teardown": False,
        })
        for operation, handler in (("init", "initialize"), ("apply", "apply"), ("inspect", "inspect"),
                                   ("connect", "connect"), ("delete", "delete")):
            self.request["operation"] = operation
            with self.subTest(operation=operation), patch.object(provider, handler, return_value={}) as run:
                self.assertEqual(provider.invoke(self.request)["status"], "ok")
                run.assert_called_once_with(self.request)

    def test_missing_parameter_file_returns_a_provider_failure(self):
        self.request["operation"] = "inspect"
        with patch.object(provider, "WORKSPACE", Path(__file__).parent), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "authenticate") as auth:
            result = provider.invoke(self.request)
        self.assertEqual(result["protocolVersion"], 1)
        self.assertEqual(result["status"], "error")
        self.assertEqual(result["code"], "provider-failed")
        self.assertIn(provider.PARAMETERS, result["message"])
        auth.assert_not_called()

    def test_os_errors_and_azure_timeouts_return_provider_failures(self):
        self.request["operation"] = "inspect"
        for error in (PermissionError("Azure cache is unreadable"),
                      subprocess.TimeoutExpired(["/bin/az", "account", "show"], 120)):
            with self.subTest(error=error), patch.object(provider, "state", return_value=self.saved), \
                 patch.object(provider, "load_parameters", return_value=self.parameters), \
                 patch.object(provider, "bounded_run", side_effect=error):
                result = provider.invoke(self.request)
                self.assertEqual(result["status"], "error")
                self.assertEqual(result["code"], "provider-failed")
                self.assertEqual(result["message"], str(error))

    def test_azure_output_limit_stops_the_process_before_full_capture(self):
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

    def test_malformed_parameters_remain_invalid_input(self):
        self.request["operation"] = "inspect"
        with patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "load_parameters", side_effect=json.JSONDecodeError("Invalid JSON", "", 0)):
            result = provider.invoke(self.request)
        self.assertEqual(result["code"], "invalid-input")
        self.assertIn("Invalid JSON", result["message"])

    def test_disabling_existing_eventing_explains_destructive_reprovisioning(self):
        self.saved["outputs"] = {"eventing": {"resourceGroup": "dedicated"}}
        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "runtime_images", return_value=[]) as runtime_images, \
             patch.object(provider, "wait_cluster") as wait, patch.object(provider, "az") as az, \
             patch.object(provider, "save") as save:
            with self.assertRaises(provider.ProviderError) as error:
                provider.apply_source(self.request, Path("."), Path("."), "digest")
        self.assertEqual(error.exception.code, "eventing-removal-required")
        message = str(error.exception)
        self.assertIn("--delete-cluster", message)
        self.assertIn("deletes AKS", message)
        self.assertIn("re-provisioning", message)
        self.assertIn("retaining the cluster does not clear", message)
        runtime_images.assert_not_called()
        wait.assert_not_called()
        az.assert_not_called()
        save.assert_not_called()

    def test_source_operations_are_driven_by_customer_owned_azure_configuration(self):
        self.saved["parameters"].update(
            subscriptionId=PRINCIPAL,
            resourceGroupName="shared",
            eventingEnabled=True,
        )
        request = {
            "protocolVersion": 1,
            "operation": "source-create",
            "installationId": OWNER,
            "config": {
                "name": "blob-created",
                "namespace": "default",
                "service": "blob-handler",
                "observations": [],
            },
        }
        sources = {
            "blob-created": {
                "storageAccount": "images",
                "container": "uploads",
                "eventTypes": ["Microsoft.Storage.BlobCreated"],
            },
        }
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, provider.EVENT_SOURCES).write_text(json.dumps(sources))
            with patch.object(provider, "WORKSPACE", Path(directory)), \
                 patch.object(provider, "state", return_value=self.saved):
                result = provider.invoke(request)
        self.assertEqual(result["status"], "ok")
        plan = result["result"]
        self.assertEqual([item["kind"] for item in plan["sharedManifests"]], ["Broker"])
        self.assertEqual([item["kind"] for item in plan["manifests"]], ["Trigger", "AzureEventGridSource"])
        source = plan["manifests"][1]
        self.assertEqual(
            source["spec"]["sourceResourceID"],
            f"/subscriptions/{PRINCIPAL}/resourceGroups/shared/providers/Microsoft.Storage/storageAccounts/images",
        )
        self.assertEqual(
            source["spec"]["eventSubscription"]["subjectBeginsWith"],
            "/blobServices/default/containers/uploads/",
        )

        observations = []
        for identity in plan["observations"]:
            observations.append({
                "identity": identity,
                "object": {
                    **copy.deepcopy(identity),
                    "metadata": {**identity["metadata"], "generation": 1},
                    "status": {
                        "observedGeneration": 1,
                        "conditions": [{
                            "type": "Ready",
                            "status": "True",
                            "observedGeneration": 1,
                            "reason": "Ready",
                            "message": "ready",
                        }],
                    },
                },
            })
        request["operation"] = "source-status"
        request["config"]["observations"] = observations
        self.assertTrue(provider.invoke(request)["result"]["ready"])
        request["operation"] = "source-delete"
        deletion = provider.invoke(request)["result"]["manifests"]
        self.assertEqual([item["kind"] for item in deletion], ["AzureEventGridSource", "Trigger"])

    def test_source_creation_requires_eventing_and_a_named_workspace_definition(self):
        request = {
            "protocolVersion": 1,
            "operation": "source-create",
            "installationId": OWNER,
            "config": {
                "name": "missing",
                "namespace": "default",
                "service": "blob-handler",
                "observations": [],
            },
        }
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(provider, "WORKSPACE", Path(directory)), \
             patch.object(provider, "state", return_value=self.saved):
            Path(directory, provider.EVENT_SOURCES).write_text("{}")
            result = provider.invoke(request)
            self.assertEqual(result["code"], "eventing-required")
            self.saved["parameters"]["eventingEnabled"] = True
            result = provider.invoke(request)
            self.assertEqual(result["code"], "unknown-source")

    def test_event_source_edits_do_not_redeploy_cluster_infrastructure(self):
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(provider, "WORKSPACE", Path(directory)):
            Path(directory, "main.bicep").write_text("resource cluster 'Example@v1' = {}")
            sources = Path(directory, provider.EVENT_SOURCES)
            sources.write_text('{"one":{"storageAccount":"account"}}')
            first = provider.source_digest()
            sources.write_text('{"two":{"storageAccount":"another"}}')
            self.assertEqual(provider.source_digest(), first)

    def test_missing_resources_accept_structured_not_found_but_not_authorization_failures(self):
        for code in ("NotFound", "ResourceNotFound", "ResourceGroupNotFound", "DeploymentNotFound"):
            with self.subTest(code=code), patch.object(provider, "run", return_value=subprocess.CompletedProcess(
                [], 1, "", f"ERROR: ({code}) Resource is missing",
            )):
                self.assertIsNone(provider.az("aks", "show", missing=True))
                with self.assertRaises(provider.ProviderError):
                    provider.az("aks", "show")
        for message in ("ERROR: (AuthorizationFailed) NotFound", "ERROR: connection failed: NotFound"):
            with self.subTest(message=message), patch.object(provider, "run", return_value=subprocess.CompletedProcess([], 1, "", message)):
                with self.assertRaises(provider.ProviderError):
                    provider.az("aks", "show", missing=True)

    def test_unattended_authentication_does_not_prompt_or_treat_permission_errors_as_login(self):
        for message, code in [
            ("ERROR: Please run 'az login' to setup account.", "authentication-required"),
            ("ERROR: (AuthorizationFailed) Access denied", "authentication-failed"),
            ("ERROR: connection timed out", "authentication-failed"),
        ]:
            with self.subTest(message=message), patch.object(provider, "run", return_value=subprocess.CompletedProcess([], 1, "", message)):
                with self.assertRaises(provider.ProviderError) as error:
                    provider.authenticate(self.request)
                self.assertEqual(error.exception.code, code)

    def test_identity_changes_are_rejected_before_requesting_a_token(self):
        account = {
            "id": "subscription", "tenantId": "tenant", "user": {"name": "another-identity", "type": "user"},
            "environmentName": "AzureCloud", "state": "Enabled",
        }
        with patch.object(provider, "run", return_value=subprocess.CompletedProcess([], 0, json.dumps(account), "")) as command:
            with self.assertRaises(provider.ProviderError) as error:
                provider.authenticate(self.request, self.auth)
            self.assertEqual(error.exception.code, "identity-mismatch")
            self.assertEqual(command.call_count, 1)

    def test_login_is_attempted_once_not_repeated_for_an_unrecoverable_session(self):
        request = copy.deepcopy(self.request)
        request["config"]["interactive"] = True
        failure = subprocess.CompletedProcess([], 1, "", "Please run 'az login'")
        with patch.object(provider, "run", return_value=failure), patch.object(provider, "browser_login") as login:
            with self.assertRaises(provider.ProviderError) as error:
                provider.authenticate(request)
            self.assertEqual(error.exception.code, "authentication-failed")
            login.assert_called_once()

    def test_authentication_mode_selects_device_code_login(self):
        config = {"interactive": True, "authenticationMode": "deviceCode"}
        with patch.object(provider, "device_code_login") as device, \
             patch.object(provider, "browser_login") as browser:
            provider.interactive_login(config, "tenant")
        device.assert_called_once_with("tenant", True)
        browser.assert_not_called()

    def test_invalid_authentication_mode_is_rejected(self):
        self.request["config"]["authenticationMode"] = "automatic"
        result = provider.invoke(self.request)
        self.assertEqual(result["code"], "invalid-config")
        self.assertIn("authenticationMode", result["message"])

    def test_saved_authentication_mode_is_restored_for_reauthentication(self):
        request = copy.deepcopy(self.request)
        request["config"]["interactive"] = True
        expected = dict(self.auth, authenticationMode="deviceCode")
        failure = subprocess.CompletedProcess([], 1, "", "Please run 'az login'")
        with patch.object(provider, "run", return_value=failure), \
             patch.object(provider, "device_code_login") as device, \
             patch.object(provider, "browser_login") as browser:
            with self.assertRaises(provider.ProviderError) as error:
                provider.authenticate(request, expected)
        self.assertEqual(error.exception.code, "authentication-failed")
        device.assert_called_once_with("tenant", True)
        browser.assert_not_called()

    def test_browser_login_prints_url_without_device_code_or_subscription_prompt(self):
        with patch.object(provider.subprocess, "Popen") as launch, \
             patch.object(provider, "stream_login", return_value=0), \
             patch.object(provider, "progress"):
            provider.browser_login("tenant", True)
        args = launch.call_args.args[0]
        self.assertNotIn("--use-device-code", args)
        self.assertEqual(args[-2:], ["--tenant", "tenant"])
        options = launch.call_args.kwargs
        self.assertEqual(options["stdout"], subprocess.PIPE)
        self.assertEqual(options["stderr"], subprocess.STDOUT)
        self.assertEqual(options["env"]["AZURE_CORE_LOGIN_EXPERIENCE_V2"], "off")
        url = "https://login.microsoftonline.com/tenant/oauth2/v2.0/authorize?state=test&redirect_uri=http://localhost:12345"
        result = subprocess.run(
            [sys.executable, "-c", "import sys, webbrowser; assert webbrowser.get().open(sys.argv[1])", url],
            env=options["env"], capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), url)

    def test_device_code_login_uses_azure_cli_device_flow(self):
        with patch.object(provider.subprocess, "Popen") as launch, \
             patch.object(provider, "stream_login", return_value=0), \
             patch.object(provider, "progress"):
            provider.device_code_login("tenant", True)
        args = launch.call_args.args[0]
        self.assertIn("--use-device-code", args)
        self.assertEqual(args[-2:], ["--tenant", "tenant"])
        options = launch.call_args.kwargs
        self.assertEqual(options["stdout"], subprocess.PIPE)
        self.assertEqual(options["stderr"], subprocess.STDOUT)

    def test_browser_login_streams_stdout_and_stderr(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import sys; print('sign-in URL', flush=True); print('login warning', file=sys.stderr, flush=True)"],
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        with patch.object(provider, "progress") as progress:
            self.assertEqual(provider.stream_login(process, 10), 0)
        self.assertEqual("\n".join(call.args[0] for call in progress.call_args_list), "sign-in URL\nlogin warning")

    def test_browser_login_streaming_has_a_real_deadline_and_reaps_the_child(self):
        process = subprocess.Popen(
            [sys.executable, "-c", "import time; print('sign-in URL', flush=True); time.sleep(60)"],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        with patch.object(provider, "progress") as progress:
            with self.assertRaises(provider.ProviderError) as error:
                provider.stream_login(process, 0.1)
        self.assertEqual(error.exception.code, "authentication-timeout")
        self.assertIsNotNone(process.poll())
        progress.assert_called_once_with("sign-in URL")

    def test_identity_guid_case_does_not_retarget_the_installation(self):
        parameters = copy.deepcopy(self.parameters)
        parameters["subscriptionId"] = "SUBSCRIPTION"
        parameters["tenantId"] = "TENANT"
        request = copy.deepcopy(self.request)
        request["config"].update(subscriptionId="SUBSCRIPTION", tenantId="TENANT")
        with patch.object(provider, "load_parameters", return_value=parameters), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "authenticate", return_value=self.auth):
            loaded, _ = provider.settings(request)
        self.assertEqual(loaded, parameters)

    def test_authentication_mode_override_is_saved_for_later_operations(self):
        self.request["config"]["authenticationMode"] = "deviceCode"
        identity = dict(self.auth, authenticationMode="deviceCode")
        with patch.object(provider, "load_parameters", return_value=self.parameters), \
             patch.object(provider, "state", return_value=self.saved), \
             patch.object(provider, "authenticate", return_value=identity), \
             patch.object(provider, "save") as save:
            _, saved = provider.settings(self.request)
        self.assertEqual(saved["auth"], identity)
        save.assert_called_once_with(saved)

    def test_cli_defaults_cannot_overwrite_edited_source_or_retarget_an_installation(self):
        self.request["config"]["nodeCount"] = 3
        with patch.object(provider, "load_parameters", return_value=self.parameters), patch.object(provider, "state", return_value=self.saved), patch.object(provider, "authenticate", return_value=self.auth) as auth:
            with self.assertRaises(provider.ProviderError) as error:
                provider.settings(self.request)
            self.assertEqual(error.exception.code, "source-conflict")
            auth.assert_not_called()
            self.request["config"].pop("nodeCount")
            self.parameters["resourceGroupName"] = "another-group"
            with self.assertRaises(provider.ProviderError) as error:
                provider.settings(self.request)
            self.assertEqual(error.exception.code, "identity-mismatch")

    def test_completed_remote_work_resumes_without_submitting_another_deployment(self):
        self.saved["phase"] = "deploying"
        remote = {"properties": {
            "provisioningState": "Succeeded", "parameters": {"lampyrSourceDigest": {"value": "digest"}},
        }}
        result = {name: self.parameters[name] for name in ("installationId", "tenantId", "subscriptionId", "clusterName")}
        result.update(
            resourceGroup="shared", sourceDigest="digest",
            clusterId=provider.resource_id(self.parameters, "providers", "Microsoft.ContainerService", "managedClusters", "test"),
        )
        current = {"tags": {provider.OWNER_TAG: OWNER}, "provisioningState": "Succeeded"}
        with patch.object(provider, "runtime_manifests", return_value={}) as runtime, patch.object(provider, "settings", return_value=(self.parameters, self.saved)), patch.object(provider, "source_digest", return_value="digest"), patch.object(provider, "cluster", return_value=current), patch.object(provider, "deployment", return_value=remote), patch.object(provider, "wait_deployment", return_value=result), patch.object(provider, "save") as save, patch.object(provider, "az") as az:
            self.assertEqual(provider.apply(self.request), result)
            self.assertEqual(save.call_args.args[0]["phase"], "ready")
            runtime.assert_called_once_with(self.request, self.parameters, result, {
                "nodeSetupImage": self.request["config"]["nodeSetupImage"],
            })
            az.assert_not_called()

    def test_cluster_operations_finish_before_reconciliation_and_output_validation(self):
        self.saved["phase"] = "deploying"
        remote = {"properties": {
            "provisioningState": "Succeeded", "parameters": {"lampyrSourceDigest": {"value": "digest"}},
        }}
        result = {name: self.parameters[name] for name in ("installationId", "tenantId", "subscriptionId", "clusterName")}
        result.update(
            resourceGroup="shared", sourceDigest="digest",
            clusterId=provider.resource_id(self.parameters, "providers", "Microsoft.ContainerService", "managedClusters", "test"),
        )
        updating = {"tags": {provider.OWNER_TAG: OWNER}, "provisioningState": "Updating"}
        ready = dict(updating, provisioningState="Succeeded")
        with patch.object(provider, "runtime_manifests", return_value={}), patch.object(provider, "settings", return_value=(self.parameters, self.saved)), patch.object(provider, "source_digest", return_value="digest"), patch.object(provider, "cluster", side_effect=[updating, ready, updating, ready]), patch.object(provider, "deployment", return_value=remote), patch.object(provider, "wait_deployment", return_value=result), patch.object(provider, "save"), patch.object(provider, "progress"), patch.object(provider.time, "sleep") as sleep, patch.object(provider, "az") as az:
            self.assertEqual(provider.apply(self.request), result)
            self.assertEqual(sleep.call_count, 2)
            az.assert_not_called()

    def test_waiting_for_cluster_operations_never_adopts_a_foreign_or_deleting_cluster(self):
        updating = {"tags": {provider.OWNER_TAG: OWNER}, "provisioningState": "Updating"}
        for current, code in [
            (dict(updating, tags={provider.OWNER_TAG: "other"}), "not-owned"),
            (dict(updating, provisioningState="Deleting"), "deleting"),
        ]:
            with self.subTest(code=code), patch.object(provider, "cluster", side_effect=[updating, current]), patch.object(provider, "progress"), patch.object(provider.time, "sleep"):
                with self.assertRaises(provider.ProviderError) as error:
                    provider.wait_cluster(self.parameters, self.request)
                self.assertEqual(error.exception.code, code)

    def test_aks_preflight_update_race_retries_only_the_specific_owned_operation(self):
        busy = provider.ProviderError(
            "azure-command", "OperationNotAllowed: in-progress update managed cluster operation",
        )
        ready = {"tags": {provider.OWNER_TAG: OWNER}, "provisioningState": "Succeeded"}
        for failure, retried in [
            (busy, True),
            (provider.ProviderError("azure-command", "OperationNotAllowed: access denied"), False),
            (provider.ProviderError("azure-command", "AuthorizationFailed"), False),
        ]:
            with self.subTest(error=str(failure)), \
                 patch.object(provider, "runtime_manifests", return_value={}), \
                 patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
                 patch.object(provider, "cluster", return_value=ready), \
                 patch.object(provider, "deployment", return_value=None), \
                 patch.object(provider, "compile_template"), patch.object(provider, "ensure_group"), \
                 patch.object(provider, "save"), patch.object(provider, "wait_deployment", return_value={}), \
                 patch.object(provider, "validate_outputs", return_value={}), patch.object(provider, "progress"), \
                 patch.object(provider.time, "sleep") as sleep, \
                 patch.object(provider, "az", side_effect=[failure, None]) as azure:
                if retried:
                    self.assertEqual(provider.apply_source(self.request, Path("."), Path("."), "digest"), {})
                    self.assertEqual(azure.call_count, 2)
                    sleep.assert_called_once_with(5)
                else:
                    with self.assertRaises(provider.ProviderError):
                        provider.apply_source(self.request, Path("."), Path("."), "digest")
                    self.assertEqual(azure.call_count, 1)
                    sleep.assert_not_called()

    def test_aks_preflight_retry_rechecks_ownership_before_resubmission(self):
        ready = {"tags": {provider.OWNER_TAG: OWNER}, "provisioningState": "Succeeded"}
        foreign = dict(ready, tags={provider.OWNER_TAG: "other"})
        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "cluster", side_effect=[ready, foreign]), \
             patch.object(provider, "deployment", return_value=None), \
             patch.object(provider, "compile_template"), patch.object(provider, "ensure_group"), \
             patch.object(provider, "save"), patch.object(provider, "az", side_effect=provider.ProviderError(
                 "azure-command", "OperationNotAllowed: in-progress update managed cluster operation",
             )) as azure:
            with self.assertRaises(provider.ProviderError) as error:
                provider.apply_source(self.request, Path("."), Path("."), "digest")
            self.assertEqual(error.exception.code, "not-owned")
            self.assertEqual(azure.call_count, 1)

    def test_running_deployment_cannot_be_replaced_with_different_source(self):
        self.saved["phase"] = "deploying"
        remote = {"properties": {
            "provisioningState": "Running", "parameters": {"lampyrSourceDigest": {"value": "old-digest"}},
        }}
        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), patch.object(provider, "source_digest", return_value="new-digest"), patch.object(provider, "cluster", return_value=None), patch.object(provider, "deployment", return_value=remote), patch.object(provider, "az") as az:
            with self.assertRaises(provider.ProviderError) as error:
                provider.apply(self.request)
            self.assertEqual(error.exception.code, "operation-conflict")
            az.assert_not_called()

    def test_terminal_deployment_is_resubmitted_for_the_same_source(self):
        self.saved["phase"] = "deploying"
        remote = {"properties": {
            "provisioningState": "Failed",
            "parameters": {"lampyrSourceDigest": {"value": "digest"}},
        }}
        with patch.object(provider, "runtime_manifests", return_value={}), \
             patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "cluster", return_value=None), \
             patch.object(provider, "deployment", return_value=remote), \
             patch.object(provider, "compile_template"), patch.object(provider, "ensure_group"), \
             patch.object(provider, "save"), patch.object(provider, "wait_deployment", return_value={}), \
             patch.object(provider, "validate_outputs", return_value={}), \
             patch.object(provider, "az") as azure:
            self.assertEqual(provider.apply_source(self.request, Path("."), Path("."), "digest"), {})
        self.assertTrue(any(call.args[:3] == ("deployment", "group", "create")
                            for call in azure.call_args_list))

    def test_successful_reinstall_persists_newly_enabled_eventing_state(self):
        parameters = copy.deepcopy(self.parameters)
        parameters["eventingEnabled"] = True
        self.saved["parameters"]["eventingEnabled"] = False
        self.saved["phase"] = "deploying"
        remote = {
            "properties": {
                "provisioningState": "Succeeded",
                "parameters": {
                    "lampyrSourceDigest": {"value": "digest"},
                },
            },
        }
        with patch.object(provider, "settings", return_value=(parameters, self.saved)), \
             patch.object(provider, "runtime_images", return_value=[]), \
             patch.object(provider, "wait_cluster"), \
             patch.object(provider, "group", return_value={
                 "tags": {provider.OWNER_TAG: OWNER},
                 "properties": {"provisioningState": "Succeeded"},
             }), \
             patch.object(provider, "deployment", return_value=remote), \
             patch.object(provider, "wait_deployment", return_value={}), \
             patch.object(provider, "validate_outputs", return_value={}), \
             patch.object(provider, "runtime_manifests", return_value={}), \
             patch.object(provider, "save"):
            provider.apply_source(
                self.request,
                Path("."),
                Path("."),
                "digest",
            )
        self.assertTrue(self.saved["parameters"]["eventingEnabled"])

    def test_teardown_retains_shared_resources_and_removes_only_its_principals_roles(self):
        self.saved["outputs"] = {"eventing": {"resourceGroup": "dedicated"}}
        scope = provider.resource_id(self.parameters)
        role = scope + "/providers/Microsoft.Authorization/roleAssignments/" + PRINCIPAL
        owned = {"properties": {"provisioningState": "Succeeded"}, "tags": {provider.OWNER_TAG: OWNER}}

        def azure(*args, **kwargs):
            if args[:2] == ("identity", "list"):
                return [{"principalId": PRINCIPAL, "tags": {provider.OWNER_TAG: OWNER}}]
            if args[:3] == ("role", "assignment", "list"):
                return [
                    {"id": role, "scope": scope, "principalId": PRINCIPAL},
                    {"id": "unrelated", "scope": scope, "principalId": "another-principal"},
                    {"id": "inherited", "scope": "/subscriptions/subscription", "principalId": PRINCIPAL},
                ]
            return None

        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "deployment", return_value=None), \
             patch.object(provider, "cluster", return_value=owned), \
             patch.object(provider, "group", side_effect=[owned, None]), \
             patch.object(provider, "save"), patch.object(provider, "az", side_effect=azure) as az:
            self.assertTrue(provider.delete(self.request)["clusterRetained"])
            self.assertEqual(self.saved["phase"], "retained")
            self.assertIn("eventing", self.saved["outputs"])
            deletions = [call.args for call in az.call_args_list if "delete" in call.args]
            self.assertEqual(len(deletions), 2)
            self.assertEqual(deletions[0], ("role", "assignment", "delete", "--subscription", "subscription", "--ids", role))
            self.assertIn("dedicated", deletions[1])
            self.assertNotIn("shared", deletions[1])

    def test_teardown_deletes_saved_roles_after_eventing_resources_are_absent(self):
        scope = provider.resource_id(self.parameters)
        role = scope + "/providers/Microsoft.Authorization/roleAssignments/" + PRINCIPAL
        self.saved["outputs"] = {
            "eventing": {"publisherRoleAssignments": [role]},
        }
        owned = {
            "properties": {"provisioningState": "Succeeded"},
            "tags": {provider.OWNER_TAG: OWNER},
        }
        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "deployment", return_value=None), \
             patch.object(provider, "cluster", return_value=owned), \
             patch.object(provider, "group", return_value=None), \
             patch.object(provider, "save"), \
             patch.object(provider, "az", return_value=None) as azure:
            self.assertTrue(provider.delete(self.request)["clusterRetained"])
        azure.assert_called_once_with(
            "role", "assignment", "delete",
            "--subscription", "subscription", "--ids", role,
        )
        self.assertEqual(self.saved["managedRoleAssignments"], [])

    def test_full_delete_waits_for_aks_before_deleting_eventing_group(self):
        self.request["config"]["deleteCluster"] = True
        owned_cluster = {
            "id": provider.resource_id(
                self.parameters,
                "providers", "Microsoft.ContainerService", "managedClusters", "test",
            ),
            "provisioningState": "Succeeded",
            "tags": {provider.OWNER_TAG: OWNER},
        }
        owned_group = {
            "properties": {"provisioningState": "Succeeded"},
            "tags": {provider.OWNER_TAG: OWNER},
        }
        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "deployment", return_value=None), \
             patch.object(provider, "cluster", side_effect=[owned_cluster, None]), \
             patch.object(provider, "group", side_effect=[owned_group, None]), \
             patch.object(provider, "save"), \
             patch.object(provider, "az", return_value=[]) as azure:
            self.assertFalse(provider.delete(self.request)["clusterRetained"])
        destructive = [
            call.args for call in azure.call_args_list
            if call.args[:2] in (("aks", "delete"), ("group", "delete"))
        ]
        self.assertEqual(destructive[0][:2], ("aks", "delete"))
        self.assertEqual(destructive[1][:2], ("group", "delete"))

    def test_deletion_surfaces_terminal_azure_failure(self):
        resource = {
            "properties": {
                "provisioningState": "Failed",
                "error": {"code": "PublicIPAddressCannotBeDeleted"},
            },
            "tags": {provider.OWNER_TAG: OWNER},
        }
        with self.assertRaises(provider.ProviderError) as error:
            provider.wait_deleted(
                lambda: resource,
                self.request,
                "Eventing resource group",
                60,
                lambda value: value["properties"]["provisioningState"],
            )
        self.assertEqual(error.exception.code, "deletion-failed")
        self.assertIn("PublicIPAddressCannotBeDeleted", str(error.exception))

    def test_foreign_cluster_prevents_any_destructive_cloud_operation(self):
        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "deployment", return_value=None), \
             patch.object(provider, "cluster", return_value={"tags": {provider.OWNER_TAG: "other"}}), \
             patch.object(provider, "az") as az:
            with self.assertRaises(provider.ProviderError) as error:
                provider.delete(self.request)
            self.assertEqual(error.exception.code, "not-owned")
            az.assert_not_called()

    def test_delete_rejects_a_running_deployment_before_inventory(self):
        remote = {"properties": {"provisioningState": "Running"}}
        with patch.object(provider, "settings", return_value=(self.parameters, self.saved)), \
             patch.object(provider, "deployment", return_value=remote), \
             patch.object(provider, "cluster") as cluster, patch.object(provider, "az") as azure:
            with self.assertRaises(provider.ProviderError) as error:
                provider.delete(self.request)
        self.assertEqual(error.exception.code, "operation-conflict")
        cluster.assert_not_called()
        azure.assert_not_called()

    def test_changed_customer_source_changes_the_deployment_binding_without_rewriting_files(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(provider, "WORKSPACE", Path(directory)):
            source = Path(directory) / "main.bicep"
            source.write_text("param name string = 'first'\n")
            first = provider.source_digest()
            source.write_text("param name string = 'edited'\n")
            edited = source.read_bytes()
            self.assertNotEqual(first, provider.source_digest())
            self.assertEqual(source.read_bytes(), edited)
            (Path(directory) / "escape.bicep").symlink_to(source)
            with self.assertRaises(provider.ProviderError):
                provider.source_digest()

    def test_snapshot_binds_non_bicep_inputs_and_is_unchanged_by_later_edits(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory) / "workspace"
            workspace.mkdir()
            settings = workspace / "settings.txt"
            settings.write_text("first")
            snapshot = Path(directory) / "snapshot"
            with patch.object(provider, "WORKSPACE", workspace):
                first = provider.source_digest(snapshot)
                settings.write_text("edited")
                self.assertNotEqual(first, provider.source_digest())
            self.assertEqual((snapshot / "settings.txt").read_text(), "first")

    def test_teardown_uses_recorded_identity_even_if_customer_source_is_broken(self):
        with patch.object(provider, "state", return_value=self.saved), patch.object(provider, "load_parameters", side_effect=ValueError("broken source")) as source, patch.object(provider, "authenticate"):
            parameters, _ = provider.settings(self.request, teardown=True)
            self.assertEqual(parameters["resourceGroupName"], "shared")
            source.assert_not_called()

    def test_latest_version_understands_minor_groups_and_rejects_preview_patches(self):
        versions = {"values": [
            {"version": "1.33.9"},
            {"version": "1.34", "patchVersions": {"1.34.2": {}, "1.34.10": {}, "1.34.11": {"isPreview": True}}},
            {"version": "1.35", "isPreview": True, "patchVersions": {"1.35.1": {}}},
        ]}
        self.assertEqual(provider.latest_version(versions), "1.34.10")

    def test_initialization_retry_keeps_existing_customer_source_and_authentication_binding(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(provider, "WORKSPACE", Path(directory)):
            parameters = Path(directory) / provider.PARAMETERS
            parameters.write_text("customer-edited content")
            with patch.object(provider, "state", return_value=self.saved), \
                 patch.object(provider, "settings", return_value=(self.parameters, self.saved)) as settings, \
                 patch.object(provider, "authenticate") as auth:
                self.assertEqual(provider.initialize(self.request), {"workspace": "main.bicep"})
                settings.assert_called_once_with(self.request)
                auth.assert_not_called()
                self.assertEqual(parameters.read_text(), "customer-edited content")
            parameters.unlink()
            (Path(directory) / "main.bicep").write_text("partially created source")
            with patch.object(provider, "state", return_value=self.saved), patch.object(provider, "authenticate") as auth:
                with self.assertRaises(provider.ProviderError) as error:
                    provider.initialize(self.request)
                self.assertEqual(error.exception.code, "incomplete-source")
                auth.assert_not_called()

    def test_reinstall_rejects_eventing_and_source_changes_ignored_by_workspace(self):
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory)
            (workspace / provider.PARAMETERS).write_text("{}")
            (workspace / provider.EVENT_SOURCES).write_text("{}")
            request = copy.deepcopy(self.request)
            request["config"]["eventing"] = True
            with patch.object(provider, "WORKSPACE", workspace), \
                 patch.object(provider, "state", return_value=self.saved), \
                 patch.object(provider, "settings", return_value=(self.parameters, self.saved)):
                with self.assertRaises(provider.ProviderError) as error:
                    provider.initialize(request)
            self.assertEqual(error.exception.code, "configuration-mismatch")
            self.assertIn("main.parameters.json", str(error.exception))

            request["config"]["eventing"] = False
            request["config"]["eventSources"] = {
                "blob": {"storageAccount": "images"},
            }
            with patch.object(provider, "WORKSPACE", workspace), \
                 patch.object(provider, "state", return_value=self.saved), \
                 patch.object(provider, "settings", return_value=(self.parameters, self.saved)):
                with self.assertRaises(provider.ProviderError) as error:
                    provider.initialize(request)
            self.assertEqual(error.exception.code, "configuration-mismatch")
            self.assertIn(provider.EVENT_SOURCES, str(error.exception))

    def test_teardown_inspection_does_not_require_valid_customer_source(self):
        self.request["config"]["teardown"] = True
        with patch.object(provider, "state", return_value=self.saved), patch.object(provider, "load_parameters", side_effect=ValueError("broken source")) as source, patch.object(provider, "authenticate"), patch.object(provider, "cluster", return_value=None), patch.object(provider, "deployment", return_value=None):
            result = provider.inspect(self.request)
            self.assertFalse(result["exists"])
            self.assertEqual(result["scope"]["resourceGroup"], "shared")
            source.assert_not_called()

    def test_aad_kubernetes_credentials_are_rejected_without_host_exec_plugins(self):
        config = {"users": [{"user": {"auth-provider": {"name": "azure", "config": {
            "tenant-id": "tenant", "apiserver-id": "unexpected-audience",
        }}}}]}
        with patch.object(provider, "az") as az:
            with self.assertRaises(provider.ProviderError) as error:
                provider.connection_credentials(config)
            self.assertEqual(error.exception.code, "invalid-connection")
            az.assert_not_called()

    def test_aks_certificate_credentials_remove_only_the_redundant_token(self):
        certificate = {"client-certificate-data": "Y2VydA==", "client-key-data": "a2V5"}
        with patch.object(provider, "az") as az:
            config = {"users": [{"user": dict(certificate, token="redundant")}]}
            provider.connection_credentials(config)
            self.assertEqual(config["users"][0]["user"], certificate)
            az.assert_not_called()
        config = {"users": [{"user": dict(certificate, **{"exec": {"command": "untrusted"}})}]}
        with self.assertRaises(provider.ProviderError):
            provider.connection_credentials(config)
        for missing in certificate:
            with self.subTest(missing=missing):
                user = dict(certificate, token="retained")
                del user[missing]
                config = {"users": [{"user": user.copy()}]}
                with self.assertRaises(provider.ProviderError):
                    provider.connection_credentials(config)
