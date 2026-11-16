"""Path jail for the oracle proxy.

Run from the repo root:

    python3 quickstart/services/test_oracle_paths.py

The server module is loaded by path because its filename contains a hyphen.
"""

import importlib.util
import os
import tempfile
import unittest
from pathlib import Path


def _load_server():
    path = Path(__file__).with_name("oracle-server.py")
    spec = importlib.util.spec_from_file_location("oracle_server", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PathJailTest(unittest.TestCase):
    def setUp(self):
        self.server = _load_server()
        self.tmp = tempfile.TemporaryDirectory()
        self.root = self.tmp.name
        self.work = os.path.join(self.root, "work")
        os.makedirs(self.work)
        # The module reads TASK_ROOT at call time, so a test can point it
        # at a scratch directory instead of the real /task.
        self.server.TASK_ROOT = self.root

    def tearDown(self):
        self.tmp.cleanup()

    def test_absolute_path_outside_is_rejected(self):
        outside = os.path.join(tempfile.gettempdir(), "mesen.wasm")
        bad = self.server._paths_inside_task(
            ["run", outside, "--dump-audio", os.path.join(self.root, "out.wav")],
            self.work,
        )
        self.assertEqual(bad, outside)

    def test_relative_dotdot_is_rejected(self):
        self.assertTrue(self.server._outside_task("../../dev/zero", self.work))
        self.assertTrue(self.server._outside_task("../../tmp/out.wav", self.work))
        bad = self.server._paths_inside_task(
            ["run", "../../dev/zero", "--dump-audio", "../../tmp/out.wav"],
            self.work,
        )
        self.assertIn(bad, ("../../dev/zero", "../../tmp/out.wav"))

    def test_absolute_dotdot_is_rejected(self):
        raw = self.root + "/../outside.wasm"
        bad = self.server._paths_inside_task(["run", raw], self.work)
        self.assertEqual(bad, raw)

    def test_rom_inside_task_is_allowed(self):
        rom = os.path.join(self.work, "game.gba")
        bad = self.server._paths_inside_task(
            ["run", "game.gba", "60", "--replay", "inputs.txt"],
            self.work,
        )
        self.assertIsNone(bad)
        # The file does not have to exist for the jail. The oracle reports
        # a missing ROM itself. A session start still requires a real file.
        self.assertFalse(os.path.exists(rom))

    def test_symlink_to_the_outside_is_rejected(self):
        with tempfile.TemporaryDirectory() as secret_dir:
            outside = os.path.join(secret_dir, "mesen.wasm")
            with open(outside, "wb") as handle:
                handle.write(b"nope")
            link = os.path.join(self.work, "link.gba")
            os.symlink(outside, link)
            bad = self.server._paths_inside_task(["run", "link.gba"], self.work)
            self.assertEqual(bad, "link.gba")

    def test_info_has_no_paths(self):
        self.assertIsNone(self.server._paths_inside_task(["info"], self.work))


if __name__ == "__main__":
    unittest.main()
