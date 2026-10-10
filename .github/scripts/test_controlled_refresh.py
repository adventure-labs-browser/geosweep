"""Unit tests for the Actions remote graceful-stop controller (no network)."""
import importlib.util
import os
from pathlib import Path
import signal
import sys
import unittest
from unittest import mock
from urllib.error import HTTPError

spec = importlib.util.spec_from_file_location(
    "controlled_refresh", Path(__file__).with_name("controlled-refresh.py"))
controller = importlib.util.module_from_spec(spec)
spec.loader.exec_module(controller)


class Response:
    status = 200

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        return False


class SoftStopTests(unittest.TestCase):
    def test_run_specific_ref_and_reject_bad_ids(self):
        self.assertEqual(
            controller.control_ref("a/b", "12345"),
            "repos/a/b/git/ref/heads/geosweep-stop-12345")
        for invalid in ("", "-1", "../123", "123/../../"):
            with self.assertRaises(ValueError):
                controller.control_ref("a/b", invalid)

    def test_marker_found(self):
        with mock.patch.object(controller, "urlopen", return_value=Response()) as call:
            self.assertTrue(controller.stop_requested("a/b", "123", "token"))
            request = call.call_args.args[0]
            self.assertTrue(request.full_url.endswith("geosweep-stop-123"))

    def test_absent_marker_is_not_a_stop(self):
        missing = HTTPError("https://example.org", 404, "not found", {}, None)
        with mock.patch.object(controller, "urlopen", side_effect=missing):
            self.assertFalse(controller.stop_requested("a/b", "123", "token"))

    def test_other_http_errors_are_not_a_stop(self):
        error = HTTPError("https://example.org", 403, "forbidden", {}, None)
        with mock.patch.object(controller, "urlopen", side_effect=error):
            with self.assertRaises(HTTPError):
                controller.stop_requested("a/b", "123", "token")

    def test_requested_stop_waits_for_child_and_reports_success(self):
        process = mock.Mock()
        process.poll.return_value = None
        process.wait.return_value = 143
        env = {"GITHUB_REPOSITORY": "a/b", "GITHUB_RUN_ID": "987",
               "GH_TOKEN": "token", "GC_BEARER": "must-not-leak",
               "GC_JAR": "must-not-leak", "RUNNER_OS": "macOS"}
        from contextlib import ExitStack
        with ExitStack() as stack:
            stack.enter_context(mock.patch.dict(os.environ, env, clear=True))
            stack.enter_context(mock.patch.object(sys, "argv", ["ctl", "2700", "1.0"]))
            popen = stack.enter_context(mock.patch.object(
                controller.subprocess, "Popen", return_value=process))
            stack.enter_context(mock.patch.object(
                controller, "stop_requested", return_value=True))
            stack.enter_context(mock.patch.object(
                controller.time, "monotonic", side_effect=[0, 21, 22]))
            self.assertEqual(controller.main(), 0)
        process.send_signal.assert_called_once_with(signal.SIGTERM)
        process.wait.assert_called()
        child_env = popen.call_args.kwargs["env"]
        for key in ("GH_TOKEN", "GC_BEARER", "GC_JAR"):
            self.assertNotIn(key, child_env)
        self.assertEqual(popen.call_args.args[0][0], "gtimeout")


if __name__ == "__main__":
    unittest.main()
