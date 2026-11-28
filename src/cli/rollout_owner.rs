//! One CLI preparation per JCODE_HOME. The daemon journal takes over at stage.

use super::rollout::{Pair, Repo, status_line};
use crate::server::rollout::{
    Journal, Phase, Preparation, read_journal, read_preparation, record_preparation,
    try_handoff_gate, try_preparation_lock,
};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub(super) struct Lease {
    pub id: String,
    lock: File,
}

impl Lease {
    pub fn inherit(&self, command: &mut Command) {
        let fd = self.lock.as_raw_fd();
        // A killed CLI must not let another prep in while cargo/package still
        // uses the shared inputs. Clear CLOEXEC only in the mutating child.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

pub(super) enum Admission {
    Prepare(Lease),
    Join(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Relation {
    Covered,
    Newer,
    Conflict,
}

fn relation(
    owner: &Preparation,
    request: &Preparation,
    ancestor: &impl Fn(Repo, &str, &str) -> bool,
) -> Relation {
    if owner.app != request.app || owner.socket != request.socket {
        return Relation::Conflict;
    }
    let covers = |left: &Preparation, right: &Preparation| {
        (!right.include_desktop || left.include_desktop)
            && ancestor(Repo::Jcode, &right.cli, &left.cli)
            && ancestor(Repo::Desktop, &right.desktop, &left.desktop)
    };
    if covers(owner, request) {
        Relation::Covered
    } else if ancestor(Repo::Jcode, &owner.cli, &request.cli)
        && ancestor(Repo::Desktop, &owner.desktop, &request.desktop)
    {
        Relation::Newer
    } else {
        Relation::Conflict
    }
}

fn from_journal(journal: &Journal) -> Preparation {
    Preparation {
        id: journal.id.clone(),
        owner_pid: journal.owner_pid,
        session: journal.intent.triggering_session.clone(),
        cli: journal
            .intent
            .cli_sha
            .as_ref()
            .unwrap_or(&journal.intent.cli)
            .clone(),
        desktop: journal.intent.desktop.clone(),
        include_desktop: journal.intent.bundle.is_some(),
        app: journal.intent.app.clone(),
        socket: journal.socket.clone(),
    }
}

fn describe(owner: &Preparation, status: &str, request: &Preparation) -> String {
    format!(
        "{} {} (session {}, pid {}) CLI {} Desktop {} [{}]; requested CLI {} Desktop {} [{}]",
        owner.id,
        status,
        owner.session.as_deref().unwrap_or("none"),
        owner.owner_pid,
        owner.cli,
        owner.desktop,
        scope(owner),
        request.cli,
        request.desktop,
        scope(request)
    )
}

fn scope(prep: &Preparation) -> &'static str {
    if prep.include_desktop {
        "matched"
    } else {
        "CLI only"
    }
}

pub(super) struct Request<'a> {
    pub pair: &'a Pair,
    pub include_desktop: bool,
    pub session: Option<&'a str>,
    pub app: &'a Path,
    pub socket: &'a Path,
    pub terminal_at_start: Option<&'a str>,
    pub timeout: Duration,
}

impl Request<'_> {
    pub fn admit(
        &self,
        ancestor: impl Fn(Repo, &str, &str) -> bool,
        notify: impl Fn(&Preparation, &str),
    ) -> Result<Admission> {
        let request = Preparation {
            id: crate::id::new_id("prep"),
            owner_pid: std::process::id(),
            session: self.session.map(str::to_owned),
            cli: self.pair.cli.clone(),
            desktop: self.pair.desktop.clone(),
            include_desktop: self.include_desktop,
            app: self.app.to_path_buf(),
            socket: self.socket.to_path_buf(),
        };
        let start = Instant::now();
        let mut announced = None;
        loop {
            // Use the same gate as submit. A competing CLI either sees the
            // submitted journal, or replaces the old reservation before a late
            // submit can be accepted. Never hold the gate during polling.
            let gate = try_handoff_gate()?;
            let journal = read_journal()?;
            let occupied = match journal.as_ref() {
                Some(journal) if !journal.phase.terminal() => {
                    Some((from_journal(journal), Some(journal.phase)))
                }
                _ if gate.is_some() => match try_preparation_lock()? {
                    Some(lock) => {
                        if let Some(journal) = &journal
                            && Some(journal.id.as_str()) != self.terminal_at_start
                        {
                            let owner = from_journal(journal);
                            let detail =
                                describe(&owner, &format!("{:?}", journal.phase), &request);
                            match relation(&owner, &request, &ancestor) {
                                Relation::Covered if journal.phase == Phase::Completed => {
                                    return Ok(Admission::Join(detail));
                                }
                                Relation::Newer if journal.phase == Phase::Completed => {}
                                _ => bail!(
                                    "rollout not started: {detail}; inspect jcode rollout --status and coordinate the next pinned pair"
                                ),
                            }
                        }
                        record_preparation(&request)?;
                        return Ok(Admission::Prepare(Lease {
                            id: request.id,
                            lock,
                        }));
                    }
                    None => Some((
                        read_preparation()?.context("rollout has no preparation record")?,
                        None,
                    )),
                },
                _ => None,
            };
            drop(gate);

            if let Some((owner, phase)) = occupied {
                let status = phase.map_or("preparing".to_string(), |phase| format!("{phase:?}"));
                let detail = describe(&owner, &status, &request);
                match relation(&owner, &request, &ancestor) {
                    Relation::Conflict => {
                        notify(
                            &owner,
                            &format!(
                                "Incompatible rollout requests: {detail}. Coordinate an explicitly reviewed pair before retrying."
                            ),
                        );
                        bail!(
                            "conflicting rollout: {detail}; coordinate via notify without wake before retrying"
                        );
                    }
                    Relation::Covered if phase.is_some() => return Ok(Admission::Join(detail)),
                    // A mid-turn session waiting for terminal status can block
                    // the daemon's drain. Return pending instead of changing
                    // drain eligibility or resubmitting this pair.
                    Relation::Newer if phase.is_some() && self.session.is_some() => {
                        notify(
                            &owner,
                            &format!(
                                "A successor rollout is waiting: {detail}. Retry the pinned request after the current rollout is terminal."
                            ),
                        );
                        bail!(
                            "newer rollout pending: {detail}; retry the pinned pair after terminal jcode rollout --status (do not wait inside a turn)"
                        );
                    }
                    Relation::Covered | Relation::Newer => {
                        if announced.as_deref() != Some(owner.id.as_str()) {
                            let mut out = std::io::stdout().lock();
                            writeln!(out, "{}", status_line("plan", "wait", &detail))?;
                            out.flush()?;
                            announced = Some(owner.id);
                        }
                        if start.elapsed() >= self.timeout {
                            bail!(
                                "timed out waiting for rollout: {detail}; owner was not interrupted"
                            );
                        }
                    }
                }
            } else if start.elapsed() >= self.timeout {
                bail!("timed out waiting for rollout admission; owner was not interrupted");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
