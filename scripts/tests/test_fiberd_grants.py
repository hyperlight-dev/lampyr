#!/usr/bin/env python3

import os
from pathlib import Path
import json
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import fiberd_grants  # noqa: E402


def grant(generation=2, observed=2, ready="True", progressing="NewReplicaSetAvailable"):
    return {
        "metadata": {"name": "demo-00001", "uid": "uid-1", "generation": generation},
        "status": {
            "observedGeneration": observed,
            "conditions": [
                {"type": "Ready", "status": ready, "reason": "Available"},
                {"type": "Progressing", "status": "True", "reason": progressing},
            ],
        },
    }


def deployment(generation=4, observed=4, replicas=1, updated=1, current=1, available=1):
    spec = {} if replicas is None else {"replicas": replicas}
    return {
        "metadata": {"name": "fiberd-demo-00001", "generation": generation},
        "spec": spec,
        "status": {
            "observedGeneration": observed,
            "replicas": current,
            "updatedReplicas": updated,
            "availableReplicas": available,
        },
    }


class FiberdGrantsTest(unittest.TestCase):
    def test_a_grant_is_ready_once_the_controller_rolled_out_its_current_spec(self):
        self.assertTrue(fiberd_grants.grant_ready(grant(), deployment()))
        self.assertFalse(fiberd_grants.grant_ready(grant(generation=3), deployment()))
        self.assertFalse(fiberd_grants.grant_ready(grant(ready="False"), deployment()))
        # An old Pod keeps the grant Ready while its replacement rolls out.
        self.assertFalse(fiberd_grants.grant_ready(grant(progressing="ReplicaSetUpdated"), deployment()))
        self.assertFalse(fiberd_grants.grant_ready({"metadata": {"generation": 1}}, deployment()))

    def test_a_ready_grant_waits_for_its_deployment_to_roll_out(self):
        # The controller can report the old Pod's conditions under the new
        # grant generation, so the Deployment decides.
        self.assertFalse(fiberd_grants.grant_ready(grant(), None))
        self.assertFalse(fiberd_grants.grant_ready(grant(), deployment(observed=3)))
        # The old Pod still runs beside the replacement.
        self.assertFalse(fiberd_grants.grant_ready(grant(), deployment(current=2)))
        self.assertFalse(fiberd_grants.grant_ready(grant(), deployment(updated=0)))
        self.assertFalse(fiberd_grants.grant_ready(grant(), deployment(available=0)))
        # A Deployment with no replicas set runs one.
        self.assertTrue(fiberd_grants.grant_ready(grant(), deployment(replicas=None)))
        self.assertFalse(fiberd_grants.grant_ready(grant(), deployment(replicas=None, updated=0)))

    def test_the_grant_is_read_before_its_deployment(self):
        calls = []

        def kubectl(context, *args, namespace="default", timeout=60):
            calls.append((args[1], args[2], namespace))
            return json.dumps(grant()) if args[1] == fiberd_grants.GRANTS else ""

        with patch.object(fiberd_grants, "kubectl", side_effect=kubectl):
            self.assertFalse(fiberd_grants.grant_settled("kind-test", "demo-00001"))
        self.assertEqual(calls, [
            (fiberd_grants.GRANTS, "demo-00001", "default"),
            ("deployment", "fiberd-demo-00001", "default"),
        ])

    def test_the_home_is_the_one_running_pod_the_controller_labels(self):
        self.assertEqual(
            fiberd_grants.pod_selector(grant()),
            "fiberd.io/grant=demo-00001,fiberd.io/grant-uid=uid-1",
        )
        pods = [
            {"metadata": {"name": "pending", "uid": "pod-0"}, "status": {"phase": "Pending"}},
            {"metadata": {"name": "leaving", "uid": "pod-1",
                "deletionTimestamp": "2026-11-06T00:00:00Z"}, "status": {"phase": "Running"}},
            {"metadata": {"name": "home", "uid": "pod-2"}, "status": {"phase": "Running"}},
        ]
        self.assertEqual(fiberd_grants.running_home(pods), ("home", "pod-2"))
        with self.assertRaisesRegex(RuntimeError, "expected one running grant Pod"):
            fiberd_grants.running_home(pods[:2])

    def test_starts_and_epoch_come_from_the_grant_pod_audit_records(self):
        def record(event, uid="pod-2", epoch=1, seq=0):
            return json.dumps({"event": event, "fence": {"GrantUID": uid, "Epoch": epoch, "Seq": seq}})

        raw = "\n".join([
            record("admit", epoch=1),
            record("create", seq=1),
            record("release", seq=1),
            record("resume", seq=2),
            # fiberd's grant UID is the Pod's, never the CapacityGrant's.
            record("create", uid="uid-1", epoch=9),
            "{torn",
            record("admit", epoch=2),
            record("attach", epoch=2, seq=1),
        ])
        records = fiberd_grants.grant_records(raw, "pod-2")
        self.assertEqual(len(records), 6)
        self.assertEqual(fiberd_grants.fibers_started(records), 2)
        self.assertEqual(fiberd_grants.epoch(records), 2)
        self.assertEqual(fiberd_grants.epoch([]), 0)

    def test_the_audit_spool_is_read_from_the_home_and_kept_for_its_uid(self):
        raw = "\n".join([
            json.dumps({"event": "create", "fence": {"GrantUID": "pod-2"}}),
            json.dumps({"event": "create", "fence": {"GrantUID": "uid-1"}}),
        ])
        home = fiberd_grants.Home("home", "pod-2")
        with patch.object(fiberd_grants, "kubectl", return_value=raw) as kubectl:
            records = fiberd_grants.audit_records("kind-test", home)
        kubectl.assert_called_once_with(
            "kind-test", "exec", "pod/home", "--container", "fiberd", "--",
            "cat", fiberd_grants.AUDIT_SPOOL,
        )
        self.assertEqual([record["fence"]["GrantUID"] for record in records], ["pod-2"])

    def test_busy_fibers_are_the_revision_series_of_the_router_gauge(self):
        metrics = "\n".join([
            "# HELP lampyr_router_fibers_busy Fibers serving a request right now.",
            "# TYPE lampyr_router_fibers_busy gauge",
            'lampyr_router_fibers_busy{namespace="default",revision="demo-00001"} 3',
            'lampyr_router_fibers_busy{namespace="default",revision="demo-00002"} 5',
            'lampyr_router_fibers_busy{namespace="other",revision="demo-00001"} 7',
            'lampyr_router_fiber_acquire_seconds_count{namespace="default",revision="demo-00001",kind="CREATE"} 11',
        ])
        self.assertEqual(fiberd_grants.parse_fibers_busy(metrics, "default", "demo-00001"), 3)
        # The router creates the series on the first Fiber.
        self.assertEqual(fiberd_grants.parse_fibers_busy(metrics, "default", "demo-00003"), 0)

    def test_port_forward_drains_kubectl_and_gives_up_on_silence(self):
        with tempfile.TemporaryDirectory() as work:
            kubectl = Path(work) / "kubectl"
            # More stderr than a pipe holds comes before the port, which a
            # pipe nobody reads would block on.
            scripts = {
                "forwards": "head -c 200000 /dev/zero >&2\n"
                            "echo 'Forwarding from 127.0.0.1:4567 -> 9090'\nexec sleep 30\n",
                "silent": "exec sleep 30\n",
                "fails": "echo 'error: pod not found' >&2\nexit 1\n",
            }
            with patch.dict(os.environ, {"PATH": f"{work}:{os.environ['PATH']}"}):
                for name, script in scripts.items():
                    with self.subTest(kubectl=name):
                        kubectl.write_text("#!/bin/sh\n" + script)
                        kubectl.chmod(0o755)
                        started = time.monotonic()
                        forward = fiberd_grants.port_forward("ctx", "ns", "pod/router", 9090, timeout=2)
                        if name == "forwards":
                            with forward as port:
                                self.assertEqual(port, 4567)
                        else:
                            with self.assertRaisesRegex(RuntimeError, "port-forward to pod/router failed") as raised:
                                with forward:
                                    self.fail("port-forward yielded without a port")
                            if name == "fails":
                                self.assertIn("pod not found", str(raised.exception))
                        self.assertLess(time.monotonic() - started, 10)


if __name__ == "__main__":
    unittest.main()
