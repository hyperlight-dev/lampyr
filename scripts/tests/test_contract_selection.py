import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location(
    "wire_contracts", ROOT / "tests/contracts/test_contract.py",
)
contracts = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contracts)


class ContractSelectionTests(unittest.TestCase):
    def selected(self, runtimes, vm):
        with patch.object(contracts, "RUNTIMES", dict.fromkeys(runtimes)), \
                patch.object(contracts, "VM", vm):
            suite = contracts.load_tests(unittest.TestLoader(), None, None)
            return {test._testMethodName for test in suite}

    def test_default_native_suite_includes_every_contract(self):
        self.assertEqual(self.selected(contracts.RUNTIMES, False),
                         set(unittest.TestLoader().getTestCaseNames(contracts.Contract)))

    def test_runtime_selection_keeps_only_applicable_contracts(self):
        selected = self.selected(["python"], False)
        self.assertIn("test_idle_python_websockets_do_not_starve_http", selected)
        self.assertIn("test_slow_reader_applies_backpressure", selected)
        self.assertFalse(any(name.startswith(("test_rust_", "test_dotnet_", "test_go_"))
                             for name in selected))

    def test_vm_suite_excludes_only_native_contracts(self):
        native = self.selected(contracts.RUNTIMES, False)
        vm = self.selected(contracts.RUNTIMES, True)
        expected = {name for name in native
                    if getattr(getattr(contracts.Contract, name), "native_only", False)}
        self.assertTrue(expected)
        self.assertEqual(native - vm, expected)

    def test_unknown_runtime_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unknown-runtime"):
            contracts.contract_for("unknown-runtime")

    def test_explicit_incompatible_runtime_is_rejected(self):
        with patch.object(contracts, "RUNTIMES", {"python": None}):
            with self.assertRaisesRegex(ValueError, "no contracts match"):
                contracts.ContractLoader().loadTestsFromNames(
                    ["Contract.test_dotnet_typed_json_roundtrip"], contracts,
                )

    def test_explicit_native_only_contract_is_rejected_in_vm(self):
        with patch.object(contracts, "VM", True):
            with self.assertRaisesRegex(ValueError, "no contracts match"):
                contracts.ContractLoader().loadTestsFromNames(
                    ["Contract.test_request_body_limit"], contracts,
                )

    def test_explicit_class_and_methods_use_same_selection(self):
        with patch.object(contracts, "RUNTIMES", {"python": None}), \
                patch.object(contracts, "VM", False):
            loader = contracts.ContractLoader()
            suite = loader.loadTestsFromNames(["Contract"], contracts)
            self.assertEqual(
                {test._testMethodName for test in suite},
                self.selected(["python"], False),
            )
            suite = loader.loadTestsFromNames([
                "Contract.test_dotnet_typed_json_roundtrip",
                "Contract.test_idle_python_websockets_do_not_starve_http",
            ], contracts)
            self.assertEqual(
                [test._testMethodName for test in suite],
                ["test_idle_python_websockets_do_not_starve_http"],
            )


if __name__ == "__main__":
    unittest.main()
