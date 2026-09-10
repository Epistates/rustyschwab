"""Acceptance criteria must reject incomplete or unhealthy soak evidence."""
import unittest
from types import SimpleNamespace
from soak import qualify


class QualificationTests(unittest.TestCase):
    def result(self, delay=0):
        return {"seconds":60,"max_delivery_gap_seconds":.5,"delivered":100,"generation":3,"lost_messages":0 if delay == 0 else 2,"peak_sampled_rss_bytes":20*1024**2}

    def check_result(self, result, samples=None, delay=0):
        if samples is None:
            samples = [{"seconds":t,"rss_bytes":20*1024**2} for t in range(61)]
        qualify(result,samples,SimpleNamespace(seconds=60,consumer_delay_ms=delay,warmup_seconds=30,max_rss_mib=128,max_rss_growth_mib=32,max_delivery_gap_seconds=10,max_sample_gap_seconds=5))
        return result["passed"]

    def test_healthy_fast_and_overflow_runs(self):
        self.assertTrue(self.check_result(self.result()))
        self.assertTrue(self.check_result(self.result(10),delay=10))

    def test_empty_or_early_exit_never_passes(self):
        for field, value in [("delivered",0),("seconds",59),("generation",1),("max_delivery_gap_seconds",30)]:
            result=self.result();result[field]=value
            self.assertFalse(self.check_result(result))

    def test_missing_memory_samples_and_warmup_fail(self):
        self.assertFalse(self.check_result(self.result(),[]))
        self.assertFalse(self.check_result(self.result(),[{"seconds":1,"rss_bytes":1024}]))

    def test_sampling_gaps_and_process_failures_fail(self):
        self.assertFalse(self.check_result(self.result(), [{"seconds":t,"rss_bytes":1024} for t in [0,31,59]]))
        for field, value in [("process_returncode",1),("error","timeout")]:
            result=self.result();result[field]=value
            self.assertFalse(self.check_result(result))
        samples=[{"seconds":t,"rss_bytes":1024} for t in range(61)]
        samples[20]["rss_bytes"]=float("nan")
        self.assertFalse(self.check_result(self.result(),samples))

    def test_memory_growth_and_unexpected_loss_fail(self):
        result=self.result();result["lost_messages"]=1
        self.assertFalse(self.check_result(result))
        result=self.result();result["peak_sampled_rss_bytes"]=129*1024**2
        self.assertFalse(self.check_result(result))
        self.assertFalse(self.check_result(self.result(),[{"seconds":31,"rss_bytes":1024},{"seconds":59,"rss_bytes":40*1024**2}]))


class FailureEvidenceTests(unittest.TestCase):
    def run_failure(self, *, returncode=1, payload='', sampling_error=None):
        import json
        import pathlib
        import tempfile
        from unittest.mock import Mock, patch
        from soak import main

        process = Mock(returncode=returncode)
        process.poll.return_value = returncode

        def start(*args, **kwargs):
            kwargs['stdout'].write(payload)
            kwargs['stderr'].write(b'synthetic diagnostic')
            return process

        with tempfile.TemporaryDirectory() as directory:
            destination = pathlib.Path(directory) / 'evidence.json'
            with patch('sys.argv', ['soak', '--seconds', '1', '--output', str(destination)]), \
                    patch('pathlib.Path.read_bytes', return_value=b'synthetic binary'), \
                    patch('soak.subprocess.Popen', side_effect=start), \
                    patch('soak.subprocess.run', side_effect=sampling_error), \
                    patch('builtins.print'):
                with self.assertRaises(SystemExit) as stopped:
                    main()
                self.assertEqual(stopped.exception.code, 1)
            evidence = json.loads(destination.read_text())
            self.assertFalse(evidence['passed'])
            self.assertIn('error', evidence)
            self.assertEqual(evidence['stderr_tail'], 'synthetic diagnostic')
            if returncode is None:
                process.kill.assert_called_once()
                process.wait.assert_called_once()
            return evidence

    def test_crash_persists_diagnostics(self):
        self.assertEqual(self.run_failure()['process_returncode'], 1)

    def test_invalid_metrics_persist_failure(self):
        for payload in ['', '{}', '{"seconds":NaN}', '[]']:
            with self.subTest(payload=payload):
                self.run_failure(returncode=0, payload=payload)

    def test_sampling_timeout_kills_child_and_persists_failure(self):
        import subprocess
        evidence = self.run_failure(returncode=None, sampling_error=subprocess.TimeoutExpired('ps', 2))
        self.assertIn('TimeoutExpired', evidence['error'])


if __name__ == "__main__":
    unittest.main()
