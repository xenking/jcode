use super::*;
use std::cell::RefCell;

const CLI_SHA: &str = "abcdef1234567890000000000000000000000000";
const DESKTOP_SHA: &str = "7654321fedcba000000000000000000000000000";

fn pair() -> Pair {
    Pair {
        cli: CLI_SHA.into(),
        desktop: DESKTOP_SHA.into(),
    }
}

struct Fake {
    _receipts: tempfile::TempDir,
    layout: Layout,
    arch: String,
    free: u64,
    built: bool,
    packaged: bool,
    mutations: RefCell<Vec<String>>,
    submitted: Option<(Pair, PathBuf, Option<PathBuf>, bool, Duration)>,
}

impl Fake {
    fn new() -> Self {
        let receipts = tempfile::tempdir().unwrap();
        acceptance::write_fixture(receipts.path(), &pair(), None);
        Self {
            layout: Layout {
                jcode_repo: "/repos/jcode".into(),
                desktop_repo: "/repos/jcode-desktop".into(),
                jcode_worktree: "/wt/jcode".into(),
                desktop_worktree: "/wt/jcode-desktop".into(),
                target_dir: "/target".into(),
                dist_dir: "/dist".into(),
                app: "/fake/Applications/Jcode.app".into(),
                builds: "/home/builds".into(),
                acceptance_dir: receipts.path().to_owned(),
            },
            _receipts: receipts,
            arch: "arm64".into(),
            free: 100,
            built: false,
            packaged: false,
            mutations: RefCell::new(Vec::new()),
            submitted: None,
        }
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
    fn resolve(&mut self, repo: Repo, _reference: &str, fetch: bool) -> Result<String> {
        if fetch {
            self.mutations.borrow_mut().push("fetch".into());
        }
        Ok(match repo {
            Repo::Jcode => CLI_SHA,
            Repo::Desktop => DESKTOP_SHA,
        }
        .into())
    }
    fn commits_between(&self, _repo: Repo, _from: &str, _to: &str) -> Option<usize> {
        Some(2)
    }
    fn deployed(&self) -> Deployed {
        Deployed {
            cli_current: Some("123456789".into()),
            cli_shared: Some("123456789".into()),
            daemon_label: Some("123456789".into()),
            daemon_pid: Some(1234),
            ..Deployed::default()
        }
    }
    fn busy_sessions(&self) -> Result<Vec<String>> {
        Ok(vec!["session_otter_1".into()])
    }
    fn workers(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![("session_otter_1".into(), "running".into())])
    }
    fn built_cli(&self, _pair: &Pair) -> Option<PathBuf> {
        self.built.then(|| "/target/jcode".into())
    }
    fn build_cli(&mut self, _pair: &Pair) -> Result<PathBuf> {
        self.mutations.borrow_mut().push("build".into());
        self.built = true;
        Ok("/target/jcode".into())
    }
    fn packaged_app(&self, _pair: &Pair) -> Option<PathBuf> {
        self.packaged.then(|| "/dist/Jcode.app".into())
    }
    fn package_desktop(&mut self, _pair: &Pair) -> Result<PathBuf> {
        self.mutations.borrow_mut().push("package".into());
        self.packaged = true;
        Ok("/dist/Jcode.app".into())
    }
    fn submit(
        &mut self,
        pair: &Pair,
        binary: &Path,
        app: Option<&Path>,
        options: &RolloutOptions,
    ) -> Result<String> {
        self.mutations.borrow_mut().push("submit".into());
        self.submitted = Some((
            pair.clone(),
            binary.to_path_buf(),
            app.map(Path::to_path_buf),
            options.now,
            options.drain_timeout,
        ));
        Ok("rollout_fixture".into())
    }
}

#[test]
fn paired_rollout_refuses_missing_blocked_and_wrong_pair_receipts_before_build() {
    let mut host = Fake::new();
    let dir = host.layout.acceptance_dir.clone();
    let receipt = acceptance::require(&host.layout, &pair()).unwrap();
    let dry_run = RolloutOptions {
        dry_run: true,
        ..RolloutOptions::default()
    };

    std::fs::remove_file(&receipt).unwrap();
    let err = run(&mut host, &dry_run, &mut Vec::new())
        .unwrap_err()
        .to_string();
    assert!(err.contains("rollout refused") && err.contains("scripts/accept-smoke.py"));
    assert!(err.contains(CLI_SHA) && err.contains(DESKTOP_SHA));
    assert!(host.mutations.borrow().is_empty());

    acceptance::write_fixture(&dir, &pair(), Some("ask_answer"));
    let err = run(&mut host, &RolloutOptions::default(), &mut Vec::new()).unwrap_err();
    assert!(format!("{err:#}").contains("required flow ask_answer did not pass"));
    assert_eq!(*host.mutations.borrow(), ["fetch", "fetch"]);
    assert!(host.submitted.is_none());

    let wrong = Pair {
        cli: DESKTOP_SHA.into(),
        desktop: CLI_SHA.into(),
    };
    let wrong_path = acceptance::write_fixture(&dir, &wrong, None);
    std::fs::copy(wrong_path, &receipt).unwrap();
    let err = run(&mut host, &dry_run, &mut Vec::new()).unwrap_err();
    assert!(format!("{err:#}").contains("SHA pair does not match"));

    acceptance::write_fixture(&dir, &pair(), None);
    assert_eq!(
        run(&mut host, &dry_run, &mut Vec::new()).unwrap(),
        Outcome::Planned
    );
}

#[test]
fn explicit_emergency_override_is_visible_without_a_receipt() {
    let mut host = Fake::new();
    std::fs::remove_file(acceptance::require(&host.layout, &pair()).unwrap()).unwrap();
    let opts = RolloutOptions {
        dry_run: true,
        skip_acceptance_smoke: true,
        ..RolloutOptions::default()
    };
    let mut out = Vec::new();
    assert_eq!(run(&mut host, &opts, &mut out).unwrap(), Outcome::Planned);
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("WARN") && text.contains("--skip-acceptance-smoke"));
    assert!(text.contains(CLI_SHA) && text.contains(DESKTOP_SHA));
    assert!(host.mutations.borrow().is_empty());
}

#[test]
fn dry_run_prints_every_phase_without_build_submit_or_host_mutations() {
    let mut host = Fake::new();
    let mut out = Vec::new();
    let outcome = run(
        &mut host,
        &RolloutOptions {
            dry_run: true,
            ..RolloutOptions::default()
        },
        &mut out,
    )
    .unwrap();
    assert_eq!(outcome, Outcome::Planned);
    assert!(host.mutations.borrow().is_empty());
    let text = String::from_utf8(out).unwrap();
    for phase in PHASES {
        assert!(text.contains(phase), "missing {phase}: {text}");
    }
    assert!(text.contains("atomic exchange"));
    assert!(text.contains("daemon journal"));
}

#[test]
fn caller_only_prepares_artifacts_then_submits_durable_intent() {
    let mut host = Fake::new();
    let mut out = Vec::new();
    assert_eq!(
        run(&mut host, &RolloutOptions::default(), &mut out).unwrap(),
        Outcome::Submitted("rollout_fixture".into())
    );
    assert_eq!(
        *host.mutations.borrow(),
        ["fetch", "fetch", "build", "package", "submit"]
    );
    let (pair, binary, bundle, now, timeout) = host.submitted.unwrap();
    assert_eq!(pair.label(), "abcdef123");
    assert_eq!(binary, Path::new("/target/jcode"));
    assert_eq!(bundle.as_deref(), Some(Path::new("/dist/Jcode.app")));
    assert!(!now, "drain remains the default");
    assert_eq!(timeout, Duration::from_secs(900));
    assert!(String::from_utf8(out).unwrap().contains("caller may exit"));
}

#[test]
fn now_is_allowed_and_cli_does_not_wait_for_busy_workers() {
    let mut host = Fake::new();
    run(
        &mut host,
        &RolloutOptions {
            now: true,
            ..RolloutOptions::default()
        },
        &mut Vec::new(),
    )
    .unwrap();
    assert!(host.submitted.unwrap().3);
}

#[test]
fn no_desktop_submits_no_bundle_and_reuses_existing_binary() {
    let mut host = Fake::new();
    host.built = true;
    run(
        &mut host,
        &RolloutOptions {
            desktop: false,
            ..RolloutOptions::default()
        },
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(*host.mutations.borrow(), ["fetch", "fetch", "submit"]);
    assert!(host.submitted.unwrap().2.is_none());
}

#[test]
fn refuses_low_disk_and_wrong_arch_before_any_mutation() {
    let mut host = Fake::new();
    host.free = 29;
    assert!(run(&mut host, &RolloutOptions::default(), &mut Vec::new()).is_err());
    assert!(host.mutations.borrow().is_empty());
    host.free = 100;
    host.arch = "x86_64".into();
    assert!(run(&mut host, &RolloutOptions::default(), &mut Vec::new()).is_err());
    assert!(host.mutations.borrow().is_empty());
}

#[test]
fn parses_versions_and_labels() {
    assert_eq!(
        parse_desktop_version("Jcode Desktop v0.7.0 (e97162a)"),
        Some(("v0.7.0".into(), "e97162a".into()))
    );
    assert_eq!(
        parse_desktop_version("Jcode Desktop v0.7.0 (c949d89, dirty)"),
        Some(("v0.7.0".into(), "c949d89".into()))
    );
    assert_eq!(
        version_label_of(
            Path::new("/builds/versions/abcdef123/jcode"),
            Path::new("/builds")
        ),
        Some("abcdef123".into())
    );
    assert!(same_commit("abcdef123", "abcdef123456"));
    assert!(!same_commit("", "abcdef123"));
}
