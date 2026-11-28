//! The facade must never persist, spawn or report anything.

use jcode_telemetry_core as telemetry;
use std::time::Duration;

fn thread_count() -> usize {
    #[cfg(target_os = "linux")]
    return std::fs::read_dir("/proc/self/task").unwrap().count();
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-M", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).lines().count()
    }
}

#[test]
fn entry_points_neither_write_files_nor_start_threads() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in the binary, so nothing reads the environment concurrently.
    unsafe { std::env::set_var("JCODE_HOME", home.path()) };
    let threads_before = thread_count();

    telemetry::record_install_if_first_run();
    telemetry::record_desktop_active("0.7.0");
    telemetry::record_desktop_update("0.7.0", "0.7.1", "app", "success", None);
    telemetry::record_upgrade_if_needed();
    telemetry::begin_session_with_parent("openai", "gpt", Some("parent".into()), false);
    telemetry::begin_resumed_session("openai", "gpt");
    telemetry::record_turn();
    telemetry::record_tool_call();
    telemetry::record_token_usage(1, 2, Some(3), None);
    telemetry::record_provider_usage_with_model_attribution(
        Some("s"),
        "openai",
        "gpt",
        None,
        telemetry::UsageSource::Agent,
        telemetry::ProviderUsage::default(),
    );
    telemetry::record_feedback("text");
    telemetry::record_error(telemetry::ErrorCategory::RateLimited);
    let mut guard = telemetry::begin_concurrency_session("s", None);
    guard.finish_background();
    guard.finish();
    telemetry::end_session_with_reason("openai", "gpt", telemetry::SessionEndReason::NormalExit);
    telemetry::flush_pending(Duration::from_secs(1));

    assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
    assert_eq!(thread_count(), threads_before);

    // Values callers branch on keep their control flow on the "disabled" path.
    assert!(!telemetry::is_enabled());
    assert!(!telemetry::content_sharing_enabled());
    assert!(!telemetry::opt_out_forced_by_env());
    assert!(!guard.is_active());
    assert_eq!(guard.session_id(), "s");
    assert_eq!(telemetry::current_provider_model(), None);
    assert!(!telemetry::record_transcript(
        "p",
        "m",
        telemetry::SessionEndReason::NormalExit,
        "x".into()
    ));
    let status = telemetry::status();
    assert!(!status.enabled && status.telemetry_id.is_none() && status.opt_out_source.is_none());
    assert!(telemetry::set_usage_telemetry_enabled(false));
    assert!(!telemetry::set_usage_telemetry_enabled(true));
    assert!(telemetry::set_content_sharing_enabled(false));
}
