use crate::server::GroupTransferBundle;

const RETURN_PENDING: &str = "return-pending";
const RETURN_COMPLETE: &str = "returned-complete";
const LOCAL_IMPORT_PENDING: &str = "group-activation-pending";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GroupDeparture {
    coordinator: String,
    epoch: u64,
    transfer: String,
}

impl GroupDeparture {
    fn from_group(group: &Group) -> Result<Self> {
        let bundle: GroupTransferBundle = serde_json::from_value(group.swarm.clone())?;
        Ok(Self {
            coordinator: group.coordinator.clone(),
            epoch: root(group)?.manifest.epoch,
            transfer: bundle.transfer_id,
        })
    }

    fn commit(&self) -> Result<storage::GroupReturnCommit> {
        Ok(storage::GroupReturnCommit {
            coordinator: self.coordinator.clone(),
            epoch: next_epoch(self.epoch)?,
        })
    }

    fn state(&self, file: &str) -> Result<PathBuf> {
        return_state_path(&self.coordinator, next_epoch(self.epoch)?, file)
    }

    fn call(&self, target: &Target, host: &str, action: &str) -> Result<Vec<u8>> {
        retry_ssh(host, action, || {
            target.run_remote(
                host,
                &[
                    "cloud",
                    action,
                    "--coordinator",
                    &self.coordinator,
                    "--epoch",
                    &self.epoch.to_string(),
                    "--transfer",
                    &self.transfer,
                ],
                None,
            )
        })
    }
}

fn next_epoch(epoch: u64) -> Result<u64> {
    epoch
        .checked_add(1)
        .context("group migration epoch exhausted")
}

#[derive(Debug, Serialize, Deserialize)]
struct ReturnedMember {
    id: String,
    session_sha256: String,
    todos_sha256: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct GroupReturnArchive {
    departure: GroupDeparture,
    bundle: GroupTransferBundle,
    repo: RepoManifest,
    members: Vec<ReturnedMember>,
}

#[derive(Serialize, Deserialize)]
enum ExportGroupState {
    Preparing(GroupTransferBundle),
    Aborting(GroupTransferBundle),
    Fenced(GroupTransferBundle),
}

pub(super) fn return_coordinator(id: &str) -> Result<Option<String>> {
    let path = member_marker(id)?;
    if path.exists() {
        let marker: serde_json::Value = read_json(&path)?;
        return Ok(Some(
            marker["coordinator"]
                .as_str()
                .context("invalid group membership marker")?
                .into(),
        ));
    }
    Ok(group_dir(id)?
        .join("journal.json")
        .is_file()
        .then(|| id.to_owned()))
}

fn group_ids(group: &Group) -> Vec<String> {
    group
        .members
        .iter()
        .map(|m| m.manifest.session_id.clone())
        .collect()
}

fn group_repo(group: &Group) -> Result<&RepoManifest> {
    root(group)?
        .manifest
        .repo
        .as_ref()
        .context("group repository missing")
}

fn validate_return_bundle(group: &Group, bundle: &GroupTransferBundle) -> Result<()> {
    if bundle.version != GROUP_VERSION
        || bundle.destination_host != RETURN_PENDING
        || bundle.sessions.len() != group.members.len()
    {
        bail!("group return membership changed; refusing to transfer part of a swarm");
    }
    for member in &group.members {
        let manifest = &member.manifest;
        let epoch = next_epoch(manifest.epoch)?;
        let matches = bundle
            .sessions
            .iter()
            .filter(|s| {
                s.session_id == manifest.session_id
                    && s.epoch == epoch
                    && s.previous_lease.as_ref().is_some_and(|lease| {
                        lease.session_id == s.session_id
                            && lease.epoch == manifest.epoch
                            && lease.is_here()
                    })
            })
            .count();
        if matches != 1 {
            bail!(
                "group return has unexpected membership or departure epoch for {}",
                manifest.session_id
            );
        }
    }
    Ok(())
}

fn received_group(departure: &GroupDeparture) -> Result<Group> {
    let group: Group = read_json(&group_dir(&departure.coordinator)?.join("received.json"))
        .context("no activated cloud group matches this return")?;
    if GroupDeparture::from_group(&group)? != *departure {
        bail!("group return does not match the activated migration identity");
    }
    Ok(group)
}

fn group_lease(id: &str) -> Result<storage::SessionLease> {
    let lease = storage::read_session_lease(id)
        .with_context(|| format!("missing or invalid group lease for {id}"))?;
    if lease.session_id != id {
        bail!("group lease identity mismatch for {id}");
    }
    Ok(lease)
}

fn cloud_fences(group: &Group, allow_completed: bool) -> Result<Vec<storage::SessionLease>> {
    let mut fences = Vec::new();
    for member in &group.members {
        let manifest = &member.manifest;
        let lease = group_lease(&manifest.session_id)?;
        if lease.epoch == next_epoch(manifest.epoch)?
            && (lease.away_host.as_deref() == Some(RETURN_PENDING)
                || (allow_completed && lease.away_host.as_deref() == Some(RETURN_COMPLETE)))
        {
            fences.push(lease);
        } else if lease.epoch != manifest.epoch || !lease.is_here() {
            bail!(
                "cloud group lease has a stale epoch or unexpected owner for {}",
                lease.session_id
            );
        }
    }
    Ok(fences)
}

fn require_repo(root: &Path) -> Result<()> {
    let actual = git(root, &["rev-parse", "--show-toplevel"])?;
    if actual != root.to_str().context("invalid group repository path")? {
        bail!("the group repository is no longer at {}", root.display());
    }
    Ok(())
}

// The old daemon already provides admission, checkpointing, fencing and abort.
// Journal before invoking it, including before a recovery abort that may lose
// its response. Once all remote fences are durable, never call abort again.
fn fence_cloud_group(group: &Group, departure: &GroupDeparture) -> Result<GroupTransferBundle> {
    let path = departure.state("group-export.json")?;
    if path.is_file() {
        let state: ExportGroupState = read_json(&path)?;
        let state = match state {
            ExportGroupState::Fenced(bundle) => return Ok(bundle),
            ExportGroupState::Preparing(bundle)
                if cloud_fences(group, false)?.len() == group.members.len() =>
            {
                validate_return_bundle(group, &bundle)?;
                return record_cloud_fence(group, &path, bundle);
            }
            ExportGroupState::Preparing(bundle) | ExportGroupState::Aborting(bundle) => bundle,
        };
        validate_return_bundle(group, &state)?;
        durable_json(&path, &ExportGroupState::Aborting(state.clone()))?;
        daemon_command("abort", &serde_json::to_value(&state)?)?;
        std::fs::remove_file(&path)?;
    }
    if !cloud_fences(group, false)?.is_empty() {
        bail!("cloud group has return fences without a recovery journal");
    }
    let ids = group_ids(group);
    let mut expected = daemon_command("inspect", &serde_json::json!({"session_ids": ids}))?;
    expected["destination_host"] = serde_json::Value::String(RETURN_PENDING.into());
    let bundle = serde_json::from_value(expected.clone())?;
    validate_return_bundle(group, &bundle)?;
    durable_json(&path, &ExportGroupState::Preparing(bundle))?;
    let prepared = daemon_command(
        "prepare",
        &serde_json::json!({
            "session_ids": ids, "destination_host": RETURN_PENDING, "expected": expected,
        }),
    )?;
    let bundle = serde_json::from_value(prepared)?;
    validate_return_bundle(group, &bundle)?;
    record_cloud_fence(group, &path, bundle)
}

fn record_cloud_fence(
    group: &Group,
    path: &Path,
    bundle: GroupTransferBundle,
) -> Result<GroupTransferBundle> {
    let fences = cloud_fences(group, false)?;
    if fences.len() != group.members.len() {
        bail!("cloud group has not completely stopped; retry the return");
    }
    for lease in fences {
        write_lease(&lease)?;
    }
    durable_json(path, &ExportGroupState::Fenced(bundle.clone()))?;
    Ok(bundle)
}

pub(super) fn export_group(coordinator: String, epoch: u64, transfer: String) -> Result<Vec<u8>> {
    let departure = GroupDeparture {
        coordinator,
        epoch,
        transfer,
    };
    let group = received_group(&departure)?;
    let ids = group_ids(&group);
    let _locks = lock_sessions(&ids)?;
    let group = received_group(&departure)?;
    let archive_path = departure.state("group-export.tar")?;
    let fences = cloud_fences(&group, archive_path.is_file())?;
    if archive_path.is_file() {
        if fences.len() != ids.len() {
            bail!("cannot replay a group archive while a cloud member is still active");
        }
        return Ok(std::fs::read(archive_path)?);
    }
    let original_repo = group_repo(&group)?;
    let mapping = group_dir(&departure.coordinator)?.join("mapped-repo.json");
    let mapped: Option<RepoManifest> = if mapping.is_file() {
        Some(read_json(&mapping)?)
    } else {
        None
    };
    let repo_root = Path::new(mapped.as_ref().unwrap_or(original_repo).root.as_str());
    require_repo(repo_root)?;
    let bundle = fence_cloud_group(&group, &departure)?;
    if cloud_fences(&group, false)?.len() != ids.len() {
        bail!("cannot export a group while a cloud member is still active");
    }
    let temporary = tempfile::tempdir()?;
    let stage = temporary.path();
    let mut repo = prepare_repo_snapshot(repo_root, &departure.coordinator, stage)?
        .context("cloud group repository disappeared")?;
    create_repo_bundle(
        &repo,
        &departure.coordinator,
        stage,
        Some(&original_repo.snapshot),
    )?;
    repo.root = original_repo.root.clone();
    repo.working_dir = original_repo.working_dir.clone();
    let mut members = Vec::new();
    for transfer in &bundle.sessions {
        let stage = stage.join("members").join(&transfer.session_id);
        std::fs::create_dir_all(&stage)?;
        let mut sess = session::Session::load(&transfer.session_id)?;
        sess.apply_lineage(crate::session::Lineage {
            migration_epoch: transfer.epoch,
            ..sess.lineage()
        });
        let session_file = stage.join("session.json");
        let todos = stage.join("todos.json");
        durable_json(&session_file, &sess)?;
        copy_if_exists(&todo_file(sess.id())?, &todos)?;
        members.push(ReturnedMember {
            id: sess.id().to_owned(),
            session_sha256: digest(&session_file)?,
            todos_sha256: todos.is_file().then(|| digest(&todos)).transpose()?,
        });
    }
    durable_json(
        &stage.join("group-return.json"),
        &GroupReturnArchive {
            departure,
            bundle,
            repo,
            members,
        },
    )?;
    let tar = stage.join("return.tar");
    tar_dir(
        stage,
        &tar,
        &["group-return.json", "members", "repo.bundle"],
    )?;
    let bytes = std::fs::read(tar)?;
    write_return_state(&archive_path, &bytes)?;
    Ok(bytes)
}

pub(super) fn complete_group_return(
    coordinator: String,
    epoch: u64,
    transfer: String,
) -> Result<()> {
    let departure = GroupDeparture {
        coordinator,
        epoch,
        transfer,
    };
    let group = received_group(&departure)?;
    let _locks = lock_sessions(&group_ids(&group))?;
    let group = received_group(&departure)?;
    let fences = cloud_fences(&group, true)?;
    if fences.len() != group.members.len() || !departure.state("group-export.tar")?.is_file() {
        bail!(
            "cannot complete group return before all cloud members have been fenced and exported"
        );
    }
    for mut lease in fences {
        lease.away_host = Some(RETURN_COMPLETE.into());
        write_lease(&lease)?;
    }
    Ok(())
}

fn return_archive_sessions(
    group: &Group,
    archive: &GroupReturnArchive,
    stage: &Path,
) -> Result<Vec<session::Session>> {
    validate_return_bundle(group, &archive.bundle)?;
    if archive.departure != GroupDeparture::from_group(group)?
        || archive.repo.root != group_repo(group)?.root
        || archive.members.len() != group.members.len()
    {
        bail!("group return archive does not match the original migration");
    }
    group
        .members
        .iter()
        .map(|member| {
            let manifest = &member.manifest;
            let id = &manifest.session_id;
            let entry = archive
                .members
                .iter()
                .find(|m| &m.id == id)
                .with_context(|| format!("group return archive is missing {id}"))?;
            let dir = stage.join("members").join(id);
            let file = dir.join("session.json");
            let mut sess: session::Session = read_json(&file)?;
            let todos = dir.join("todos.json");
            if sess.id() != *id
                || sess.migration_epoch() != next_epoch(manifest.epoch)?
                || digest(&file)? != entry.session_sha256
                || todos.is_file().then(|| digest(&todos)).transpose()? != entry.todos_sha256
            {
                bail!("group return archive failed transcript or todo verification for {id}");
            }
            sess.apply_workspace(crate::session::Workspace {
                working_dir: manifest.repo.as_ref().map(|repo| repo.working_dir.clone()),
                ..sess.workspace()
            });
            Ok(sess)
        })
        .collect()
}

pub(super) fn return_group(
    coordinator: &str,
    requested: &str,
    target: &Target,
    refs_only: bool,
    keep_local_auth: bool,
) -> Result<ReturnReport> {
    let path = group_dir(coordinator)?.join("journal.json");
    let record: Journal =
        read_json(&path).context("group return must run on the original machine")?;
    let ids = group_ids(&record.group);
    let _locks = lock_sessions(&ids)?;
    let journal: Journal = read_json(&path)?;
    let group = &journal.group;
    if group.coordinator != coordinator
        || !ids.iter().any(|id| id == requested)
        || group_ids(group) != ids
    {
        bail!("group return membership changed; retry from the original machine");
    }
    if journal.phase != Phase::Complete {
        bail!("finish the original group move before returning it");
    }
    let departure = GroupDeparture::from_group(group)?;
    let commit = departure.commit()?;
    let host = &root(group)?.manifest.target_host;
    if target.host.as_ref().is_some_and(|given| given != host) {
        bail!("this group was moved to `{host}`; omit --host or use its original host");
    }
    let receipt_path = departure.state("group-receipt.json")?;
    let done = departure.state("group-acknowledged")?;
    if !commit.path()?.is_file() {
        let repo = group_repo(group)?;
        let local_root = Path::new(&repo.root);
        require_repo(local_root)?;
        for member in &group.members {
            let manifest = &member.manifest;
            let lease = group_lease(&manifest.session_id)?;
            let departure_fence =
                lease.epoch == manifest.epoch && lease.away_host.as_deref() == Some(host);
            let import_fence = lease.epoch == next_epoch(manifest.epoch)?
                && lease.away_host.as_deref() == Some(LOCAL_IMPORT_PENDING);
            if (!departure_fence && !import_fence) || lease.repo_root.as_deref() != Some(&repo.root)
            {
                bail!(
                    "local group member {} has stale or conflicting ownership",
                    lease.session_id
                );
            }
        }
        let temporary = tempfile::tempdir()?;
        let stage = temporary.path();
        let archive_path = departure.state("group-incoming.tar")?;
        if !archive_path.is_file() {
            daemon_command("check_import", &group.swarm)?;
            step(
                "export",
                &format!(
                    "stopping and collecting all {} sessions from `{host}`",
                    ids.len()
                ),
            );
            let bytes = departure.call(target, host, "group-export")
                .context("group export unconfirmed; all local members remain fenced. Retry the same return. The remote CLI must support group return (use --remote-binary if needed)")?;
            write_return_state(&archive_path, &bytes)?;
        }
        untar(&archive_path, stage)?;
        let archive: GroupReturnArchive = read_json(&stage.join("group-return.json"))?;
        let sessions = return_archive_sessions(group, &archive, stage)?;
        let bundle = serde_json::to_value(&archive.bundle)?;
        daemon_command("check_import", &bundle)?;
        if !keep_local_auth {
            return_group_credentials(group, target, host)?;
        }
        step(
            "merge",
            "bringing back the shared Git checkout once for the group",
        );
        let merge = merge_group_repo(
            local_root,
            &departure,
            repo,
            &archive.repo,
            &stage.join("repo.bundle"),
            refs_only,
        )?;
        step(
            "session",
            "installing all transcripts, todos and swarm state under a local fence",
        );
        daemon_command("reserve", &bundle)?;
        let messages = install_returned_group(&sessions, stage, host)?;
        import_group(&bundle)?;
        for member in &group.members {
            let mut lease = member_lease(member, Some(LOCAL_IMPORT_PENDING.into()));
            lease.epoch = next_epoch(lease.epoch)?;
            lease.last_sync_snapshot = Some(archive.repo.snapshot.clone());
            lease.last_sync_head = Some(archive.repo.head.clone());
            let mut value = serde_json::to_value(&lease)?;
            value["group_return"] = serde_json::to_value(&commit)?;
            let path = storage::session_leases_dir()
                .context("missing lease directory")?
                .join(format!("{}.json", lease.session_id));
            durable_json(&path, &value)?;
        }
        durable_json(
            &receipt_path,
            &ReturnReport {
                session_id: coordinator.to_owned(),
                host: host.clone(),
                epoch: commit.epoch,
                messages,
                merge,
                members: ids.clone(),
            },
        )?;
        write_return_state(&commit.path()?, b"1")?;
    }
    let report: ReturnReport = read_json(&receipt_path)?;
    if !done.is_file() {
        // Normal leases also let older local daemons see the committed group.
        // Never reinstall transcripts after publication, even if ACK was lost.
        for member in &group.members {
            let lease = group_lease(&member.manifest.session_id)?;
            if !lease.is_here() || lease.epoch != next_epoch(member.manifest.epoch)? {
                bail!(
                    "committed local group lease changed for {}",
                    lease.session_id
                );
            }
            write_lease(&lease)?;
        }
        departure.call(target, host, "group-complete-return")
            .context("the entire group is safely local, but the cloud checkout remains protected; retry the same return to finish")?;
        write_return_state(&done, b"1")?;
    }
    step("done", "every group member is local again");
    Ok(report)
}

fn return_group_credentials(group: &Group, target: &Target, host: &str) -> Result<()> {
    let path = baseline_path(&group.coordinator)?;
    if path.is_file() {
        let baselines: Vec<CloudAuthBaseline> = read_json(&path)?;
        for baseline in baselines {
            let account = baseline.account;
            let bytes = retry_ssh(host, "group credential return", || {
                target.exchange_credentials(host, &account, None)
            })?;
            import_return_credentials(&group.coordinator, &account, &bytes)
                .context("group credential return refused; retry with --keep-local-auth to keep local credentials")?;
        }
    }
    Ok(())
}

fn merge_group_repo(
    root: &Path,
    departure: &GroupDeparture,
    original: &RepoManifest,
    remote: &RepoManifest,
    bundle: &Path,
    refs_only: bool,
) -> Result<MergeOutcome> {
    let path = departure.state("group-merge.json")?;
    let applied = departure.state("group-merge-applied")?;
    let plan: ReturnMergePlan = if path.is_file() {
        read_json(&path)?
    } else {
        let plan = plan_merge_back(
            root,
            &departure.coordinator,
            remote,
            bundle,
            Some(&original.snapshot),
            refs_only,
        )?;
        if let Some(checkout) = &plan.checkout {
            let prefix = format!(
                "{SNAPSHOT_REF_PREFIX}/{}/return/{}",
                departure.coordinator,
                next_epoch(departure.epoch)?
            );
            git(
                root,
                &[
                    "update-ref",
                    &format!("{prefix}/local-tree"),
                    &checkout.ours,
                ],
            )?;
            git(
                root,
                &[
                    "update-ref",
                    &format!("{prefix}/merged-tree"),
                    &checkout.tree,
                ],
            )?;
        }
        durable_json(&path, &plan)?;
        plan
    };
    if !applied.is_file() {
        if let Some(checkout) = &plan.checkout {
            apply_checkout_merge(root, checkout)?;
        }
        write_return_state(&applied, b"1")?;
    }
    Ok(plan.outcome)
}

fn install_returned_group(
    sessions: &[session::Session],
    stage: &Path,
    host: &str,
) -> Result<usize> {
    let mut messages = 0;
    for sess in sessions {
        let mut sess = sess.clone();
        append_system_notice(
            &mut sess,
            &format!(
                "Coordinated group returned from cloud host `{host}`. Swarm ownership, task assignments and todos came back. Cloud processes did not come back. Workers remain paused until explicitly resumed."
            ),
        );
        let path = session::session_path(sess.id())?;
        remove_return_file(&session::session_journal_path_from_snapshot(&path))?;
        durable_json(&path, &sess)?;
        let todos = stage.join("members").join(sess.id()).join("todos.json");
        let dest = todo_file(sess.id())?;
        if todos.is_file() {
            write_return_state(&dest, &std::fs::read(todos)?)?;
        } else {
            remove_return_file(&dest)?;
        }
        messages += sess.messages().len();
    }
    Ok(messages)
}

fn remove_return_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other.with_context(|| format!("remove {}", path.display())),
    }
}
