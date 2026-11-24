import contextlib
import hashlib
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
            (("--output", "results.md"), "JSON and Markdown"),
            (("--output", "results.MD"), "JSON and Markdown"),
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

    @contextlib.contextmanager
    def scenario_environment(self, routers, resources, inventory=None, arguments=()):
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
        coverage = (
            contextlib.nullcontext() if inventory is not None else
            mock.patch.object(scale, "resource_coverage", create=True, return_value=True)
        )
        with mock.patch.object(
            scale.sys, "argv", self.main_arguments(*arguments)
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
            scale, "prometheus_query",
            side_effect=lambda context, query, evaluation_time=None: 0 if "resets(" in query else 1,
        ), mock.patch.object(
            scale, "router_scrapes", side_effect=routers,
        ), mock.patch.object(
            scale, "resource_samples", create=True, side_effect=resources,
        ), mock.patch.object(
            scale, "resource_inventory", create=True, return_value=inventory or {},
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
        ), mock.patch.object(scale, "print", create=True), coverage:
            datetime.now.return_value = clock
            yield write

    def test_scenario_waits_for_every_router_post_load_scrape(self):
        routers = [{"router-1": 400, "router-2": 400}]
        routers += [{"router-1": 420, "router-2": 399}] * 6
        resources = [{"cadvisor": 400, "kube-state": 400}]
        resources += [{"cadvisor": 420, "kube-state": 420}] * 6
        with self.scenario_environment(routers, resources) as write:
            with self.assertRaisesRegex(RuntimeError, "fresh.*router"):
                scale.main()
            write.assert_not_called()

    def test_scenario_rejects_stale_resource_samples(self):
        for source in ("cadvisor", "kube-state"):
            before = {"cadvisor": 400, "kube-state": 400}
            after = {"cadvisor": 420, "kube-state": 420}
            after[source] = 399
            routers = [{"router-1": 400}] + [{"router-1": 420}] * 20
            resources = [before] + [after] * 20
            with self.subTest(source=source), self.scenario_environment(
                routers, resources
            ) as write:
                with self.assertRaisesRegex(RuntimeError, "fresh.*resource"):
                    scale.main()
                write.assert_not_called()

    def test_scenario_rejects_disappeared_resource_series(self):
        routers = [{"router-1": 420}] * 20
        resources = [{"container-1": 400, "container-2": 400}]
        resources += [{"container-1": 420}] * 20
        with self.scenario_environment(routers, resources) as write:
            with self.assertRaisesRegex(RuntimeError, "fresh.*resource"):
                scale.main()
            write.assert_not_called()

    def test_scenario_accepts_fresh_resource_samples(self):
        with self.scenario_environment(
            [{"router-1": 420}] * 20,
            [{"container-1": 420, "kube-state": 420}] * 20,
        ) as write:
            scale.main()
            self.assertEqual(len(write.call_args.args[0]["scenarios"]), 2)

    def test_warmup_and_load_use_identical_request_paths_and_queries(self):
        for lampyr, baseline in (
            ("https://example.test/", "http://localhost/"),
            (
                "https://example.test/compute?iterations=1000&size=64",
                "http://localhost/compute?iterations=1000&size=64",
            ),
            (
                "https://example.test/a%2Fb?value=a%26b&value=c+d&empty=",
                "http://localhost/a%2Fb?value=a%26b&value=c+d&empty=",
            ),
            (
                "https://example.test//other.test/compute?next=https%3A%2F%2Felsewhere.test",
                "http://localhost//other.test/compute?next=https%3A%2F%2Felsewhere.test",
            ),
            ("https://example.test?iterations=1000", "http://localhost/?iterations=1000"),
        ):
            expected_lampyr = lampyr.replace(
                "example.test?", "example.test/?"
            )
            with self.subTest(lampyr=lampyr), self.scenario_environment(
                [{"router-1": 420}] * 20,
                [{"container-1": 420}] * 20,
                arguments=("--lampyr-url", lampyr),
            ), mock.patch.object(scale, "warm_target") as warm, mock.patch.object(
                scale, "run_oha", return_value={
                    "summary": {"requestsPerSec": 1},
                    "httpSuccessRate": 1,
                    "successfulLatencyMs": {"p95": 1},
                },
            ) as load:
                scale.main()
                for execute in (warm, load):
                    self.assertEqual(
                        [call.args[1] for call in execute.call_args_list],
                        [expected_lampyr, baseline],
                    )

    def test_invalid_workload_urls_fail_before_cluster_work(self):
        for url in (
            "/compute?iterations=1000",
            "ftp://example.test/compute",
            "https:///compute",
            "https://example.test:99999/compute",
            "https://user:password@example.test/compute",
            "https://example.test/compute#iterations=1000",
            "https://example.test/compute#",
            "https://example.test/a b",
            "https://example.test/a\nb",
            "https://example.test\\other.test/compute",
        ):
            with self.subTest(url=url), mock.patch.object(
                scale.sys, "argv", self.main_arguments("--lampyr-url", url)
            ), mock.patch.object(
                scale, "active_revision", return_value="demo-00001"
            ) as revision, mock.patch.object(
                scale, "command", return_value="{}"
            ), mock.patch.object(
                scale, "baseline_server", side_effect=AssertionError("cluster work started")
            ):
                with self.assertRaises(ValueError):
                    scale.main()
                revision.assert_not_called()

    def test_scenario_rejects_missing_router_membership_at_both_endpoints(self):
        inventory = {
            "lampyrRouter": {
                ("lampyr-system", "router-1"), ("lampyr-system", "router-2"),
            },
            "lampyrHome": {("team-a", "home-1")},
        }
        queries = scale.resource_sample_queries(
            "team-a", "demo-00001", "baseline-owned", "lampyr"
        )
        for missing in ("membership", "router-target"):
            samples = {}
            for source in queries:
                pods = (
                    [("lampyr-system", f"router-{number}") for number in (
                        (1,) if missing == "membership" else (1, 2)
                    )] if source.startswith("lampyrRouter")
                    else [("team-a", "home-1")]
                )
                for namespace, pod in pods:
                    labels = {"scale_input": source, "namespace": namespace, "pod": pod}
                    samples[tuple(sorted(labels.items()))] = 420
            routers = {
                (("pod", f"router-{number}"),): 420
                for number in ((1, 2) if missing == "membership" else (1,))
            }
            with self.subTest(missing=missing), self.scenario_environment(
                [routers] * 20, [samples] * 20, inventory,
            ) as write:
                with self.assertRaisesRegex(RuntimeError, "fresh.*resource"):
                    scale.main()
                write.assert_not_called()

    def test_resource_samples_require_every_input_family(self):
        queries = {"cpu": "cpu_timestamp", "phase": "phase_timestamp"}
        complete = [
            {"metric": {"scale_input": "cpu", "pod": "one"}, "value": [450, "420"]},
            {"metric": {"scale_input": "phase", "pod": "one"}, "value": [450, "410"]},
        ]
        with mock.patch.object(scale, "prometheus_results", return_value=complete):
            samples = scale.resource_samples("context", queries, 450)
        self.assertEqual(sorted(samples.values()), [410, 420])
        for partial in ([], complete[:1]):
            with self.subTest(partial=partial), mock.patch.object(
                scale, "prometheus_results", return_value=partial
            ):
                self.assertIsNone(scale.resource_samples("context", queries, 450))

    def test_resource_inventory_selects_the_revision_and_unique_baseline(self):
        names = {
            "app=fiber-router": "router-1",
            "lampyr.dev/home=true,serving.knative.dev/revision=demo-00001": "home-1",
            "app=baseline-owned": "baseline-owned-one",
        }

        def pods(*arguments):
            namespace = arguments[arguments.index("--namespace") + 1]
            selector = arguments[arguments.index("--selector") + 1]
            return json.dumps({"items": [{
                "metadata": {"namespace": namespace, "name": names[selector]},
            }]})

        with mock.patch.object(scale, "command", side_effect=pods):
            lampyr = scale.resource_inventory(
                "context", "team-a", "demo-00001", "baseline-owned", "lampyr"
            )
            baseline = scale.resource_inventory(
                "context", "team-a", "demo-00001", "baseline-owned", "kubernetes"
            )
        self.assertEqual(lampyr, {
            "lampyrRouter": {("lampyr-system", "router-1")},
            "lampyrHome": {("team-a", "home-1")},
        })
        self.assertEqual(baseline, {
            "lampyrRouter": {("lampyr-system", "router-1")},
            "baseline": {("team-a", "baseline-owned-one")},
        })

    def test_resource_coverage_accepts_complete_target_inventories(self):
        routers = {(("pod", "router-1"),): 420}
        for target, prefix, pod in (
            ("lampyr", "lampyrHome", "home-1"),
            ("kubernetes", "baseline", "baseline-owned-one"),
        ):
            queries = scale.resource_sample_queries(
                "team-a", "demo-00001", "baseline-owned", target
            )
            inventory = {
                "lampyrRouter": {("lampyr-system", "router-1")},
                prefix: {("team-a", pod)},
            }
            samples = {}
            for source in queries:
                namespace, name = (
                    ("lampyr-system", "router-1") if source.startswith("lampyrRouter")
                    else ("team-a", pod)
                )
                labels = {"scale_input": source, "namespace": namespace, "pod": name}
                samples[tuple(sorted(labels.items()))] = 420
            with self.subTest(target=target):
                self.assertTrue(scale.resource_coverage(
                    samples, queries, inventory, routers
                ))
                self.assertFalse(scale.resource_coverage(
                    None, queries, inventory, routers
                ))

    def test_resource_samples_require_cpu_and_memory_for_each_container(self):
        queries = {
            "routersCpu": "cpu_timestamp",
            "routersMemory": "memory_timestamp",
            "routersPhase": "phase_timestamp",
        }
        complete = []
        for number in (1, 2):
            labels = {
                "namespace": "lampyr-system", "pod": f"router-{number}",
                "container": "router", "id": f"/container-{number}",
                "job": "kubernetes-cadvisor", "instance": f"node-{number}:10250",
                "node": f"node-{number}",
            }
            for source in queries:
                metric = {**labels, "scale_input": source}
                if source == "routersCpu":
                    metric["cpu"] = "total"
                complete.append({"metric": metric, "value": [450, "420"]})
        with mock.patch.object(scale, "prometheus_results", return_value=complete):
            self.assertIsNotNone(scale.resource_samples("context", queries, 450))
        for missing in ("routersCpu", "routersMemory"):
            partial = [
                item for item in complete
                if not (
                    item["metric"]["scale_input"] == missing
                    and item["metric"]["pod"] == "router-2"
                )
            ]
            with self.subTest(missing=missing), mock.patch.object(
                scale, "prometheus_results", return_value=partial
            ):
                self.assertIsNone(scale.resource_samples("context", queries, 450))
        mismatched = [
            {
                **item,
                "metric": {**item["metric"], "id": "/different-container"},
            } if item["metric"]["scale_input"] == "routersCpu" else item
            for item in complete
        ]
        with mock.patch.object(scale, "prometheus_results", return_value=mismatched):
            self.assertIsNone(scale.resource_samples("context", queries, 450))

    def test_installer_uses_unique_downloads_for_concurrent_invocations(self):
        justfile = (PATH.parents[1] / "Justfile").read_text()
        recipe = justfile.split("\ninstall-oha:\n", 1)[1].split("\n\n", 1)[0]
        checksums = scale.re.findall(r"[0-9a-f]{64}", recipe)
        self.assertEqual(len(checksums), 1)
        payload = b"#!/bin/sh\necho 'oha 1.17.0'\n"
        digest = hashlib.sha256(payload).hexdigest()
        recipe = recipe.replace(checksums[0], digest)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bin_path = root / "bin"
            bin_path.mkdir()
            (root / "payload").write_bytes(payload)
            (root / "Justfile").write_text(
                'set shell := ["bash", "-euo", "pipefail", "-c"]\n'
                'repo-root := justfile_directory()\noha-version := "1.17.0"\n'
                f"install-oha:\n{recipe}\n"
            )
            scripts = {
                "uname": "#!/bin/sh\necho 'Linux x86_64'\n",
                "sha256sum": (
                    "#!/usr/bin/env python3\n"
                    "import hashlib, pathlib, sys\n"
                    "expected, filename = sys.stdin.read().strip().split('  ', 1)\n"
                    "try:\n"
                    "    actual = hashlib.sha256(pathlib.Path(filename).read_bytes()).hexdigest()\n"
                    "except OSError:\n"
                    "    print('checksum input missing', file=sys.stderr)\n"
                    "    sys.exit(1)\n"
                    "sys.exit(0 if actual == expected else 1)\n"
                ),
                "curl": (
                    "#!/bin/bash\nset -euo pipefail\n"
                    "while (($#)); do\n"
                    "  if [[ $1 == --output ]]; then output=$2; shift 2; else shift; fi\n"
                    "done\n"
                    'cp "$TEST_ROOT/payload" "$output"\n'
                    "wait_for() {\n"
                    '  for ((i=0; i<500; i++)); do [[ -e $1 ]] && return 0; sleep 0.01; done\n'
                    "  return 1\n"
                    "}\n"
                    'if [[ $TEST_INSTALLER_ID == first ]]; then\n'
                    '  touch "$TEST_ROOT/first"\n'
                    '  wait_for "$TEST_ROOT/second"\n'
                    "else\n"
                    '  touch "$TEST_ROOT/second"\n'
                    '  wait_for "$TEST_ROOT/build/tools/oha"\n'
                    "fi\n"
                ),
            }
            for name, content in scripts.items():
                executable = bin_path / name
                executable.write_text(content)
                executable.chmod(0o755)
            environment = {
                **os.environ,
                "PATH": f"{bin_path}{os.pathsep}{os.environ['PATH']}",
                "TEST_ROOT": str(root),
            }
            processes = []
            try:
                for identifier in ("first", "second"):
                    processes.append(scale.subprocess.Popen(
                        ["just", "--justfile", str(root / "Justfile"), "install-oha"],
                        cwd=root,
                        env={**environment, "TEST_INSTALLER_ID": identifier},
                        stdout=scale.subprocess.PIPE,
                        stderr=scale.subprocess.PIPE,
                        text=True,
                    ))
                    if identifier == "first":
                        deadline = scale.time.monotonic() + 5
                        while not (root / "first").exists():
                            self.assertLess(scale.time.monotonic(), deadline)
                            scale.time.sleep(0.01)
                for process in processes:
                    stdout, stderr = process.communicate(timeout=10)
                    self.assertEqual(process.returncode, 0, stdout + stderr)
                self.assertEqual((root / "build/tools/oha").read_bytes(), payload)
            finally:
                for process in processes:
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=3)
                        except scale.subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()


if __name__ == "__main__":
    unittest.main()
