#!/usr/bin/env python3
"""Mutation tests of the catalog collection and its actual consumption path."""
from pathlib import Path
import tempfile
import unittest

from check_catalog_fanout import provider_fields, violations

MAIN = """
struct MultiProvider {
    alpha: RwLock<Option<Arc<dyn Provider>>>,
    endpoints: RwLock<HashMap<String, Arc<dyn Provider>>>,
    active: RwLock<ActiveProvider>,
}
impl Provider for MultiProvider {
    async fn prefetch_models(&self) { self.refresh_catalogs(false).await }
    async fn refresh_model_catalog(&self) { self.refresh_catalogs(true).await }
}
"""
REFRESH = """
impl MultiProvider {
    fn catalog_targets(&self) {
        let slots = [catalog_slot(&self.alpha)];
        targets.extend(self.endpoints.read().unwrap().iter().map(make_target));
        targets
    }
    async fn refresh_catalogs(&self, force: bool) {
        join_all(self.catalog_targets().into_iter().map(|target| async move {
            refresh_child_catalog(target.provider.as_ref(), force).await
        })).await
    }
}
async fn refresh_child_catalog(provider: &dyn Provider, force: bool) {
    if force { provider.refresh_model_catalog().await } else { provider.prefetch_models().await }
}
"""


class CatalogFanoutTests(unittest.TestCase):
    def check(self, main=MAIN, refresh=REFRESH):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            provider = root / "crates/jcode-base/src/provider"
            provider.mkdir(parents=True)
            (provider / "mod.rs").write_text(main)
            if refresh is not None:
                (provider / "catalog_refresh.rs").write_text(refresh)
            return violations(root)

    def test_every_slot_and_endpoint_map_in_both_modes_passes(self):
        self.assertEqual(self.check(), [])

    def test_arbitrary_future_slot_is_discovered_without_a_lint_allowlist(self):
        main = MAIN.replace("    active:", "    future_2099: RwLock<Option<Arc<future::FutureProvider>>>,\n    active:")
        errors = self.check(main)
        self.assertEqual(len(errors), 1)
        self.assertIn("future_2099", errors[0])
        self.assertIn("catalog_slot(&self.future_2099)", errors[0])
        self.assertEqual(self.check(main, REFRESH.replace("[catalog_slot(&self.alpha)]", "[catalog_slot(&self.alpha), catalog_slot(&self.future_2099)]")), [])

    def test_new_provider_map_cannot_be_forgotten(self):
        main = MAIN.replace("    active:", "    extra: RwLock<BTreeMap<String, Arc<dyn Provider>>>,\n    active:")
        self.assertIn("MultiProvider.extra", self.check(main)[0])

    def test_removing_a_real_target_is_red(self):
        errors = self.check(refresh=REFRESH.replace("catalog_slot(&self.alpha)", "None"))
        self.assertEqual(len(errors), 1)
        self.assertIn("MultiProvider.alpha", errors[0])

    def test_bypassing_the_collection_is_red_even_if_the_collection_is_complete(self):
        errors = self.check(refresh=REFRESH.replace("self.catalog_targets()", "active_only()"))
        self.assertIn("consume every catalog_targets entry", errors[0])

    def test_dropping_force_at_consumption_is_red(self):
        errors = self.check(refresh=REFRESH.replace("target.provider.as_ref(), force", "target.provider.as_ref(), false"))
        self.assertIn("forward force", errors[0])

    def test_refresh_cannot_silently_degrade_to_prefetch(self):
        self.assertIn("refresh_catalogs(true)", self.check(main=MAIN.replace("refresh_catalogs(true)", "refresh_catalogs(false)"))[0])
        broken = REFRESH.replace("provider.refresh_model_catalog()", "provider.prefetch_models()")
        self.assertIn("forced refresh must call refresh_model_catalog", self.check(refresh=broken)[0])

    def test_comments_and_test_only_references_cannot_satisfy_coverage(self):
        broken = REFRESH.replace("catalog_slot(&self.alpha)", "None /* catalog_slot(&self.alpha) */")
        broken += "\n#[cfg(test)]\nfn catalog_targets() { catalog_slot(&self.alpha); }"
        self.assertIn("MultiProvider.alpha", self.check(refresh=broken)[0])

    def test_historical_manual_fanout_reports_actual_missing_endpoint_map(self):
        old = MAIN.replace("self.refresh_catalogs(false).await", "self.alpha_provider().prefetch_models().await")
        errors = self.check(main=old, refresh=None)
        self.assertTrue(any("MultiProvider.endpoints: missing catalog fanout" in error for error in errors))
        self.assertFalse(any("MultiProvider.alpha:" in error for error in errors))

    def test_field_parser_handles_map_commas_and_skips_non_provider_state(self):
        self.assertEqual(provider_fields(MAIN), {"alpha": False, "endpoints": True})

    def test_missing_owner_or_no_provider_fields_fails_closed(self):
        for main in ("struct Renamed {}", "struct MultiProvider { active: ActiveProvider }"):
            with self.assertRaises(ValueError):
                self.check(main=main)


if __name__ == "__main__":
    unittest.main()
