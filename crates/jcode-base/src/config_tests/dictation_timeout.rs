//! The dictation timeout migration for configs saved by older releases.

use super::*;

#[test]
fn migrate_legacy_dictation_timeout_raises_ninety_once() {
    let _guard = crate::storage::lock_test_env();
    let prev_home = std::env::var_os("JCODE_HOME");
    let dir = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir.path());

    let config_path = dir.path().join("config.toml");
    let original = "[provider]\nstream_idle_timeout_secs = 90\n\n[dictation]\nmode = \"insert\"\ntimeout_secs = 90\nrecorder = \"\"\n";
    std::fs::write(&config_path, original).expect("write config");

    assert!(Config::migrate_legacy_dictation_timeout_once());
    let migrated = std::fs::read_to_string(&config_path).expect("read config");
    let parsed: Config = toml::from_str(&migrated).expect("migrated config parses");
    assert_eq!(parsed.dictation.timeout_secs, 300);
    // Only the dictation line changed.
    assert!(migrated.contains("stream_idle_timeout_secs = 90"));
    assert!(migrated.contains("mode = \"insert\""));

    // A later explicit 90 is a choice and survives.
    std::fs::write(&config_path, "[dictation]\ntimeout_secs = 90\n").expect("write config");
    assert!(!Config::migrate_legacy_dictation_timeout_once());
    let content = std::fs::read_to_string(&config_path).expect("read config");
    assert!(content.contains("timeout_secs = 90"));

    // Other values are never touched.
    let dir2 = tempfile::TempDir::new().expect("tempdir");
    crate::env::set_var("JCODE_HOME", dir2.path());
    let kept = "[dictation]\ntimeout_secs = 120\n";
    std::fs::write(dir2.path().join("config.toml"), kept).expect("write config");
    assert!(!Config::migrate_legacy_dictation_timeout_once());
    assert_eq!(
        std::fs::read_to_string(dir2.path().join("config.toml")).unwrap(),
        kept
    );

    restore_env_var("JCODE_HOME", prev_home);
}
