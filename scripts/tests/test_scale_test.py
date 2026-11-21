import contextlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock


PATH = Path(__file__).resolve().parents[1] / "scale-test.py"
SPEC = importlib.util.spec_from_file_location("scale_test", PATH)
scale = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(scale)


class ScaleTestTests(unittest.TestCase):
    def main_arguments(self, *extra):
        return [
            str(PATH),
            "--context", "test",
            "--service", "demo",
            "--lampyr-url", "https://example.test/",
            "--baseline-image", "example.test/baseline@sha256:abc",
            "--oha", str(PATH),
            "--concurrency", "1",
            *extra,
        ]

    def test_duration_seconds(self):
        self.assertEqual(scale.duration_seconds("30s"), 30)
        self.assertEqual(scale.duration_seconds("2m"), 120)
        for invalid in ("0s", "1h", "ten"):
            with self.assertRaises(ValueError):
                scale.duration_seconds(invalid)

    def test_report_compares_lampyr_and_kubernetes(self):
        result = {
            "metadata": {
                "commit": "abc",
                "dirty": False,
                "context": "kind-test",
                "revision": "demo-00001",
                "duration": "30s",
                "baselineImage": "example.test/baseline@sha256:abc",
                "networkPaths": {
                    "lampyr": "configured ingress URL",
                    "kubernetes": "kubectl Service port-forward",
                },
            },
            "scenarios": [
                {
                    "target": "lampyr",
                    "concurrency": 4,
                    "client": {
                        "summary": {"requestsPerSec": 10},
                        "httpSuccessRate": 1,
                        "successfulLatencyMs": {
                            "p50": 1,
                            "p95": 2,
                            "p99": 3,
                        },
                    },
                    "platform": {
                        "lampyrErrors": 0,
                        "lampyrShed": 0,
                        "lampyrRouterCPU": 0.1,
                        "lampyrHomeCPU": 0.2,
                        "lampyrRouterMemoryBytes": 1 << 20,
                        "lampyrHomeMemoryBytes": 2 << 20,
                        "lampyrMemoryBytes": 2.5 * (1 << 20),
                        "baselineCPU": None,
                        "baselineMemoryBytes": None,
                    },
                },
                {
                    "target": "kubernetes",
                    "concurrency": 4,
                    "client": {
                        "summary": {"requestsPerSec": 20},
                        "httpSuccessRate": 1,
                        "successfulLatencyMs": {
                            "p50": 1,
                            "p95": 2,
                            "p99": 3,
                        },
                    },
                    "platform": {
                        "lampyrErrors": None,
                        "lampyrShed": None,
                        "lampyrRouterCPU": None,
                        "lampyrHomeCPU": None,
                        "lampyrRouterMemoryBytes": None,
                        "lampyrHomeMemoryBytes": None,
                        "baselineCPU": 0.4,
                        "baselineMemoryBytes": 4 << 20,
                    },
                },
            ],
        }
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.md"
            scale.write_report(result, output)
            report = output.read_text()
        self.assertIn("| lampyr | 4 | 10.00", report)
        self.assertIn("| kubernetes | 4 | 20.00", report)
        self.assertIn("| 0 | 0 | 0.300 | 2.50 |", report)
        self.assertIn("kubectl Service port-forward", report)

    def test_report_handles_no_successful_responses(self):
        result = {
            "metadata": {
                "commit": "abc",
                "dirty": True,
                "context": "kind-test",
                "revision": "demo-00001",
                "duration": "30s",
                "baselineImage": "example.test/baseline@sha256:abc",
                "networkPaths": {
                    "lampyr": "configured ingress URL",
                    "kubernetes": "kubectl Service port-forward",
                },
            },
            "scenarios": [
                {
                    "target": "lampyr",
                    "concurrency": 4,
                    "client": {
                        "summary": {"requestsPerSec": 10},
                        "httpSuccessRate": 0,
                        "successfulLatencyMs": {
                            "p50": None,
                            "p95": None,
                            "p99": None,
                        },
                    },
                    "platform": {
                        "lampyrErrors": 20,
                        "lampyrShed": 20,
                        "lampyrRouterCPU": None,
                        "lampyrHomeCPU": None,
                        "lampyrRouterMemoryBytes": None,
                        "lampyrHomeMemoryBytes": None,
                        "lampyrMemoryBytes": None,
                        "baselineCPU": None,
                        "baselineMemoryBytes": None,
                    },
                }
            ],
        }
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.md"
            scale.write_report(result, output)
            report = output.read_text()
        self.assertIn("| lampyr | 4 | 10.00 | 0.0000 | n/a | n/a | n/a | 20 | 20 |", report)

    def test_report_does_not_turn_missing_metrics_into_zero(self):
        result = {
            "metadata": {
                "commit": "abc",
                "dirty": False,
                "context": "kind-test",
                "revision": "demo-00001",
                "duration": "30s",
                "baselineImage": "example.test/baseline@sha256:abc",
                "networkPaths": {
                    "lampyr": "configured ingress URL",
                    "kubernetes": "kubectl Service port-forward",
                },
            },
            "scenarios": [
                {
                    "target": "kubernetes",
                    "concurrency": 4,
                    "client": {
                        "summary": {"requestsPerSec": 20},
                        "httpSuccessRate": 1,
                        "successfulLatencyMs": {
                            "p50": 1,
                            "p95": 2,
                            "p99": 3,
                        },
                    },
                    "platform": {
                        "lampyrErrors": 0,
                        "lampyrShed": 0,
                        "lampyrRouterCPU": 0.1,
                        "lampyrHomeCPU": 0.2,
                        "lampyrRouterMemoryBytes": 1 << 20,
                        "lampyrHomeMemoryBytes": 2 << 20,
                        "baselineCPU": None,
                        "baselineMemoryBytes": None,
                    },
                }
            ],
        }
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "report.md"
            scale.write_report(result, output)
            report = output.read_text()
        self.assertIn("| n/a | n/a | n/a | n/a |", report)

    def test_http_success_rate_includes_transport_errors(self):
        client = {
            "statusCodeDistribution": {"200": 3, "503": 1},
            "errorDistribution": {"connection refused": 2},
        }
        self.assertEqual(scale.http_success_rate(client), 0.5)

    def test_safe_url_removes_credentials_and_query(self):
        self.assertEqual(
            scale.safe_url("https://user:secret@example.test:8443/path?token=secret"),
            "https://example.test:8443/path",
        )

    def test_metric_queries_use_requested_namespace(self):
        queries = scale.metric_queries("team-a", "demo-00001", "30s", "baseline")
        for name in (
            "lampyrRequestP95Seconds",
            "lampyrAcquireP95Seconds",
            "lampyrPeakBusyFibers",
            "lampyrPeakInFlight",
            "baselineCPU",
            "baselineMemoryBytes",
        ):
            self.assertIn('namespace="team-a"', queries[name])
        self.assertIn(
            'label_serving_knative_dev_revision="demo-00001"',
            queries["lampyrHomeCPU"],
        )
        for name in (
            "lampyrRouterCPU",
            "lampyrRouterMemoryBytes",
            "lampyrHomeCPU",
            "lampyrHomeMemoryBytes",
            "baselineCPU",
            "baselineMemoryBytes",
        ):
            self.assertIn("kube_pod_status_phase", queries[name])

    def test_target_resources_require_cpu_and_memory(self):
        metrics = {
            "lampyrRouterCPU": 0.1,
            "lampyrHomeCPU": 0.2,
            "lampyrRouterMemoryBytes": 1,
            "lampyrHomeMemoryBytes": 2,
            "lampyrMemoryBytes": 3,
            "baselineCPU": None,
            "baselineMemoryBytes": None,
        }
        self.assertTrue(scale.target_resources_available("lampyr", metrics))
        self.assertFalse(scale.target_resources_available("kubernetes", metrics))
        metrics["lampyrMemoryBytes"] = None
        self.assertFalse(scale.target_resources_available("lampyr", metrics))

    def test_baseline_manifest_uses_restricted_pod_security(self):
        manifest = scale.baseline_manifest(
            "example.test/baseline@sha256:abc", 1, "team-a", "baseline-owned"
        )
        deployment = manifest["items"][0]
        pod = deployment["spec"]["template"]["spec"]
        container = pod["containers"][0]
        self.assertEqual(deployment["metadata"]["namespace"], "team-a")
        self.assertFalse(pod["automountServiceAccountToken"])
        self.assertEqual(pod["securityContext"]["seccompProfile"]["type"], "RuntimeDefault")
        self.assertTrue(container["securityContext"]["runAsNonRoot"])
        self.assertTrue(container["securityContext"]["readOnlyRootFilesystem"])

    def test_baseline_cleanup_runs_when_rollout_fails(self):
        image = "example.test/baseline@sha256:abc"
        rollout_error = scale.subprocess.CalledProcessError(1, ["kubectl"])
        with mock.patch.object(scale, "kubectl_manifest") as manifest:
            with mock.patch.object(
                scale.subprocess, "run", side_effect=rollout_error
            ):
                with self.assertRaises(scale.subprocess.CalledProcessError):
                    with scale.baseline_server("test", image, 1, "team-a"):
                        pass
        self.assertEqual(
            [call.args[1] for call in manifest.call_args_list],
            ["create", "create", "delete"],
        )

    def test_baseline_does_not_delete_resources_when_creation_fails(self):
        failure = scale.subprocess.CalledProcessError(1, ["kubectl"])
        with mock.patch.object(scale, "kubectl_manifest", side_effect=failure) as manifest:
            with self.assertRaises(scale.subprocess.CalledProcessError):
                with scale.baseline_server(
                    "test", "example.test/baseline@sha256:abc", 1, "team-a"
                ):
                    pass
        self.assertEqual(manifest.call_count, 1)

    def test_baseline_partial_creation_cleans_only_created_resources(self):
        failure = scale.subprocess.CalledProcessError(1, ["kubectl"])
        with mock.patch.object(
            scale, "kubectl_manifest", side_effect=[None, failure, None]
        ) as manifest, mock.patch.object(
            scale.subprocess, "run", side_effect=AssertionError("unexpected rollout")
        ):
            with self.assertRaises(scale.subprocess.CalledProcessError):
                with scale.baseline_server(
                    "test", "example.test/baseline@sha256:abc", 1, "team-a"
                ):
                    pass
        self.assertEqual(
            [call.args[1] for call in manifest.call_args_list],
            ["create", "create", "delete"],
        )
        cleanup = manifest.call_args.args[2]["items"]
        self.assertEqual(cleanup, [manifest.call_args_list[0].args[2]])
        self.assertEqual(cleanup[0]["kind"], "Deployment")

    def test_baseline_runs_have_distinct_names_and_selectors(self):
        failure = scale.subprocess.CalledProcessError(1, ["kubectl"])
        with mock.patch.object(scale, "kubectl_manifest") as manifest, mock.patch.object(
            scale.subprocess, "run", side_effect=failure
        ):
            for _ in range(2):
                with self.assertRaises(scale.subprocess.CalledProcessError):
                    with scale.baseline_server(
                        "test", "example.test/baseline@sha256:abc", 1, "team-a"
                    ):
                        pass
        deployments = []
        for call in manifest.call_args_list:
            if call.args[1] in ("apply", "create"):
                value = call.args[2]
                deployments.extend(
                    item for item in value.get("items", [value])
                    if item["kind"] == "Deployment"
                )
        self.assertEqual(len(deployments), 2)
        self.assertNotEqual(
            deployments[0]["metadata"]["name"], deployments[1]["metadata"]["name"]
        )
        self.assertNotEqual(
            deployments[0]["spec"]["selector"], deployments[1]["spec"]["selector"]
        )

    def test_counter_resets_are_rejected_when_aggregate_increases(self):
        queries = scale.metric_queries("team-a", "demo-00001", "30s", "baseline")
        before = {name: 100 for name in scale.COUNTER_METRICS}
        for name in scale.COUNTER_METRICS:
            series = queries[name].removeprefix("sum(").removesuffix(")")

            def query_result(context, query, evaluation_time):
                if "resets(" in query:
                    return int(f"resets({series}[" in query)
                return 150 if query in (queries[key] for key in before) else 1

            with self.subTest(metric=name), mock.patch.object(
                scale, "prometheus_query", side_effect=query_result
            ):
                with self.assertRaisesRegex(RuntimeError, f"counter reset.*{name}"):
                    scale.collect_metrics("test", queries, before, 42)

    def test_load_and_warmup_bound_stalled_requests_with_the_same_timeout(self):
        result = json.dumps({
            "summary": {"requestsPerSec": 1},
            "statusCodeDistribution": {"200": 1},
            "errorDistribution": {},
            "latencyPercentilesSuccessful": {"p95": 0.1},
        })
        with mock.patch.object(
            scale.subprocess, "check_output", return_value=result
        ) as execute:
            scale.run_oha(Path("/oha"), "http://example.test/", "30s", 1)
            scale.warm_target(Path("/oha"), "http://example.test/")
        timeouts = []
        for call in execute.call_args_list:
            arguments = call.args[0]
            self.assertIn("-t", arguments)
            value = arguments[arguments.index("-t") + 1]
            self.assertGreater(scale.duration_seconds(value), 0)
            self.assertGreater(call.kwargs["timeout"], 0)
            timeouts.append(value)
        self.assertEqual(timeouts[0], timeouts[1])

    def test_main_rejects_multiple_baseline_replicas_and_short_cpu_windows(self):
        for arguments, message in (
            (("--baseline-replicas", "2"), "one replica"),
            (("--duration", "1s"), "at least 30"),
        ):
            with self.subTest(arguments=arguments), mock.patch.object(
                scale.sys, "argv", self.main_arguments(*arguments)
            ), mock.patch.object(
                scale, "active_revision", return_value="demo-00001"
            ) as revision, mock.patch.object(
                scale, "command", return_value="{}"
            ), mock.patch.object(
                scale, "baseline_server", side_effect=AssertionError("unsafe configuration")
            ):
                with self.assertRaisesRegex(ValueError, message):
                    scale.main()
                revision.assert_not_called()

    def test_relative_oha_path_executes_the_requested_local_binary(self):
        execute = scale.command
        with tempfile.TemporaryDirectory() as temporary:
            executable = Path(temporary) / "oha"
            executable.write_text("#!/bin/sh\necho 'oha test'\n")
            executable.chmod(0o755)

            def metadata_command(*arguments):
                return execute(*arguments) if "--version" in arguments else "{}"

            with contextlib.chdir(temporary), mock.patch.dict(
                os.environ, {"PATH": os.defpath}
            ), mock.patch.object(
                scale.sys, "argv", self.main_arguments("--oha", "./oha")
            ), mock.patch.object(
                scale, "active_revision", return_value="demo-00001"
            ), mock.patch.object(
                scale, "command", side_effect=metadata_command
            ) as command, mock.patch.object(
                scale, "baseline_server", side_effect=RuntimeError("metadata collected")
            ):
                with self.assertRaisesRegex(RuntimeError, "metadata collected"):
                    scale.main()
            version = next(
                call.args for call in command.call_args_list if "--version" in call.args
            )
            self.assertEqual(version[0], str(executable))

    def test_scenario_waits_for_every_router_post_load_scrape(self):
        clock = scale.datetime.datetime.fromtimestamp(400, scale.datetime.timezone.utc)
        baseline = {
            "name": "baseline-owned",
            "url": "http://localhost/",
            "process": mock.Mock(poll=mock.Mock(return_value=None)),
        }
        client = {
            "summary": {"requestsPerSec": 1},
            "httpSuccessRate": 1,
            "successfulLatencyMs": {"p95": 1},
        }
        with mock.patch.object(
            scale.sys, "argv", self.main_arguments()
        ), mock.patch.object(
            scale, "active_revision", return_value="demo-00001"
        ), mock.patch.object(
            scale, "command", return_value="{}"
        ), mock.patch.object(
            scale, "baseline_server", return_value=contextlib.nullcontext(baseline)
        ), mock.patch.object(
            scale, "warm_target"
        ), mock.patch.object(
            scale, "run_oha", return_value=client
        ), mock.patch.object(
            scale, "prometheus_query", return_value=1
        ), mock.patch.object(
            scale, "router_scrapes", create=True,
            side_effect=[{"router-1": 399, "router-2": 399}]
            + [{"router-1": 420, "router-2": 399}] * 6,
        ), mock.patch.object(
            scale.datetime, "datetime", wraps=scale.datetime.datetime
        ) as datetime, mock.patch.object(
            scale.time, "time", return_value=420
        ), mock.patch.object(
            scale.time, "sleep"
        ), mock.patch.object(
            scale, "write_json"
        ) as write, mock.patch.object(
            scale, "write_report"
        ), mock.patch.object(scale, "print", create=True):
            datetime.now.return_value = clock
            with self.assertRaisesRegex(RuntimeError, "fresh.*router"):
                scale.main()
            write.assert_not_called()


if __name__ == "__main__":
    unittest.main()
