import json
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import start


class BuildCommandTests(unittest.TestCase):
    def test_uses_focused_pane_cwd_and_workspace(self):
        command = start.build_command(
            {
                "HERDR_BIN_PATH": "/opt/herdr",
                "HERDR_PLUGIN_ID": "risingtides.ocean",
                "HERDR_PLUGIN_CONTEXT_JSON": json.dumps(
                    {
                        "workspace_id": "w7",
                        "workspace_cwd": "/work/fallback",
                        "focused_pane_cwd": "/work/ocean",
                    }
                ),
            }
        )

        self.assertEqual(command[0], "/opt/herdr")
        self.assertIn("risingtides.ocean", command)
        self.assertEqual(command[command.index("--cwd") + 1], "/work/ocean")
        self.assertEqual(command[command.index("--workspace") + 1], "w7")

    def test_forwards_explicit_ocean_overrides(self):
        command = start.build_command(
            {
                "HERDR_PLUGIN_CONTEXT_JSON": json.dumps(
                    {"workspace_cwd": "/work/ocean"}
                ),
                "OCEAN_BIN": "/tmp/ocean",
                "OCEAN_DAEMON_URL": "http://127.0.0.1:9999",
            }
        )

        self.assertIn("OCEAN_BIN=/tmp/ocean", command)
        self.assertIn("OCEAN_DAEMON_URL=http://127.0.0.1:9999", command)

    def test_requires_a_workspace_cwd(self):
        with self.assertRaisesRegex(ValueError, "workspace cwd"):
            start.build_command({"HERDR_PLUGIN_CONTEXT_JSON": "{}"})

    def test_requires_object_context_json(self):
        with self.assertRaisesRegex(ValueError, "JSON object"):
            start.build_command({"HERDR_PLUGIN_CONTEXT_JSON": "[]"})


if __name__ == "__main__":
    unittest.main()
