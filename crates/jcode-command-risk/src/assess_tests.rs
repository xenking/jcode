//! End-to-end assessment tests: real command strings in, verdict out.
//!
//! The two failure modes that matter are opposite and both fatal:
//! under-firing loses a home directory (#604), over-firing trains users to
//! bypass the gate. Both directions are covered.

use super::*;
use std::path::PathBuf;

#[test]
fn directory_changes_keep_protected_fixture_paths_guarded() {
    let ctx = RiskContext {
        working_dir: Some("/fixture/project".into()),
        home_dir: Some("/fixture/person".into()),
        ..Default::default()
    };
    let command = "cd /fixture/person && rmdir Documents";
    assert_eq!(assess(command, &ctx).level, RiskLevel::Catastrophic);
    assert_eq!(
        assess("cd $UNRESOLVED && rmdir artifact", &ctx).level,
        RiskLevel::Confirm
    );
}

#[test]
fn redirects_are_not_destructive_command_operands() {
    let ctx = RiskContext {
        working_dir: Some("/fixture/project".into()),
        ..Default::default()
    };
    for redirect in [
        "2>/dev/null",
        "2>>/dev/null",
        "< /dev/null",
        "2>&1",
        "2>& 1",
        "&>/dev/null",
        "&>>/dev/null",
    ] {
        let assessment = assess(&format!("rm -r artifact {redirect}"), &ctx);
        assert_eq!(assessment.level, RiskLevel::Low, "{redirect}");
        assert_eq!(assessment.findings.len(), 1, "{redirect}: {assessment:?}");
    }
    for operand in ["2 ", "'2'", "\\2"] {
        assert_eq!(
            assess(&format!("rm -r artifact {operand}>/dev/null"), &ctx)
                .findings
                .len(),
            2
        );
    }
    assert_eq!(
        assess("echo line >& /dev/fixture", &ctx).level,
        RiskLevel::Catastrophic
    );
}

fn ctx() -> RiskContext {
    RiskContext {
        working_dir: Some(PathBuf::from("/home/u/proj")),
        home_dir: Some(PathBuf::from("/home/u")),
        scratch_dir: None,
        temp_dir: None,
    }
}

/// The context a real agent session runs with: a scratch directory and a
/// private TMPDIR, both disposable.
fn ctx_with_scratch() -> RiskContext {
    RiskContext {
        working_dir: Some(PathBuf::from("/home/u/proj")),
        home_dir: Some(PathBuf::from("/home/u")),
        scratch_dir: Some(PathBuf::from("/home/u/.jcode/scratch")),
        temp_dir: Some(PathBuf::from("/var/folders/xy/T")),
    }
}

fn level_in(ctx: &RiskContext, command: &str) -> RiskLevel {
    assess(command, ctx).level
}

fn level(command: &str) -> RiskLevel {
    assess(command, &ctx()).level
}

#[test]
fn issue_922_heredoc_payload_does_not_trip_the_gate() {
    for command in [
        "cat >> log.md <<'EOF'\nO(n^2) time and O(n) space\nEOF",
        "cat > helper.sh <<'EOF'\n#!/bin/bash\nrm -f \"$SOME_VAR\"\nEOF",
    ] {
        assert!(level(command).runs_immediately(), "{command:?}");
    }

    assert_eq!(
        level("cat <<'EOF'\nrm -rf ~\nEOF\nrm -rf ~"),
        RiskLevel::Catastrophic
    );
}

#[test]
fn heredoc_inside_double_quoted_command_substitution_is_data() {
    // The common "pass a multiline body to a flag" idiom. The body is quoted
    // heredoc data, so backticks, quotes and `>` inside it are prose.
    for command in [
        "echo \"$(cat <<'EOF'\n`\"p>\"` y\nEOF\n)\"",
        "gh api repos/o/r/pulls -f body=\"$(cat <<'EOF'\nRan `tool resume X \"<pointer>\"` ok.\nEOF\n)\"",
        "git commit -m \"$(cat <<'EOF'\nSummary with `rm -rf ~` in prose\nEOF\n)\"",
    ] {
        assert!(level(command).runs_immediately(), "{command:?}");
    }

    // A real command after the substitution is still assessed.
    assert_eq!(
        level("echo \"$(cat <<'EOF'\ninert\nEOF\n)\"; rm -rf ~"),
        RiskLevel::Catastrophic
    );
    // `<<` inside single quotes is still not a heredoc.
    assert_eq!(
        level("echo '$(cat <<EOF'\nrm -rf ~\nEOF"),
        RiskLevel::Catastrophic
    );
}

#[test]
fn the_issue_604_command_is_blocked_outright() {
    // The reported incident: "jcode just deleted everything in my ~".
    for command in [
        "rm -rf ~",
        "rm -rf $HOME",
        "rm -rf \"$HOME\"",
        "rm -rf ${HOME}",
        "rm -rf /home/u",
        "rm -fr ~/",
        "/bin/rm -rf ~",
        "rm -rf ~/*",
    ] {
        assert_eq!(
            level(command),
            RiskLevel::Catastrophic,
            "{command:?} must be denied outright"
        );
    }
}

#[test]
fn root_destruction_is_blocked_outright() {
    for command in [
        "rm -rf /",
        "rm -rf /*",
        "rm -rf /etc",
        "rm --recursive --force /usr",
    ] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command:?}");
    }
}

#[test]
fn credential_theft_or_destruction_is_blocked() {
    for command in ["rm -rf ~/.ssh", "shred ~/.gnupg", "rm -rf ~/.aws"] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command:?}");
    }
}

#[test]
fn chaining_does_not_bypass_the_gate() {
    // The dangerous half is second; the first half is innocuous.
    assert_eq!(level("echo starting && rm -rf ~"), RiskLevel::Catastrophic);
    assert_eq!(level("cd /tmp; rm -rf $HOME"), RiskLevel::Catastrophic);
    assert_eq!(level("true || rm -rf /"), RiskLevel::Catastrophic);
}

#[test]
fn routine_cleanup_inside_the_project_is_not_blocked() {
    // If these prompt, the gate is useless because everyone learns to dismiss it.
    for command in [
        "rm -rf target",
        "rm -rf ./node_modules",
        "rm -f Cargo.lock",
        "rm -rf /home/u/proj/build",
        "rm -rf /tmp/scratch",
        "cargo clean",
        "ls -la",
        "git status",
        "echo hello > out.txt",
    ] {
        assert!(
            level(command).runs_immediately(),
            "{command:?} should run without a reflection turn, got {:?}",
            level(command)
        );
    }
}

#[test]
fn deleting_concrete_paths_outside_the_project_runs_immediately() {
    for command in ["rm -rf /home/u/other-project", "rm -rf /srv/data"] {
        assert_eq!(level(command), RiskLevel::Low, "{command:?}");
    }
}

#[test]
fn non_rm_destructive_tools_are_covered() {
    // A name-based denylist would miss all of these.
    assert_eq!(level("find /home/u -delete"), RiskLevel::Catastrophic);
    assert!(!level("dd if=/dev/zero of=/dev/sda").runs_immediately());
    assert_eq!(level("shred /home/u/other/secrets.txt"), RiskLevel::Low);
}

#[test]
fn concrete_truncating_redirect_outside_cwd_runs_immediately() {
    assert_eq!(
        level("echo '' > /home/u/other/important.conf"),
        RiskLevel::Low
    );
}

#[test]
fn appending_is_not_treated_as_destructive() {
    assert!(level("echo line >> /home/u/other/log.txt").runs_immediately());
}

#[test]
fn runtime_computed_targets_require_justification() {
    // We cannot see what $TARGET holds, so we must not assume it is safe.
    assert!(level("rm -rf $TARGET") >= RiskLevel::Confirm);
    assert!(level("rm -rf $(cat list.txt)") >= RiskLevel::Confirm);
}

#[test]
fn destructive_command_without_a_visible_target_escalates() {
    // Unknown blast radius is a reason to ask, not to proceed.
    assert!(level("rm -rf") >= RiskLevel::Confirm);
}

#[test]
fn assessment_is_total_over_garbage_input() {
    let ctx = ctx();
    for command in ["", "   ", "'", "\\", ";;;", "&&", "rm -rf \"unterminated"] {
        let _ = assess(command, &ctx);
    }
}

#[test]
fn explanation_names_the_offending_target() {
    let assessment = assess("rm -rf ~", &ctx());
    let text = assessment.explanation();
    assert!(
        text.contains("/home/u"),
        "explanation should name the path: {text}"
    );
}

#[test]
fn catastrophic_cannot_be_unlocked_but_confirm_can() {
    assert!(RiskLevel::Catastrophic.is_absolute_deny());
    assert!(!RiskLevel::Confirm.is_absolute_deny());
    assert!(!RiskLevel::Confirm.runs_immediately());
    assert!(RiskLevel::Safe.runs_immediately());
    assert!(RiskLevel::Low.runs_immediately());
}

#[test]
fn severity_ordering_is_monotonic() {
    // from_findings takes the max, so the ordering must be correct.
    assert!(RiskLevel::Safe < RiskLevel::Low);
    assert!(RiskLevel::Low < RiskLevel::Confirm);
    assert!(RiskLevel::Confirm < RiskLevel::Catastrophic);
}

#[test]
fn the_worst_finding_in_a_chain_wins() {
    // A Low finding must not mask a Catastrophic one in the same command.
    let assessment = assess("rm -rf target && rm -rf ~", &ctx());
    assert_eq!(assessment.level, RiskLevel::Catastrophic);
    assert!(assessment.findings.len() >= 2);
}

/// State-space sweep: every combination of a destructive verb, a recursive
/// flag spelling, and a way of naming the home directory must be denied.
///
/// Hand-written cases encode the failures we already thought of. This encodes
/// the ones we did not: 6 verbs x 5 flags x 11 spellings = 330 commands.
#[test]
fn no_combination_of_verb_flag_and_home_spelling_slips_through() {
    let ctx = ctx();
    let verbs = ["rm", "/bin/rm", "shred", "srm", "rmdir", "unlink"];
    let flags = ["-rf", "-fr", "-r -f", "--recursive --force", "-Rf"];
    let homes = [
        "~",
        "~/",
        "$HOME",
        "${HOME}",
        "\"$HOME\"",
        "'/home/u'",
        "/home/u",
        "/home/u/",
        "/home/u/.",
        "/home/u/x/..",
        "/home/./u",
    ];

    let mut escaped = Vec::new();
    for verb in verbs {
        for flag in flags {
            for home in homes {
                let command = format!("{verb} {flag} {home}");
                if assess(&command, &ctx).level != RiskLevel::Catastrophic {
                    escaped.push(command);
                }
            }
        }
    }
    assert!(
        escaped.is_empty(),
        "{} command(s) reached the home directory ungated: {:#?}",
        escaped.len(),
        escaped
    );
}

/// The inverse sweep: ordinary in-project work must never be interrupted.
#[test]
fn no_routine_project_command_triggers_a_prompt() {
    let ctx = ctx();
    let commands = [
        "rm -rf target",
        "rm -rf ./target",
        "rm -rf build dist",
        "rm -f a.txt b.txt",
        "rm -rf src/generated",
        "rm -rf /home/u/proj/.cache",
        "rm -rf /tmp/x",
        "git status",
        "cargo build",
        "npm ci",
        "mkdir -p out",
        "cp a b",
        "mv old new",
        "echo hi > out.txt",
        "cat Cargo.toml",
        "grep -r TODO src",
    ];
    let mut noisy = Vec::new();
    for command in commands {
        let level = assess(command, &ctx).level;
        if !level.runs_immediately() {
            noisy.push(format!("{command} -> {level:?}"));
        }
    }
    assert!(
        noisy.is_empty(),
        "gate fired on routine work, which trains users to bypass it: {noisy:#?}"
    );
}

/// Chaining, in every separator form, must not launder a catastrophic command.
#[test]
fn no_separator_launders_a_catastrophic_command() {
    let ctx = ctx();
    let mut escaped = Vec::new();
    for sep in ["&&", "||", ";", "|", "\n"] {
        for command in [
            format!("echo safe {sep} rm -rf ~"),
            format!("rm -rf ~ {sep} echo done"),
            format!("cd /tmp {sep} rm -rf $HOME"),
        ] {
            if assess(&command, &ctx).level != RiskLevel::Catastrophic {
                escaped.push(command);
            }
        }
    }
    assert!(
        escaped.is_empty(),
        "laundered through a separator: {escaped:#?}"
    );
}

/// Bypasses found by automated review of the original PR. Each of these was
/// classified `Safe` before the fix, including `sudo rm -rf ~`.
#[test]
fn wrapper_commands_do_not_hide_the_real_program() {
    let ctx = ctx();
    let mut escaped = Vec::new();
    for wrapper in [
        "sudo",
        "doas",
        "env",
        "nice -n 10",
        "ionice -c 3",
        "time",
        "timeout 5",
        "nohup",
        "setsid",
        "stdbuf -o0",
        "command",
        "exec",
        "sudo -u root",
        "env FOO=bar",
        "watch",
    ] {
        for tail in ["rm -rf ~", "rm -rf $HOME", "rm -rf /"] {
            let command = format!("{wrapper} {tail}");
            if assess(&command, &ctx).level != RiskLevel::Catastrophic {
                escaped.push(command);
            }
        }
    }
    assert!(escaped.is_empty(), "wrapper bypass: {escaped:#?}");
}

#[test]
fn nested_wrappers_are_unwrapped_all_the_way_down() {
    assert_eq!(
        level("sudo env nice -n 5 rm -rf ~"),
        RiskLevel::Catastrophic
    );
    assert_eq!(level("timeout 5 sudo rm -rf /"), RiskLevel::Catastrophic);
}

#[test]
fn a_wrapper_hiding_an_unparseable_payload_escalates() {
    // We could not see what sudo was about to run, so we must not assume safety.
    let ctx = ctx();
    assert!(assess("sudo", &ctx).level >= RiskLevel::Confirm);
}

#[test]
fn inline_shell_scripts_are_assessed_not_skipped() {
    // `sh -c "..."` is the classic way around a static parser. It cannot be
    // fully solved, but the plain literal case must not be a free pass.
    let ctx = ctx();
    for command in [
        r#"sh -c "rm -rf ~""#,
        r#"bash -c 'rm -rf $HOME'"#,
        r#"sudo sh -c "rm -rf /""#,
    ] {
        assert_eq!(
            assess(command, &ctx).level,
            RiskLevel::Catastrophic,
            "{command:?}"
        );
    }
}

#[test]
fn piped_deletes_cannot_launder_their_targets() {
    // `find ~ -type f | xargs rm -rf` deletes home contents, but neither
    // segment shows it: xargs takes its operands from stdin.
    let ctx = ctx();
    for command in [
        "find ~ -type f | xargs rm -rf",
        "find / -name '*.conf' | xargs rm",
        "cat paths.txt | xargs rm -rf",
    ] {
        assert!(
            !assess(command, &ctx).level.runs_immediately(),
            "{command:?} must not run unchallenged"
        );
    }
}

#[test]
fn files_inside_system_directories_are_protected_too() {
    // Exact-root matching left /etc/passwd merely "Confirm", and apply_patch
    // consults only the catastrophic tier, so it would have deleted it.
    for path in [
        "rm -f /etc/passwd",
        "rm -rf /usr/bin/env",
        "rm /boot/vmlinuz",
        "shred /etc/shadow",
    ] {
        assert_eq!(level(path), RiskLevel::Catastrophic, "{path:?}");
    }
}

#[test]
fn user_directories_under_home_root_stay_workable() {
    // /home and /Users must not become recursive, or every project path under
    // them would be blocked.
    assert!(level("rm -rf /home/u/proj/target").runs_immediately());
    assert_eq!(level("rm -rf /home"), RiskLevel::Catastrophic);
}

/// The wrapper unwrapping must not make everyday wrapped commands noisy.
#[test]
fn ordinary_wrapped_commands_still_run_immediately() {
    let ctx = ctx();
    let mut noisy = Vec::new();
    for command in [
        "sudo apt update",
        "env RUST_LOG=debug cargo test",
        "timeout 30 cargo build",
        "nice -n 10 make",
        "time ls -la",
        "sudo systemctl status jcode",
        "xargs echo",
        "find . -name '*.rs' | xargs grep TODO",
        "sh -c 'echo hello'",
        "bash -c 'cargo build'",
        "sudo rm -rf /home/u/proj/target",
    ] {
        let level = assess(command, &ctx).level;
        if !level.runs_immediately() {
            noisy.push(format!("{command} -> {level:?}"));
        }
    }
    assert!(
        noisy.is_empty(),
        "gate became noisy on normal work: {noisy:#?}"
    );
}

#[test]
fn read_only_find_with_stderr_redirect_is_safe() {
    let command = "find ~/.jcode -maxdepth 3 -type f \\
                   \\( -name '*.log' -o -name '*events*' \\) \\
                   -printf '%T@ %p\\n' 2>/dev/null | sort -nr | head -20";
    let assessment = assess(command, &ctx());
    assert_eq!(assessment.level, RiskLevel::Safe, "{assessment:#?}");
    assert!(assessment.findings.is_empty(), "{assessment:#?}");
}

#[test]
fn harmless_command_operands_are_not_redirect_targets() {
    for command in [
        "grep needle /home/u/.jcode/config 2>/dev/null",
        "find / -name '*.conf' 2>/dev/null",
        "printf '%s\\n' /home/u/.ssh/id_ed25519 >/tmp/list",
    ] {
        let assessment = assess(command, &ctx());
        assert_eq!(
            assessment.level,
            RiskLevel::Safe,
            "{command}: {assessment:#?}"
        );
    }
}

#[test]
fn safe_redirect_sink_does_not_make_explicit_deletion_safe() {
    assert_eq!(level("rm /dev/null"), RiskLevel::Catastrophic);
}

#[test]
fn shell_constructs_do_not_hide_catastrophic_commands() {
    for command in [
        "(rm -rf ~)",
        "x=$(rm -rf ~)",
        "case x in y) rm -rf ~;; esac",
        "if true; then rm -rf ~; fi",
        "while true; do rm -rf ~; done",
    ] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command:?}");
    }
    assert!(
        level("rm -rf $(echo ~)") >= RiskLevel::Confirm,
        "runtime-computed targets must not run immediately"
    );
}

#[test]
fn issue_725_read_only_and_bounded_commands_run_immediately() {
    for command in [
        r#"t=$(readlink "$f" 2>/dev/null)"#,
        "sudo -n ls /sys",
        "rm -f build-2026-*.json",
        "rm -f /tmp/jcode-*.json",
    ] {
        assert!(
            level(command).runs_immediately(),
            "{command:?} should be bounded, got {:?}",
            level(command)
        );
    }
}

#[test]
fn wrapper_value_flags_and_unbounded_globs_remain_guarded() {
    for command in [
        "nice -n 10 rm -rf ~",
        "sudo -u root rm -rf ~",
        "timeout -s KILL 5 rm -rf ~",
        "xargs -n 1 rm -rf ~",
        "ionice -c 3 rm -rf ~",
        "rm -rf /home/u/*/node_modules",
        "rm -rf /etc/*",
    ] {
        assert!(
            level(command) >= RiskLevel::Confirm,
            "{command:?} must remain guarded"
        );
    }
}

#[test]
fn environment_printing_and_command_lookup_are_read_only() {
    for command in [
        "env",
        "env -i",
        "env FOO=bar",
        "sudo env",
        "command -v sudo",
        "command -V rm",
        "command -pv sudo env rm",
        "command -p -V sudo",
        "env command -v sudo",
        "command -v rm -rf /",
    ] {
        assert_eq!(level(command), RiskLevel::Safe, "{command}");
    }
    for command in ["command -p rm -rf ~", "env rm -rf ~", "command -- rm -rf ~"] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command}");
    }
}

#[test]
fn read_only_find_actions_do_not_delete_search_roots() {
    for action in ["-exec", "-execdir", "-ok", "-okdir"] {
        for payload in [
            "cat {}",
            "/bin/cat -- {}",
            "readlink -f {}",
            "sed -n '1,200p' {}",
            "sed 'p' {}",
        ] {
            for root in ["/", "~", "~/.ssh", "/etc"] {
                for terminator in [r"\;", "+"] {
                    let command = format!("find {root} -type f {action} {payload} {terminator}");
                    assert_eq!(level(&command), RiskLevel::Safe, "{command}");
                }
            }
        }
    }
    assert_eq!(
        level(r"find / -exec cat {} \; -exec readlink {} \;"),
        RiskLevel::Safe
    );
}

#[test]
fn find_mutating_and_unknown_actions_remain_protected() {
    for action in ["-exec", "-execdir", "-ok", "-okdir"] {
        for payload in ["rm -rf {}", "sudo rm -rf {}", "shred {}"] {
            let command = format!(r"find ~ {action} {payload} \;");
            assert_eq!(level(&command), RiskLevel::Catastrophic, "{command}");
        }
        for payload in [
            "mystery {}",
            "$PROGRAM {}",
            "sed -i 's/a/b/' {}",
            "sed 'w /etc/passwd' {}",
            "sed 'e rm -rf ~' {}",
            "sed -n -f script {}",
            "sh -c '$PAYLOAD' {}",
        ] {
            let command = format!(r"find / {action} {payload} \;");
            assert!(level(&command) >= RiskLevel::Confirm, "{command}");
        }
    }
    for command in [
        "find ~ -delete",
        r"find ~ -exec cat {} \; -delete",
        r"find / -exec cat {} \; -exec rm -rf {} +",
        r"find . -exec rm -rf /etc \;",
        "find -L ~ -delete",
    ] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command}");
    }
    for command in [
        "find / -exec",
        "find / -exec cat {}",
        "find / -execdir mystery {} +",
    ] {
        assert!(level(command) >= RiskLevel::Confirm, "{command}");
    }
}

#[test]
fn read_only_early_returns_preserve_redirect_checks() {
    for prefix in [
        "env",
        "command -v sudo",
        r"find / -exec cat {} \;",
        r"find / -exec sed -n '1p' {} \;",
        "sh -c 'echo hello'",
    ] {
        assert_eq!(
            level(&format!("{prefix} > /etc/passwd")),
            RiskLevel::Catastrophic,
            "{prefix}"
        );
        assert_eq!(
            level(&format!("{prefix} > $UNKNOWN")),
            RiskLevel::Confirm,
            "{prefix}"
        );
        assert_eq!(
            level(&format!("{prefix} 2>/dev/null")),
            RiskLevel::Safe,
            "{prefix}"
        );
    }
}

#[test]
fn reassignment_does_not_override_trusted_protection() {
    for command in ["HOME=/tmp; rm -rf /home/u", "env HOME=/tmp rm -rf ~/.ssh"] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command}");
    }
    for command in [
        "HOME=/etc; rm -rf $HOME/cache",
        "JCODE_SCRATCH_DIR=/etc; rm -rf $JCODE_SCRATCH_DIR/passwd",
    ] {
        assert!(level(command) >= RiskLevel::Confirm, "{command}");
    }
    assert_eq!(level("env -u UNUSED rm -rf ~"), RiskLevel::Catastrophic);
    assert_eq!(level("env -u UNUSED"), RiskLevel::Safe);
    assert!(level("env -S 'rm -rf ~'") >= RiskLevel::Confirm);
}

#[test]
fn env_split_strings_cannot_be_mistaken_for_printing() {
    for command in [
        "env -S='rm -rf /'",
        "env --split-string='rm -rf /'",
        "env -u FOO -S 'rm -rf /'",
        "env -iS 'rm -rf /'",
    ] {
        assert!(level(command) >= RiskLevel::Confirm, "{command}");
    }
}

#[test]
fn find_output_actions_write_only_their_destinations() {
    for action in ["-fprint", "-fprint0", "-fls", "-fprintf"] {
        let suffix = if action == "-fprintf" { " '%p'" } else { "" };
        for (target, expected) in [
            ("/etc/passwd", RiskLevel::Catastrophic),
            ("$UNKNOWN", RiskLevel::Confirm),
            ("/tmp/find-output", RiskLevel::Safe),
            ("/dev/null", RiskLevel::Safe),
        ] {
            let command = format!("find / {action} {target}{suffix}");
            assert_eq!(level(&command), expected, "{command}");
        }
    }
    for command in [
        "find / -name '-delete'",
        "find / -printf '-delete'",
        "find / -name '-exec'",
    ] {
        assert_eq!(level(command), RiskLevel::Safe, "{command}");
    }
}

#[test]
fn read_only_find_exec_payloads_cover_inspection_programs() {
    // A live false positive on an installed build: listing the largest files
    // was refused as "`find` action may modify files" because only `cat`,
    // `readlink` and print-only `sed` were recognized.
    for action in ["-exec", "-execdir", "-ok", "-okdir"] {
        for payload in [
            "stat -f '%z %N' {}",
            "stat {}",
            "wc -l {}",
            "head -3 {}",
            "tail -n 5 {}",
            "grep -n TODO {}",
            "egrep 'a|b' {}",
            "fgrep literal {}",
            "rg --no-heading TODO {}",
            "ls -la {}",
            "file {}",
            "du -k {}",
            "md5 {}",
            "md5sum {}",
            "shasum -a 256 {}",
            "sha256sum {}",
            "b3sum {}",
            "basename {}",
            "dirname {}",
            "echo {}",
            "printf '%s\\n' {}",
            "test -f {}",
            "[ -d {} ]",
        ] {
            for root in ["/", "~", "~/.ssh", "/etc"] {
                for terminator in [r"\;", "+"] {
                    let command = format!("find {root} -type f {action} {payload} {terminator}");
                    assert_eq!(level(&command), RiskLevel::Safe, "{command}");
                }
            }
        }
    }
}

#[test]
fn the_reported_stat_find_pipeline_is_safe() {
    for command in [
        r"find /home/u/proj -type f -exec stat -f '%z %N' {} + | sort -rn | head -3",
        r"find . -type f -exec stat -c '%s %n' {} \; | sort -rn | head -3",
    ] {
        assert_eq!(level(command), RiskLevel::Safe, "{command}");
    }
}

#[test]
fn find_exec_still_refuses_payloads_that_write_or_execute() {
    for action in ["-exec", "-execdir", "-ok", "-okdir"] {
        for payload in [
            "rm -rf {}",
            "mv {} /tmp",
            "sh -c 'rm -rf ~' {}",
            "sed -i 's/a/b/' {}",
            "xargs rm {}",
        ] {
            for terminator in [r"\;", "+"] {
                let command = format!("find ~ -type f {action} {payload} {terminator}");
                assert!(level(&command) >= RiskLevel::Confirm, "{command}");
            }
        }
    }
}

#[test]
fn find_exec_refuses_read_only_programs_with_executing_flags() {
    // `rg --pre` runs a program per file and `file -C` writes magic.mgc.
    for command in [
        r#"find / -type f -exec rg --pre 'sh -c "rm -rf ~"' TODO {} \;"#,
        r"find / -type f -exec rg --pre=./hook TODO {} +",
        r"find / -type f -exec file -C {} \;",
        r"find / -type f -exec file --compile {} +",
    ] {
        assert!(level(command) >= RiskLevel::Confirm, "{command}");
    }
    for command in [
        r"find / -type f -exec rg -n TODO {} \;",
        r"find / -type f -exec file {} \;",
    ] {
        assert_eq!(level(command), RiskLevel::Safe, "{command}");
    }
}

#[test]
fn issue_1753_substitutions_are_assessed_like_top_level_commands() {
    for command in [
        "echo $(rm -rf ~)",
        "x=$(rm -rf ~)",
        "echo \"$(rm -rf ~)\"",
        "echo `rm -rf ~`",
        "echo \"`rm -rf ~`\"",
        "cat <<EOF\n$(rm -rf ~)\nEOF",
        "cat <<EOF\n`rm -rf ~`\nEOF",
        "echo \"$(echo \"$(rm -rf ~)\")\"",
        "git commit -m \"$(rm -rf ~)\"",
    ] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command:?}");
    }
}

#[test]
fn issue_1753_inert_text_stays_safe() {
    for command in [
        // The shell does not expand quoted heredocs or single quotes.
        "cat <<'EOF'\n$(rm -rf ~)\nEOF",
        "cat <<\"EOF\"\n`rm -rf ~`\nEOF",
        "echo '$(rm -rf ~)'",
        "echo '`rm -rf ~`'",
        // Ordinary substitutions keep running immediately.
        "echo \"$(git rev-parse HEAD)\"",
        "echo `date`",
        "git commit -m \"$(cat <<'EOF'\nfix: thing\n\nrm -rf ~ is mentioned here\nEOF\n)\"",
    ] {
        assert!(level(command).runs_immediately(), "{command:?}");
    }
}

// ---------------------------------------------------------------------------
// False positives reported from a live coordinator session. Each of these was
// refused with "target is computed at runtime", which trains an agent to add a
// hollow justification. They must run without a reflection turn.
// ---------------------------------------------------------------------------

#[test]
fn redirect_into_scratch_through_a_same_command_variable_runs_immediately() {
    let ctx = ctx_with_scratch();
    for command in [
        "R=$HOME/.jcode/scratch/cm-check; mkdir -p $R; cargo test > $R/out.ndjson 2> $R/err.log",
        "R=$JCODE_SCRATCH_DIR/cm-check; cargo test > $R/out.ndjson 2> $R/err.log",
        "S=$HOME/.jcode/scratch; R=$S/cm-check; cargo test > $R/out.ndjson",
        "cargo test > /home/u/.jcode/scratch/cm-check/out.ndjson 2> /home/u/.jcode/scratch/cm-check/err.log",
        "cargo test > /var/folders/xy/T/jcode-test.log 2>&1",
    ] {
        let level = level_in(&ctx, command);
        assert!(
            level.runs_immediately(),
            "{command:?} should run, got {level:?}: {}",
            assess(command, &ctx).explanation()
        );
    }
}

#[test]
fn worktree_cleanup_loops_over_known_names_run_immediately() {
    let ctx = ctx_with_scratch();
    for command in [
        "for w in gro sar wcm; do rm -rf ~/.jcode/wt/$w/target; done",
        "for w in /home/u/.jcode/wt/gro/target /home/u/.jcode/wt/sar/target; do rm -rf \"$w\"; done",
        "w=/home/u/.jcode/wt/gro; rm -rf $w/target",
        "for w in gro sar; do echo $w; done",
    ] {
        let level = level_in(&ctx, command);
        assert!(
            level.runs_immediately(),
            "{command:?} should run, got {level:?}: {}",
            assess(command, &ctx).explanation()
        );
    }
}

#[test]
fn read_only_pipelines_quoting_a_path_variable_are_not_reassignments() {
    let ctx = ctx_with_scratch();
    for command in [
        "ps eww -p $pid | grep -o 'JCODE_HOME=[^ ]*'",
        "ps eww -p $pid | grep JCODE_HOME",
        "lsof -p $pid | grep HOME=",
        "env | grep '^HOME='",
        "grep -rn 'HOME=' src",
        "grep -rn JCODE_SCRATCH_DIR= docs",
        "rg -n 'JCODE_SCRATCH_DIR=' crates",
    ] {
        let assessment = assess(command, &ctx);
        assert_eq!(
            assessment.level,
            RiskLevel::Safe,
            "{command}: {assessment:#?}"
        );
    }
}

/// Resolution must not launder a protected path through a variable.
#[test]
fn resolved_variables_do_not_launder_protected_targets() {
    let ctx = ctx_with_scratch();
    for command in [
        "R=/; rm -rf $R",
        "R=/etc; rm -rf $R/passwd",
        "S=/home/u/.jcode/sessions; rm -rf $S",
        "R=$HOME; rm -rf $R",
        "R=/home/u; rm -rf $R",
        "R=/home/u/.ssh; rm -rf $R",
        "P=rm; $P -rf /",
    ] {
        assert_eq!(
            level_in(&ctx, command),
            RiskLevel::Catastrophic,
            "{command}"
        );
    }
}

#[test]
fn unknown_variables_still_require_justification_after_resolution() {
    let ctx = ctx_with_scratch();
    for command in [
        "rm -rf $UNKNOWN",
        "rm -rf $wd/target",
        "for w in $(ls); do rm -rf $w; done",
        "X=$UNKNOWN; rm -rf $X",
    ] {
        assert!(level_in(&ctx, command) >= RiskLevel::Confirm, "{command}");
    }
}

#[test]
fn irreversible_git_operations_require_justification() {
    for command in [
        "git reset --hard",
        "git reset --hard HEAD~3",
        "git push --force origin main",
        "git push -f",
        "git push --force-with-lease origin main",
        "git clean -fdx",
    ] {
        assert!(level(command) >= RiskLevel::Confirm, "{command}");
    }
    for command in [
        "git status",
        "git push origin main",
        "git reset HEAD file.txt",
        "git reset --soft HEAD~1",
        "git log --oneline -3",
    ] {
        assert!(
            level(command).runs_immediately(),
            "{command} -> {:?}",
            level(command)
        );
    }
}

#[test]
fn deleting_a_repository_or_the_session_store_is_refused() {
    for command in [
        "rm -rf ~/.jcode/sessions",
        "rm -rf /home/u/.jcode/sessions",
        "rm -rf .git",
        "rm -rf /home/u/proj/.git",
    ] {
        assert!(level(command) >= RiskLevel::Confirm, "{command}");
    }
    assert!(level("rm -rf target").runs_immediately());
    assert!(level("rm -rf /home/u/proj/src/generated").runs_immediately());
}

#[test]
fn assignment_prefixed_commands_are_still_assessed() {
    // `X=1` in the program position used to hide the destructive verb entirely.
    for command in [
        "X=1 rm -rf ~",
        "HOME=/tmp rm -rf ~/.ssh",
        "FOO=bar shred /etc/passwd",
        "A=1 B=2 rm -rf /",
        "X=1 sudo rm -rf ~",
        "RUST_LOG=debug find /home/u -delete",
    ] {
        assert_eq!(level(command), RiskLevel::Catastrophic, "{command}");
    }
    // ...while an environment prefix on ordinary work stays quiet.
    for command in [
        "X=1 cargo build",
        "RUST_LOG=debug cargo test",
        "PATH=/opt/bin:$PATH cargo build",
        "EDITOR=vim git commit -m msg",
    ] {
        assert!(
            level(command).runs_immediately(),
            "{command} -> {:?}",
            level(command)
        );
    }
}

#[test]
fn a_variable_used_before_it_is_assigned_stays_guarded() {
    // The shell sees `$R` unset here, so resolving it from a later assignment
    // would describe a different command than the one that runs.
    assert!(level("rm -rf $R/target; R=/tmp") >= RiskLevel::Confirm);
}

/// A refusal is only useful if it names the token that tripped it and says how
/// to make the command pass.
#[test]
fn every_refusal_names_the_offending_token_and_a_way_forward() {
    let ctx = ctx_with_scratch();
    for (command, token) in [
        ("rm -rf $UNKNOWN", "$UNKNOWN"),
        ("echo x > $UNKNOWN", "$UNKNOWN"),
        ("find ~ -delete", "~"),
        ("git reset --hard", "--hard"),
        ("HOME=/etc; rm -rf $HOME/cache", "HOME=/etc"),
        ("rm -rf", "rm"),
        ("cat paths.txt | xargs rm -rf", "rm"),
        ("find / -exec mystery {} \\;", "mystery"),
    ] {
        let assessment = assess(command, &ctx);
        assert!(
            !assessment.level.runs_immediately(),
            "{command} should be refused"
        );
        let text = assessment.explanation();
        assert!(
            text.contains(token),
            "{command:?} refusal lacks {token:?}: {text}"
        );
        assert!(
            text.contains("instead") || text.contains("explicitly") || text.contains("literal"),
            "{command:?} refusal offers no way forward: {text}"
        );
    }
}
