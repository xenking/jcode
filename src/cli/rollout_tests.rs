use super::*;
use std::cell::RefCell;

/// A machine in memory. Every mutating call is logged so a test can assert
/// exactly what the rollout did, and that a dry run did nothing.
struct Fake {
    layout: Layout,
    arch: String,
    free: u64,
    heads: (String, String),
    deployed: RefCell<Deployed>,
    /// Successive answers for busy_sessions; the last one repeats.
    busy: RefCell<Vec<Vec<String>>>,
    workers_before: Vec<(String, String)>,
    workers_after: Vec<(String, String)>,
    live_sessions: Vec<String>,
    reloaded: bool,
    built: bool,
    packaged: bool,
    reload_takes: bool,
    rollback_takes: bool,
    bridge_ok: bool,
    swap_sparkle: bool,
    state: Option<RolloutState>,
    calls: Vec<String>,
}

const CLI: &str = "2838f516c1234567";
const DESKTOP: &str = "b7adb60abcdef";

impl Fake {
    fn new() -> Self {
        Self {
            layout: Layout {
                jcode_repo: "/repo/jcode".into(),
                desktop_repo: "/repo/jcode-desktop".into(),
                jcode_worktree: "/wt/jcode".into(),
                desktop_worktree: "/wt/jcode-desktop".into(),
                target_dir: "/target".into(),
                dist_dir: "/wt/jcode-desktop/dist/macos".into(),
                app: "/Applications/Jcode.app".into(),
                app_backup: "/home/.jcode/migration/Jcode.app.prev".into(),
                builds: "/home/.jcode/builds".into(),
            },
            arch: "arm64".into(),
            free: 72,
            heads: (CLI.into(), DESKTOP.into()),
            deployed: RefCell::new(Deployed {
                cli_current: Some("22774c334".into()),
                cli_shared: Some("22774c334".into()),
                daemon_label: Some("22774c334".into()),
                daemon_pid: Some(84375),
                desktop_sha: Some("e97162a".into()),
                desktop_cli: Some("22774c334".into()),
                desktop_version: Some("v0.7.0".into()),
                sparkle_key: true,
                bridge_paths: vec!["/Applications/Jcode.app/Contents/MacOS/jcode".into()],
            }),
            busy: RefCell::new(vec![Vec::new()]),
            workers_before: vec![
                ("session_calf_1_a".into(), "running".into()),
                ("session_hare_1_a".into(), "ready".into()),
            ],
            workers_after: vec![
                ("session_calf_1_a".into(), "running".into()),
                ("session_hare_1_a".into(), "stopped".into()),
            ],
            live_sessions: vec!["session_calf_1_a".into(), "session_hare_1_a".into()],
            reloaded: false,
            built: false,
            packaged: false,
            reload_takes: true,
            rollback_takes: true,
            bridge_ok: true,
            swap_sparkle: true,
            state: None,
            calls: Vec::new(),
        }
    }

    fn mutations(&self) -> Vec<&str> {
        self.calls.iter().map(String::as_str).collect()
    }
}

impl Host for Fake {
    fn layout(&self) -> &Layout {
        &self.layout
    }
    fn arch(&self) -> String {
        self.arch.clone()
    }
    fn free_gb(&self) -> Result<u64> {
        Ok(self.free)
    }
    fn resolve(&mut self, repo: Repo, _refname: &str, fetch: bool) -> Result<String> {
        if fetch {
            self.calls.push(format!("fetch {repo:?}"));
        }
        Ok(match repo {
            Repo::Jcode => self.heads.0.clone(),
            Repo::Desktop => self.heads.1.clone(),
        })
    }
    fn commits_between(&self, _repo: Repo, _from: &str, _to: &str) -> Option<usize> {
        Some(3)
    }
    fn deployed(&self) -> Deployed {
        self.deployed.borrow().clone()
    }
    fn busy_sessions(&self) -> Result<Vec<String>> {
        let mut busy = self.busy.borrow_mut();
        let ids = if busy.len() > 1 {
            busy.remove(0)
        } else {
            busy[0].clone()
        };
        Ok(ids
            .into_iter()
            .filter(|id| self.live_sessions.contains(id))
            .collect())
    }
    fn workers(&self) -> Result<Vec<(String, String)>> {
        let workers = if self.reloaded {
            self.workers_after.clone()
        } else {
            self.workers_before.clone()
        };
        Ok(workers
            .into_iter()
            .filter(|(id, _)| self.live_sessions.contains(id))
            .collect())
    }
    fn built_cli(&self, _pair: &Pair) -> Option<PathBuf> {
        self.built.then(|| PathBuf::from("/target/release/jcode"))
    }
    fn build_cli(&mut self, pair: &Pair) -> Result<PathBuf> {
        self.calls.push(format!("build {}", pair.label()));
        self.built = true;
        Ok("/target/release/jcode".into())
    }
    fn packaged_app(&self, _pair: &Pair) -> Option<PathBuf> {
        self.packaged
            .then(|| self.layout.dist_dir.join("Jcode.app"))
    }
    fn package_desktop(&mut self, pair: &Pair) -> Result<PathBuf> {
        self.calls.push(format!(
            "package {} against {}",
            pair.desktop_short(),
            pair.label()
        ));
        self.packaged = true;
        Ok(self.layout.dist_dir.join("Jcode.app"))
    }
    fn stage(&mut self, label: &str, _binary: &Path) -> Result<()> {
        self.calls.push(format!("stage {label}"));
        let mut deployed = self.deployed.borrow_mut();
        deployed.cli_current = Some(label.into());
        deployed.cli_shared = Some(label.into());
        Ok(())
    }
    fn reload(&mut self, label: &str) -> Result<()> {
        self.calls.push(format!("reload {label}"));
        self.reloaded = true;
        if self.reload_takes {
            let mut deployed = self.deployed.borrow_mut();
            deployed.daemon_label = Some(label.into());
            deployed.daemon_pid = Some(90001);
        }
        Ok(())
    }
    fn resume_worker(&mut self, session_id: &str) -> Result<()> {
        self.calls.push(format!("resume {session_id}"));
        Ok(())
    }
    fn swap_desktop(&mut self, _app: &Path) -> Result<()> {
        self.calls.push("swap".into());
        let mut deployed = self.deployed.borrow_mut();
        deployed.desktop_sha = Some(DESKTOP[..7].into());
        deployed.desktop_cli = Some(CLI[..9].into());
        deployed.sparkle_key = self.swap_sparkle;
        Ok(())
    }
    fn bridge_search(&self) -> Result<()> {
        if self.bridge_ok {
            Ok(())
        } else {
            bail!("bridge did not answer")
        }
    }
    fn rollback_cli(&mut self, current: &str, shared: &str) -> Result<()> {
        self.calls.push(format!("rollback cli {current} {shared}"));
        let mut deployed = self.deployed.borrow_mut();
        deployed.cli_current = Some(current.into());
        deployed.cli_shared = Some(shared.into());
        if self.rollback_takes {
            deployed.daemon_label = Some(shared.into());
        }
        Ok(())
    }
    fn rollback_desktop(&mut self) -> Result<()> {
        self.calls.push("rollback desktop".into());
        Ok(())
    }
    fn sleep(&mut self, _duration: Duration) {}
    fn load_state(&self) -> Option<RolloutState> {
        self.state.clone()
    }
    fn save_state(&mut self, state: &RolloutState) -> Result<()> {
        self.state = Some(state.clone());
        Ok(())
    }
}

fn rollout(fake: &mut Fake, opts: &RolloutOptions) -> (Result<Outcome>, String) {
    let mut out = Vec::new();
    let outcome = run(fake, opts, &mut out);
    (outcome, String::from_utf8(out).unwrap())
}

fn opts() -> RolloutOptions {
    RolloutOptions::default()
}

#[test]
fn dry_run_prints_every_phase_and_changes_nothing() {
    let mut fake = Fake::new();
    fake.busy = RefCell::new(vec![vec!["session_calf_1_a".into()]]);
    let (outcome, output) = rollout(
        &mut fake,
        &RolloutOptions {
            dry_run: true,
            ..opts()
        },
    );
    assert_eq!(outcome.unwrap(), Outcome::Planned);
    assert!(fake.mutations().is_empty(), "{:?}", fake.calls);
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(lines.len(), PHASES.len() + 1, "{output}");
    for (index, phase) in PHASES.iter().enumerate() {
        assert!(
            lines[index].starts_with(&format!("[{}/8] {phase}", index + 1)),
            "{}",
            lines[index]
        );
    }
    assert!(lines[0].contains("jcode 2838f516c"), "{}", lines[0]);
    assert!(lines[0].contains("desktop b7adb60"), "{}", lines[0]);
    assert!(lines[0].contains("mid-turn: calf"), "{}", lines[0]);
    assert!(
        lines[2].contains("JCODE_REPO=/wt/jcode"),
        "Desktop must build against the same jcode worktree: {}",
        lines[2]
    );
    assert!(lines[2].contains("PACKAGE_TARGETS=aarch64-apple-darwin"));
    assert!(lines[3].contains("no reload"), "{}", lines[3]);
    assert!(lines[5].contains("workers live now: calf"), "{}", lines[5]);
    assert_eq!(lines[8], "dry run: nothing changed");
}

#[test]
fn stale_snapshot_worker_is_not_reported_as_busy_or_live() {
    let mut fake = Fake::new();
    fake.workers_before = vec![("session_mushroom_1_a".into(), "running".into())];
    fake.busy = RefCell::new(vec![vec!["session_mushroom_1_a".into()]]);
    fake.live_sessions.clear();
    let (outcome, output) = rollout(
        &mut fake,
        &RolloutOptions {
            dry_run: true,
            ..opts()
        },
    );
    assert_eq!(outcome.unwrap(), Outcome::Planned);
    assert!(
        !output.contains("mushroom"),
        "stale worker leaked into plan: {output}"
    );
}

#[test]
fn refuses_low_disk_and_non_arm64_before_touching_anything() {
    let mut fake = Fake::new();
    fake.free = 24;
    let (outcome, output) = rollout(&mut fake, &opts());
    assert!(outcome.unwrap_err().to_string().contains("24G free"));
    assert!(output.contains("FAIL"));
    assert!(fake.mutations().is_empty());

    let mut fake = Fake::new();
    fake.arch = "x86_64".into();
    assert!(rollout(&mut fake, &opts()).0.is_err());
    assert!(fake.mutations().is_empty());

    // A dry run still shows the whole plan, and says where a real run stops.
    let mut fake = Fake::new();
    fake.free = 15;
    let (outcome, output) = rollout(
        &mut fake,
        &RolloutOptions {
            dry_run: true,
            ..opts()
        },
    );
    assert_eq!(outcome.unwrap(), Outcome::Planned);
    assert!(output.starts_with("[1/8] plan            FAIL"), "{output}");
    assert!(
        output.ends_with(
            "a real run would stop at plan: refusing: 15G free, need 25G for a release build\n"
        ),
        "{output}"
    );
    assert!(fake.mutations().is_empty());
}

#[test]
fn full_rollout_runs_phases_in_order_and_resumes_lost_workers() {
    let mut fake = Fake::new();
    let (outcome, output) = rollout(&mut fake, &opts());
    assert_eq!(outcome.unwrap(), Outcome::Deployed, "{output}");
    assert_eq!(
        fake.mutations(),
        [
            "fetch Jcode",
            "fetch Desktop",
            "build 2838f516c",
            "package b7adb60 against 2838f516c",
            "stage 2838f516c",
            "reload 2838f516c",
            "swap",
        ],
        "{output}"
    );
    // calf was running before and is running after; hare was idle. Nothing
    // was lost, so nothing is resumed by hand.
    assert!(output.contains("workers restored 1/1"), "{output}");
    assert_eq!(fake.state.as_ref().unwrap().done.len(), PHASES.len());
    for line in output.lines() {
        assert!(line.starts_with('['), "one status line per phase: {line}");
    }
}

#[test]
fn a_worker_the_reload_left_stopped_is_resumed() {
    let mut fake = Fake::new();
    fake.workers_after = vec![("session_calf_1_a".into(), "stopped".into())];
    let (outcome, output) = rollout(&mut fake, &opts());
    assert_eq!(outcome.unwrap(), Outcome::Deployed, "{output}");
    assert!(fake.mutations().contains(&"resume session_calf_1_a"));
    assert!(output.contains("resumed: calf"), "{output}");
}

#[test]
fn rerun_after_success_is_a_no_op() {
    let mut fake = Fake::new();
    assert_eq!(rollout(&mut fake, &opts()).0.unwrap(), Outcome::Deployed);
    fake.calls.clear();
    let (outcome, output) = rollout(&mut fake, &opts());
    assert_eq!(outcome.unwrap(), Outcome::Deployed, "{output}");
    assert_eq!(
        fake.mutations(),
        ["fetch Jcode", "fetch Desktop"],
        "{output}"
    );
    assert_eq!(output.matches(" skip ").count(), 5, "{output}");
}

#[test]
fn drain_waits_names_who_and_refuses_on_timeout() {
    let mut fake = Fake::new();
    fake.busy = RefCell::new(vec![
        // The plan line reads it once, then drain polls.
        vec!["session_calf_1_a".into(), "session_hare_1_a".into()],
        vec!["session_calf_1_a".into(), "session_hare_1_a".into()],
        vec!["session_calf_1_a".into()],
        Vec::new(),
    ]);
    let (outcome, output) = rollout(&mut fake, &opts());
    assert_eq!(outcome.unwrap(), Outcome::Deployed, "{output}");
    assert!(
        output.contains("drain           wait  calf, hare"),
        "{output}"
    );
    assert!(
        output.contains("drain           ok    idle after 10s"),
        "{output}"
    );

    let mut fake = Fake::new();
    fake.busy = RefCell::new(vec![vec!["session_calf_1_a".into()]]);
    let (outcome, output) = rollout(
        &mut fake,
        &RolloutOptions {
            drain_timeout: Duration::from_secs(10),
            ..opts()
        },
    );
    let error = outcome.unwrap_err().to_string();
    assert!(error.contains("calf still mid-turn"), "{error}");
    assert!(error.contains("--now"), "{error}");
    assert!(
        !fake
            .mutations()
            .iter()
            .any(|call| call.starts_with("reload"))
    );
    assert!(output.contains("drain           FAIL"), "{output}");
}

#[test]
fn now_reloads_without_waiting() {
    let mut fake = Fake::new();
    fake.busy = RefCell::new(vec![vec!["session_calf_1_a".into()]]);
    let (outcome, output) = rollout(
        &mut fake,
        &RolloutOptions {
            now: true,
            ..opts()
        },
    );
    assert_eq!(outcome.unwrap(), Outcome::Deployed, "{output}");
    assert!(
        output.contains("drain           skip  --now with 1 mid-turn (calf)"),
        "{output}"
    );
}

#[test]
fn a_reload_that_does_not_take_fails_before_the_desktop_swap() {
    let mut fake = Fake::new();
    fake.reload_takes = false;
    let (outcome, output) = rollout(&mut fake, &opts());
    assert!(outcome.is_err());
    assert!(output.contains("reload          FAIL"), "{output}");
    assert!(!fake.mutations().contains(&"swap"));
}

#[test]
fn failed_verify_rolls_back_cli_and_desktop() {
    let mut fake = Fake::new();
    fake.bridge_ok = false;
    fake.swap_sparkle = false;
    let (outcome, output) = rollout(&mut fake, &opts());
    match outcome.unwrap() {
        Outcome::RolledBack(reason) => {
            assert!(reason.contains("search_sessions"), "{reason}");
            assert!(reason.contains("SUPublicEDKey"), "{reason}");
        }
        other => panic!("expected rollback, got {other:?}"),
    }
    let calls = fake.mutations();
    assert!(
        calls.contains(&"rollback cli 22774c334 22774c334"),
        "{calls:?}"
    );
    assert!(calls.contains(&"rollback desktop"), "{calls:?}");
    assert!(
        output.contains("verify          back  rolled back"),
        "{output}"
    );
}

#[test]
fn rollback_must_not_restore_old_desktop_if_daemon_did_not_downgrade() {
    let mut fake = Fake::new();
    fake.bridge_ok = false;
    fake.rollback_takes = false;
    let (outcome, output) = rollout(&mut fake, &opts());
    assert!(
        outcome.is_err(),
        "rollback must reject the new daemon: {output}"
    );
    assert!(output.contains("rollback FAILED"), "{output}");
    assert!(!fake.mutations().contains(&"rollback desktop"));
}

#[test]
fn verify_catches_a_stale_bundle_bridge() {
    let fake = Fake::new();
    let pair = Pair {
        cli: CLI.into(),
        desktop: DESKTOP.into(),
    };
    {
        let mut deployed = fake.deployed.borrow_mut();
        deployed.daemon_label = Some(pair.label().into());
        deployed.cli_current = Some(pair.label().into());
        deployed.desktop_sha = Some("b7adb60".into());
        deployed.desktop_cli = Some("2838f516c".into());
        deployed.bridge_paths = vec![
            "/Applications/Jcode.app/Contents/MacOS/jcode".into(),
            "/Users/x/.jcode/migration/Jcode.app.prev/Contents/MacOS/jcode-harness-api-bridge"
                .into(),
        ];
    }
    let layout = fake.layout.clone();
    let failures = verify(&fake, &pair, true, &layout);
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("stale bundle bridge"), "{failures:?}");
}

#[test]
fn parses_versions_and_labels() {
    assert_eq!(
        parse_desktop_version(
            "jcode desktop: runtime dir ... using /tmp\nJcode Desktop v0.7.0 (e97162a)\n"
        ),
        Some(("v0.7.0".into(), "e97162a".into()))
    );
    // Packaging rewrites Cargo.lock, so the bundle reports `dirty`; the
    // commit is still the one before the comma.
    assert_eq!(
        parse_desktop_version("Jcode Desktop v0.7.0 (c949d89, dirty)\n"),
        Some(("v0.7.0".into(), "c949d89".into()))
    );
    assert_eq!(parse_desktop_version("garbage"), None);
    assert_eq!(
        version_label_of(
            Path::new("/home/.jcode/builds/versions/22774c334/jcode"),
            Path::new("/home/.jcode/builds")
        ),
        Some("22774c334".into())
    );
    assert_eq!(
        version_label_of(
            Path::new("/usr/bin/jcode"),
            Path::new("/home/.jcode/builds")
        ),
        None
    );
    assert!(same_commit("2838f516c", "2838f516c1234567"));
    assert!(!same_commit("2838f516c", "22774c334"));
    assert!(!same_commit("", "abc"));
    assert_eq!(friendly("session_calf_1794_abc"), "calf");
    assert_eq!(friendly("odd"), "odd");
}
