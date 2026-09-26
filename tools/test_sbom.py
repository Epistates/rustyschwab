"""The release SBOM lists what the shipped package links, and nothing else."""
import unittest
from sbom import shipped


def package(name, version="1.0.0"):
    return {"id": f"{name} {version}", "name": name, "version": version}


def dep(name, *kinds, version="1.0.0"):
    return {"pkg": f"{name} {version}", "dep_kinds": [{"kind": kind} for kind in kinds]}


def metadata():
    names = ["app", "sdk", "serde", "cc", "proptest", "bench", "anyhow"]
    return {
        "packages": [package(name) for name in names],
        "workspace_members": ["app 1.0.0", "sdk 1.0.0", "bench 1.0.0"],
        "resolve": {"nodes": [
            {"id": "app 1.0.0", "deps": [dep("sdk", None), dep("proptest", "dev")]},
            {"id": "sdk 1.0.0", "deps": [dep("serde", None), dep("cc", "build"),
                                         dep("anyhow", "dev")]},
            {"id": "serde 1.0.0", "deps": []},
            {"id": "cc 1.0.0", "deps": []},
            {"id": "proptest 1.0.0", "deps": []},
            {"id": "bench 1.0.0", "deps": [dep("app", None)]},
            {"id": "anyhow 1.0.0", "deps": []},
        ]},
    }


class ShippedTests(unittest.TestCase):
    def test_normal_and_build_edges_only(self):
        root, _, ids, edges = shipped(metadata(), "app")
        self.assertEqual(root, "app 1.0.0")
        # Dev-dependencies (at any depth) and other workspace members are out.
        self.assertEqual(ids, {"app 1.0.0", "sdk 1.0.0", "serde 1.0.0", "cc 1.0.0"})
        self.assertEqual(edges["sdk 1.0.0"], ["cc 1.0.0", "serde 1.0.0"])

    def test_an_edge_that_is_also_dev_still_ships(self):
        data = metadata()
        data["resolve"]["nodes"][0]["deps"][1] = dep("proptest", "dev", None)
        _, _, ids, _ = shipped(data, "app")
        self.assertIn("proptest 1.0.0", ids)

    def test_the_root_must_be_a_workspace_member(self):
        with self.assertRaises(SystemExit):
            shipped(metadata(), "serde")
        with self.assertRaises(SystemExit):
            shipped(metadata(), "missing")


if __name__ == "__main__":
    unittest.main()
