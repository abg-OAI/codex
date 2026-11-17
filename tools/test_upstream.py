import copy
import unittest

from upstream import release_source


class ReleaseSourceTest(unittest.TestCase):
    def setUp(self):
        self.manifest = {
            "operation": [{"git-clone": {
                "url": "https://github.com/openai/codex.git",
                "ref": "refs/tags/rust-v0.171.0-alpha.11",
                "commit": "57da98c18f84311dc23abfe1cda8f2c0dbd41cb9",
            }}],
        }

    def test_release_identity_comes_from_the_import(self):
        self.assertEqual(release_source(self.manifest), (
            "rust-v0.171.0-alpha.11", "57da98c18f84311dc23abfe1cda8f2c0dbd41cb9",
        ))
        self.manifest["operation"][0]["git-clone"]["ref"] = "refs/tags/rust-v0.171.0"
        self.assertEqual(release_source(self.manifest)[0], "rust-v0.171.0")

    def test_rejects_sources_outside_the_release_contract(self):
        for field, value in (
            ("url", "https://github.com/another/codex.git"),
            ("ref", "refs/heads/main"),
            ("ref", "refs/tags/rust-v0.171.0\nextra"),
            ("commit", "57da98c"),
            ("dir", "vendor/codex"),
        ):
            with self.subTest(field=field, value=value):
                manifest = copy.deepcopy(self.manifest)
                manifest["operation"][0]["git-clone"][field] = value
                with self.assertRaises(ValueError):
                    release_source(manifest)


if __name__ == "__main__":
    unittest.main()
