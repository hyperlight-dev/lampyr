import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest import mock


PATH = Path(__file__).resolve().parents[1] / "scale-test.py"
SPEC = importlib.util.spec_from_file_location("scale_test", PATH)
scale = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(scale)


class ScaleTestTests(unittest.TestCase):
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
        self.assertIn("| 0 | 0 | 0.300 | 3.00 |", report)
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
            "baselineCPU": None,
            "baselineMemoryBytes": None,
        }
        self.assertTrue(scale.target_resources_available("lampyr", metrics))
        self.assertFalse(scale.target_resources_available("kubernetes", metrics))

    def test_baseline_manifest_uses_restricted_pod_security(self):
        manifest = scale.baseline_manifest(
            "example.test/baseline@sha256:abc", 1, "team-a"
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
            ["apply", "delete"],
        )


if __name__ == "__main__":
    unittest.main()
