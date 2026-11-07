#!/usr/bin/env python3

import importlib.util
from pathlib import Path
import sys
import unittest


ROOT = Path(__file__).resolve().parents[2]
# The gate imports its sibling fiberd_grants, as python3 scripts/... finds it.
sys.path.insert(0, str(ROOT / "scripts"))
SPEC = importlib.util.spec_from_file_location(
    "disruption_gate",
    ROOT / "scripts" / "disruption-gate.py",
)
disruption_gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(disruption_gate)


class DisruptionGateTest(unittest.TestCase):
    def test_active_revision_uses_resolved_traffic(self):
        service = {"status": {"traffic": [
            {"revisionName": "demo-00001", "percent": 100},
        ]}}
        self.assertEqual(disruption_gate.active_revision(service), "demo-00001")
        service["status"]["traffic"].append(
            {"revisionName": "demo-00002", "percent": 10}
        )
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            disruption_gate.active_revision(service)

    def test_outage_stops_only_the_active_serving_control_plane(self):
        self.assertEqual(
            disruption_gate.CONTROL_PLANE_DEPLOYMENTS,
            (
                "lampyr-serving-controller",
                "webhook",
                "net-kourier-controller",
            ),
        )
        self.assertNotIn("controller", disruption_gate.CONTROL_PLANE_DEPLOYMENTS)
        self.assertNotIn("autoscaler", disruption_gate.CONTROL_PLANE_DEPLOYMENTS)

    def test_the_policy_probe_breaks_only_the_image_rule(self):
        resource = {
            "apiVersion": "fiberd.io/v1alpha1",
            "kind": "CapacityGrant",
            "metadata": {
                "name": "demo-00001", "namespace": "default", "uid": "uid-1",
                "ownerReferences": [{"kind": "Revision", "name": "demo-00001"}],
            },
            "spec": {
                "fibers": {"max": 100},
                "podTemplate": {"spec": {"containers": [
                    {"name": "fiberd", "image": "ghcr.io/example/fiberd@sha256:" + "1" * 64},
                ]}},
            },
            "status": {"conditions": []},
        }
        probe = disruption_gate.policy_probe(resource, disruption_gate.POLICY_PROBE_IMAGE)
        self.assertEqual(
            probe["metadata"], {"generateName": "policy-probe-", "namespace": "default"}
        )
        self.assertNotIn("status", probe)
        self.assertEqual(
            probe["spec"]["podTemplate"]["spec"]["containers"][0]["image"],
            disruption_gate.POLICY_PROBE_IMAGE,
        )
        self.assertEqual(probe["spec"]["fibers"], {"max": 100})
        # The active grant is left as it was.
        self.assertTrue(
            resource["spec"]["podTemplate"]["spec"]["containers"][0]["image"].startswith(
                "ghcr.io/example/"
            )
        )


if __name__ == "__main__":
    unittest.main()
