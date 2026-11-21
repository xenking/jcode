#!/usr/bin/env python3
"""Contract tests for Cargo test isolation, independent of workspace unification."""
import copy
import unittest

from check_test_isolation import feature_closure, violations


def dependency(name, kind=None, features=(), optional=False, target=None, rename=None):
    return dict(name=name, kind=kind, features=list(features), optional=optional,
                target=target, rename=rename)


def package(name, features=None, dependencies=()):
    return dict(id=name, name=name, manifest_path=f"{name}/Cargo.toml",
                features=features or {}, dependencies=list(dependencies))


def fixture():
    base = package("jcode-base", {"test-support": ["test-isolated-hooks"], "test-isolated-hooks": []})
    app = package("app", dependencies=[dependency("jcode-base"), dependency("jcode-base", "dev", ["test-support"])])
    return dict(packages=[base, app], workspace_members=["jcode-base", "app"])


class TestIsolationTests(unittest.TestCase):
    def test_isolated_dev_feature_passes(self):
        self.assertEqual(violations(fixture()), [])

    def test_test_support_cannot_omit_hook_isolation(self):
        data = fixture()
        data["packages"][0]["features"]["test-support"] = []
        self.assertIn("test-support must enable test-isolated-hooks", "\n".join(violations(data)))

    def test_new_runtime_consumer_needs_own_dev_feature(self):
        data = fixture()
        data["packages"][1]["dependencies"].pop()
        errors = "\n".join(violations(data))
        self.assertIn("tests can inherit", errors)
        self.assertIn('[dev-dependencies]', errors)

    def test_dev_only_consumer_is_also_checked(self):
        data = fixture()
        data["packages"][1]["dependencies"] = [dependency("jcode-base", "dev")]
        self.assertIn("dev-dependency jcode-base is not isolated", "\n".join(violations(data)))

    def test_production_features_must_not_suppress_users_hooks(self):
        data = fixture()
        data["packages"][1]["dependencies"][0]["features"] = ["test-support"]
        self.assertIn("production dependency", "\n".join(violations(data)))
        data = fixture()
        data["packages"][0]["features"]["default"] = ["test-support"]
        self.assertIn("default enables test isolation", "\n".join(violations(data)))

    def test_transitive_feature_forwarding_and_renamed_dependency(self):
        data = fixture()
        app = data["packages"][1]
        app["features"] = {"test-support": ["foundation/test-support"]}
        for dep in app["dependencies"]:
            dep["rename"] = "foundation"
        client = package("client", dependencies=[dependency("app"), dependency("app", "dev", ["test-support"])])
        data["packages"].append(client)
        data["workspace_members"].append("client")
        self.assertEqual(violations(data), [])
        client["dependencies"].pop()
        self.assertIn("real app home/hooks", "\n".join(violations(data)))

    def test_hook_only_feature_cannot_disable_production_hooks(self):
        data = fixture()
        data["packages"][1]["dependencies"][0]["features"] = ["test-isolated-hooks"]
        self.assertIn("production dependency", "\n".join(violations(data)))
        data = fixture()
        data["packages"][0]["features"]["default"] = ["test-isolated-hooks"]
        self.assertIn("default enables test isolation", "\n".join(violations(data)))

    def test_forwarded_hook_only_default_is_also_rejected(self):
        data = fixture()
        app = data["packages"][1]
        app["features"] = {"default": ["foundation/test-isolated-hooks"]}
        for dep in app["dependencies"]:
            dep["rename"] = "foundation"
        self.assertIn("default enables test isolation", "\n".join(violations(data)))

    def test_target_specific_opt_in_does_not_protect_other_platforms(self):
        data = fixture()
        data["packages"][1]["dependencies"][1]["target"] = "cfg(unix)"
        self.assertIn("unconditional", "\n".join(violations(data)))

    def test_optional_runtime_does_not_force_heavy_dependency_on_lightweight_tests(self):
        data = fixture()
        data["packages"][1]["dependencies"] = [dependency("jcode-base", optional=True)]
        self.assertEqual(violations(data), [])

    def test_feature_cycles_terminate(self):
        data = fixture()
        data["packages"][0]["features"]["test-isolated-hooks"] = ["test-support"]
        self.assertEqual(violations(data), [])

    def test_unrelated_external_packages_do_not_change_the_contract(self):
        data = fixture()
        before = copy.deepcopy(data)
        data["packages"].append(package("external"))
        self.assertEqual(violations(data), violations(before))


if __name__ == "__main__":
    unittest.main()
