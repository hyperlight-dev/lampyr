import unittest
from unittest import skip as disabled
import pytest


# ruleid: python-disabled-test-api
@unittest.skip("later")
def test_skipped():
    pass


# ruleid: python-disabled-test-api
@disabled("later")
def test_alias():
    pass


# ruleid: python-disabled-test-api
@unittest.expectedFailure
def test_expected_failure():
    pass


# ruleid: python-disabled-test-api
@pytest.mark.skipif(True)
def test_pytest_skip():
    pass


class Example(unittest.TestCase):
    def test_runtime_skip(self):
        # ruleid: python-disabled-test-method
        self.skipTest("later")


def fail_setup():
    # ruleid: python-disabled-test-api
    raise unittest.SkipTest("later")


# ok: python-disabled-test-api, python-disabled-test-method
fixture = 'self.skipTest("fixture"); unittest.skip("fixture")'
