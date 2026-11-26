#![cfg(unix)]
//! Routing acceptance against a real daemon.
//!
//! The built `jcode` binary runs as a daemon in a private `JCODE_HOME` and
//! socket, against a fixture provider with two keys (`key-main`, the global
//! one, and `key-second`, labelled `second`). The fixture records which key
//! served each request, so every scenario asserts the order the accounts were
//! used in. No network beyond loopback, no real account, no shared daemon.

mod daemon;
mod fixture;
mod restart;
mod scenarios;

use daemon::{Daemon, Setup};
use fixture::Fixture;
use std::sync::{Mutex, MutexGuard};

/// One daemon at a time: they are heavy and the scenarios are not about load.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A started daemon over a fixture whose keys have each used 10%.
pub struct Rig {
    pub fx: Fixture,
    pub daemon: Daemon,
    _serial: MutexGuard<'static, ()>,
}

pub fn boot(setup: Setup, arrange: impl FnOnce(&Fixture)) -> Rig {
    let serial = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let fx = Fixture::start();
    fx.used("key-main", 10);
    fx.used("key-second", 10);
    arrange(&fx);
    let daemon = Daemon::new(fx.port, &setup);
    daemon.start();
    Rig {
        fx,
        daemon,
        _serial: serial,
    }
}
