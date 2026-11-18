#!/usr/bin/env python3
"""Regression tests for the swarm registry lock-order checker."""
import unittest

from check_lock_order import check


class LockOrderTests(unittest.TestCase):
    def scan(self, body):
        return check('fixture.rs', 'async fn f() { ' + body + ' }')

    def test_inversion_and_queued_writer_hazard(self):
        self.assertTrue(self.scan('let plans = swarm_plans.read().await; '
                                  'let members = swarm_members.read().await;'))
        self.assertTrue(self.scan('let a = swarm_members.read().await; '
                                  'let b = swarm_members.read().await;'))

    def test_unrelated_await(self):
        self.assertTrue(self.scan('let members = swarm_members.write().await; '
                                  'persist().await;'))

    def test_scrutinee_temporary_spans_body(self):
        self.assertTrue(self.scan('if let Some(id) = swarm_members.read().await.get("x").cloned() '
                                  '{ persist(id).await; }'))

    def test_reload_recovery_wedge_2026_11_18(self):
        # The daemon wedged for three hours when a reload recovery turn ended:
        # the scrutinee read guard lived through persist_swarm_state_for, which
        # reads members again. A writer queued in between blocks both forever.
        self.assertTrue(self.scan(
            'if let Some(swarm_id) = self.swarm.members.read().await.get(session_id)'
            '.and_then(|member| member.swarm_id.clone()) '
            '{ super::persist_swarm_state_for(&swarm_id, &self.swarm).await; }'))
        self.assertFalse(self.scan(
            'let swarm_id = self.swarm.members.read().await.get(session_id)'
            '.and_then(|member| member.swarm_id.clone()); '
            'if let Some(swarm_id) = swarm_id '
            '{ super::persist_swarm_state_for(&swarm_id, &self.swarm).await; }'))

    def test_order_and_snapshot(self):
        self.assertFalse(self.scan('let members = swarm_members.read().await; '
                                   'let plans = swarm_plans.write().await;'))
        self.assertFalse(self.scan('let ids = swarm_members.read().await.keys().cloned().collect(); '
                                   'persist().await;'))
        self.assertFalse(self.scan('let plans = swarm_plans.read().await; drop(plans); '
                                   'let members = swarm_members.read().await;'))

    def test_test_module_does_not_hide_later_production(self):
        code = '#[cfg(test)] mod tests { async fn test() {} }\n' \
               'async fn production() { let plans = swarm_plans.read().await; '
        code += 'let members = swarm_members.read().await; }'
        self.assertTrue(check('fixture.rs', code))


if __name__ == '__main__':
    unittest.main()
