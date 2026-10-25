import asyncio
from pathlib import Path
import unittest
from unittest.mock import patch

try:
    from . import testbed
except ImportError:
    import testbed

Job, parse_job, pump, slot_cpus = testbed.Job, testbed.parse_job, testbed.pump, testbed.slot_cpus


class CpuMappingTests(unittest.TestCase):
    def test_disjoint_slots_and_wrap(self):
        available = list(range(8))
        self.assertEqual([slot_cpus(slot, 2, available) for slot in range(5)],
                         [[0, 1], [2, 3], [4, 5], [6, 7], [0, 1]])

    def test_noncontiguous_allowed_cpus(self):
        self.assertEqual(slot_cpus(1, 2, [9, 3, 5, 7]), [7, 9])
        self.assertEqual(slot_cpus(1, 3, [2, 4, 8, 10]), [2, 4, 10])
        self.assertEqual(slot_cpus(0, 4, [2]), [2])

    def test_invalid_inputs(self):
        for slot, count, available in [(-1, 2, [0]), (0, 0, [0]), (0, 1, [])]:
            with self.assertRaises(ValueError):
                slot_cpus(slot, count, available)


class JobParsingTests(unittest.TestCase):
    def test_comments_and_blank_lines(self):
        self.assertIsNone(parse_job("  # ignored"))
        self.assertIsNone(parse_job(""))

    def test_quoting_and_benchmark_out_argument(self):
        self.assertEqual(parse_job('node qa/storage_bench.mjs --label "A B" --out result.jsonl # comment'),
                         Job(("node", "qa/storage_bench.mjs", "--label", "A B", "--out", "result.jsonl")))
        self.assertEqual(parse_job("python3 -c 'print(\"a > b # c\")'").command,
                         ("python3", "-c", 'print("a > b # c")'))

    def test_output_redirection(self):
        self.assertEqual(parse_job('echo "hello world" > "a b.jsonl"'),
                         Job(("echo", "hello world"), Path("a b.jsonl")))
        self.assertEqual(parse_job("echo '{}' >> shared.jsonl"),
                         Job(("echo", "{}"), Path("shared.jsonl"), True))

    def test_invalid_jobs(self):
        for line in ['echo "unterminated', "echo a >", "> file", "echo > a > b",
                     "echo > file extra", "echo a | cat", "echo a && echo b"]:
            with self.subTest(line=line), self.assertRaises(ValueError):
                parse_job(line)

    def test_shell_is_explicit(self):
        self.assertEqual(parse_job("sh -c 'echo a | cat'").command,
                         ("sh", "-c", "echo a | cat"))


class OutputTests(unittest.TestCase):
    def test_split_reads_and_final_partial_line(self):
        async def check():
            reader = asyncio.StreamReader()
            reader.feed_data(b'{"job":1}\n{"jo')
            reader.feed_data(b'b":2}')
            reader.feed_eof()
            # A real file is used because shared output uses flock.
            import tempfile
            with tempfile.TemporaryFile() as output:
                await pump(reader, output)
                output.seek(0)
                self.assertEqual(output.read(), b'{"job":1}\n{"job":2}\n')
        asyncio.run(check())


class CleanupTests(unittest.TestCase):
    def test_signal_during_normal_cleanup_still_finishes_cleanup(self):
        async def check():
            started, release, finished = asyncio.Event(), asyncio.Event(), asyncio.Event()

            async def teardown(name):
                self.assertEqual(name, "mistlab-0")
                started.set()
                await release.wait()
                finished.set()

            with patch.object(testbed, "teardown", teardown):
                task = asyncio.create_task(testbed.protected_teardown("mistlab-0"))
                await started.wait()
                task.cancel()
                await asyncio.sleep(0)
                self.assertFalse(task.done())
                release.set()
                with self.assertRaises(asyncio.CancelledError):
                    await task
                self.assertTrue(finished.is_set())

        asyncio.run(check())


if __name__ == "__main__":
    unittest.main()
