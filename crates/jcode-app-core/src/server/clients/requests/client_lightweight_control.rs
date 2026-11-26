use super::client_comm::{
    handle_comm_channel_members, handle_comm_list, handle_comm_list_channels, handle_comm_message,
    handle_comm_read, handle_comm_share, handle_comm_subscribe_channel,
    handle_comm_unsubscribe_channel,
};
use super::client_comm_swarms::{
    handle_comm_focus_session, handle_comm_list_sessions, handle_comm_list_swarms,
    handle_comm_set_swarm_label,
};
use super::client_writer::write_direct_event;
use super::comm_await::{CommAwaitMembersContext, handle_comm_await_members};
use super::comm_control::{
    handle_comm_assign_next, handle_comm_assign_role, handle_comm_assign_task,
    handle_comm_task_control,
};
use super::comm_plan::{
    handle_comm_approve_plan, handle_comm_propose_plan, handle_comm_reject_plan,
};
use super::comm_session::{handle_comm_list_models, handle_comm_spawn, handle_comm_stop};
use super::comm_sync::{
    CommResyncPlanContext, handle_comm_plan_status, handle_comm_read_context,
    handle_comm_resync_plan, handle_comm_status, handle_comm_summary,
};
use super::session_notifications::{
    NotifySessionContext, handle_applet_action, handle_close_applet, handle_notify_session,
};
use super::{
    AwaitMembersRuntime, ChannelSubscriptions, ClientConnectionInfo, FileTouchService,
    SessionAgents, SessionInterruptQueues, SharedContext, SwarmEvent, SwarmMember,
    SwarmMutationRuntime, VersionedPlan, format_structured_completion_report, truncate_detail,
    update_member_status_with_report_tldr,
};
use crate::config::SwarmSpawnMode;
use crate::protocol::{Request, ServerEvent};
use crate::provider::Provider;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

pub(super) async fn openrig_host_identity() -> Option<String> {
    static IDENTITY: OpenrigIdentity = OpenrigIdentity::new();
    IDENTITY.get("http://127.0.0.1:7433/healthz").await
}

struct OpenrigIdentity {
    // Only explicit identity requests reach this lock. Ping remains an
    // independent liveness/presence snapshot even on cold starts.
    cached: Mutex<Option<(tokio::time::Instant, Option<String>)>>,
}

impl OpenrigIdentity {
    const fn new() -> Self {
        Self {
            cached: Mutex::const_new(None),
        }
    }

    async fn get(&self, url: &str) -> Option<String> {
        let mut cached = self.cached.lock().await;
        if let Some((expires, identity)) = cached.as_ref()
            && tokio::time::Instant::now() < *expires
        {
            return identity.clone();
        }
        let identity = openrig_host_identity_at(url).await;
        let ttl = std::time::Duration::from_secs(if identity.is_some() { 60 } else { 5 });
        // An expired claim is never returned if the host is no longer reachable.
        *cached = Some((tokio::time::Instant::now() + ttl, identity.clone()));
        identity
    }
}

async fn openrig_host_identity_at(url: &str) -> Option<String> {
    static CLIENT: std::sync::LazyLock<Option<reqwest::Client>> = std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            // This endpoint is fixed to loopback HTTP. Don't open the OS trust store.
            .tls_built_in_root_certs(false)
            .timeout(std::time::Duration::from_millis(750))
            .build()
            .ok()
    });
    let client = CLIENT.as_ref()?;
    let mut response = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > 16384 {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let id = body.get("selfHostId")?.as_str()?;
    if id.is_empty() || id.trim() != id || id.chars().any(char::is_control) {
        return None;
    }
    Some(id.to_owned())
}

#[cfg(test)]
mod openrig_identity_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn probe(body: &str) -> Option<String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_owned();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(request.starts_with("GET /healthz HTTP/1.1\r\n"));
            assert!(!request.to_ascii_lowercase().contains("authorization:"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let result = openrig_host_identity_at(&format!("http://{address}/healthz")).await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn concurrent_identity_requests_share_one_health_read() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/healthz", listener.local_addr().unwrap());
        let reads = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn({
            let reads = reads.clone();
            async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut input = [0; 4096];
                    if stream.read(&mut input).await.unwrap() == 0 {
                        continue;
                    }
                    reads.fetch_add(1, Ordering::Relaxed);
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 23\r\nConnection: close\r\n\r\n{\"selfHostId\":\"host-a\"}").await.unwrap();
                }
            }
        });
        let cache = OpenrigIdentity::new();
        let (a, b, c) = tokio::join!(cache.get(&url), cache.get(&url), cache.get(&url));
        server.abort();
        assert_eq!(a.as_deref(), Some("host-a"));
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn real_read_only_health_requires_authoritative_id() {
        assert_eq!(
            probe(r#"{"selfHostId":"host-a"}"#).await.as_deref(),
            Some("host-a")
        );
        assert_eq!(probe(r#"{"name":"same-name"}"#).await, None);
        assert_eq!(probe(r#"{"selfHostId":""}"#).await, None);
        assert_eq!(probe("broken").await, None);
    }
}

pub(super) fn parse_swarm_spawn_mode(
    id: u64,
    spawn_mode: Option<String>,
    client_event_tx: &mpsc::UnboundedSender<ServerEvent>,
) -> Option<Option<SwarmSpawnMode>> {
    match spawn_mode {
        Some(value) => match SwarmSpawnMode::parse(&value) {
            Some(mode) => Some(Some(mode)),
            None => {
                let _ = client_event_tx.send(ServerEvent::Error {
                    id,
                    message: format!(
                        "Invalid spawn_mode '{value}'. Expected one of: visible, headless, inline, auto"
                    ),
                    retry_after_secs: None,
                });
                None
            }
        },
        None => Some(None),
    }
}

/// Extract the session_id from a Comm* request that carries one, so the
/// lightweight control path can ensure swarm membership before dispatching.
/// Returns None for requests that do not reference a swarm session.
fn comm_request_session_id(request: &Request) -> Option<&str> {
    match request {
        Request::CommShare { session_id, .. }
        | Request::CommRead { session_id, .. }
        | Request::CommMessage {
            from_session: session_id,
            ..
        }
        | Request::CommList { session_id, .. }
        | Request::CommListChannels { session_id, .. }
        | Request::CommListSwarms { session_id, .. }
        | Request::CommListSessions { session_id, .. }
        | Request::CommFocusSession { session_id, .. }
        | Request::CommSetSwarmLabel { session_id, .. }
        | Request::CommChannelMembers { session_id, .. }
        | Request::CommProposePlan { session_id, .. }
        | Request::CommApprovePlan { session_id, .. }
        | Request::CommRejectPlan { session_id, .. }
        | Request::CommSpawn { session_id, .. }
        | Request::CommListModels { session_id, .. }
        | Request::CommStop { session_id, .. }
        | Request::CommWorkerControl { session_id, .. }
        | Request::CommAssignRole { session_id, .. }
        | Request::CommSummary { session_id, .. }
        | Request::CommStatus { session_id, .. }
        | Request::CommReport { session_id, .. }
        | Request::CommPlanStatus { session_id, .. }
        | Request::CommReadContext { session_id, .. }
        | Request::CommResyncPlan { session_id, .. }
        | Request::CommAssignTask { session_id, .. }
        | Request::CommAssignNext { session_id, .. }
        | Request::CommTaskControl { session_id, .. }
        | Request::CommSubscribeChannel { session_id, .. }
        | Request::CommUnsubscribeChannel { session_id, .. }
        | Request::CommAwaitMembers { session_id, .. }
        | Request::CommAnswerEscalatedAsk { session_id, .. } => Some(session_id),
        _ => None,
    }
}

/// Ensure a session referenced by a lightweight Comm* request has a
/// SwarmMember entry. Headless `jcode run` sessions are in-process agents
/// whose session_id was never registered on the server (no Subscribe ever
/// happened), so every swarm lookup returns "Not in a swarm" (#1748).
/// Auto-registering here makes the swarm tool work for any lightweight
/// client without changing the transport or requiring a Subscribe frame.
async fn ensure_lightweight_swarm_member(
    session_id: &str,
    sessions: &SessionAgents,
    swarm_members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
    swarms_by_id: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    if !crate::config::config().features.swarm {
        return;
    }

    // Fast path: already registered (the normal TUI/attached case).
    if swarm_members.read().await.contains_key(session_id) {
        return;
    }

    // A session hosted by this server owns its membership through
    // Subscribe. If it has none, that is deliberate (for example swarm was
    // disabled for it), so do not grant membership behind its back.
    if sessions.contains(session_id).await {
        return;
    }

    // Only register sessions that really exist on disk. A headless `jcode
    // run` saves its session before the first turn, so its stub is present
    // by the time any tool runs. Unknown ids (typos, stale ids, arbitrary
    // strings) keep the historical "Not in a swarm" answer instead of
    // minting a phantom member.
    //
    // Recover the caller's working directory from that stub too. Do NOT use
    // the request's working_dir: CommSpawn's is the WORKER's directory, not
    // the caller's: caching it on the caller would make subsequent spawns
    // without an override inherit the wrong project.
    let Ok(stub) = crate::session::Session::load_startup_stub(session_id) else {
        return;
    };
    let working_dir = stub.working_dir().map(PathBuf::from);

    let swarm_id = super::util::swarm_id_for_session(session_id);
    let now = std::time::Instant::now();

    // Create a dedicated event channel for the member with a lifecycle-owned
    // drain task, mirroring create_headless_session's pattern: the channel
    // stays open for as long as the member exists in swarm_members. Without
    // the drain task, dropping the receiver would close the channel
    // immediately, and a shared-swarm root replacement would treat the
    // still-running coordinator as unreachable (review: "Live coordinator
    // gets replaced"). When the member is removed (comm stop / daemon
    // shutdown), dropping the last sender closes the channel and the drain
    // task exits naturally.
    //
    // KNOWN LIMITATION (design follow-up): the drain task keeps the channel
    // open after the headless caller exits, so the member stays "ready". In
    // a shared swarm (JCODE_SWARM_ID), a later root's spawn may defer to
    // the exited coordinator. The proper fix requires a session-exit hook
    // (mark/remove the auto-registered member when the in-process `jcode run`
    // session reaches its terminal state): connection close is NOT a valid
    // proxy (it only proves one request finished, not that the session
    // exited). Tracked as a follow-up; the single-caller case (the primary
    // use case: headless run + swarm spawn) works correctly.
    let (member_event_tx, mut member_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
    tokio::spawn(async move {
        while member_event_rx.recv().await.is_some() {
            // Drain events to keep the channel alive
        }
    });

    let mut members = swarm_members.write().await;
    // Re-check under the write lock: another request may have registered
    // this session while we were waiting.
    if members.contains_key(session_id) {
        return;
    }
    members.insert(
        session_id.to_string(),
        SwarmMember {
            session_id: session_id.to_string(),
            event_tx: member_event_tx,
            event_txs: HashMap::new(),
            working_dir: working_dir.clone(),
            swarm_id: swarm_id.clone(),
            swarm_enabled: true,
            status: "ready".to_string(),
            detail: None,
            task_label: None,
            friendly_name: None,
            report_back_to_session_id: None,
            latest_completion_report: None,
            role: "agent".to_string(),
            joined_at: now,
            last_status_change: now,
            is_headless: true,
            output_tail: None,
            todo_progress: None,
            todo_items: Vec::new(),
            runtime: crate::protocol::SwarmMemberRuntime::default(),
        },
    );
    drop(members);

    if let Some(swarm_id) = swarm_id {
        let mut swarms = swarms_by_id.write().await;
        swarms
            .entry(swarm_id)
            .or_insert_with(HashSet::new)
            .insert(session_id.to_string());
    }

    crate::logging::event_info(
        "SWARM_LIFECYCLE",
        vec![
            ("phase", "lightweight_member_registered".to_string()),
            ("session_id", session_id.to_string()),
            ("working_dir_recovered", working_dir.is_some().to_string()),
        ],
    );
}

pub(super) struct LightweightControlContext<'a> {
    pub(super) sessions: &'a SessionAgents,
    pub(super) global_session_id: &'a Arc<RwLock<String>>,
    pub(super) provider_template: &'a Arc<dyn Provider>,
    pub(super) swarm_members: &'a Arc<RwLock<HashMap<String, SwarmMember>>>,
    pub(super) swarms_by_id: &'a Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub(super) shared_context: &'a Arc<RwLock<HashMap<String, HashMap<String, SharedContext>>>>,
    pub(super) swarm_plans: &'a Arc<RwLock<HashMap<String, VersionedPlan>>>,
    pub(super) swarm_coordinators: &'a Arc<RwLock<HashMap<String, String>>>,
    pub(super) file_touch: &'a FileTouchService,
    pub(super) channel_subscriptions: &'a ChannelSubscriptions,
    pub(super) channel_subscriptions_by_session: &'a ChannelSubscriptions,
    pub(super) client_connections: &'a Arc<RwLock<HashMap<String, ClientConnectionInfo>>>,
    pub(super) event_history: &'a Arc<RwLock<std::collections::VecDeque<SwarmEvent>>>,
    pub(super) event_counter: &'a Arc<std::sync::atomic::AtomicU64>,
    pub(super) swarm_event_tx: &'a broadcast::Sender<SwarmEvent>,
    pub(super) mcp_pool: &'a Arc<crate::mcp::SharedMcpPool>,
    pub(super) soft_interrupt_queues: &'a SessionInterruptQueues,
    pub(super) await_members_runtime: &'a AwaitMembersRuntime,
    pub(super) swarm_mutation_runtime: &'a SwarmMutationRuntime,
}

#[path = "side_question_control.rs"]
pub(super) mod side_question_control;
use side_question_control::handle_side_question_control;

pub(super) async fn handle_lightweight_control_request(
    request: Request,
    writer: Arc<Mutex<crate::transport::WriteHalf>>,
    context: LightweightControlContext<'_>,
) -> Result<()> {
    let LightweightControlContext {
        sessions,
        global_session_id,
        provider_template,
        swarm_members,
        swarms_by_id,
        shared_context,
        swarm_plans,
        swarm_coordinators,
        file_touch,
        channel_subscriptions,
        channel_subscriptions_by_session,
        client_connections,
        event_history,
        event_counter,
        swarm_event_tx,
        mcp_pool,
        soft_interrupt_queues,
        await_members_runtime,
        swarm_mutation_runtime,
    } = context;
    if matches!(
        request,
        Request::ListSideQuestions { .. }
            | Request::GetSideQuestionHistory { .. }
            | Request::PromoteSideQuestion { .. }
    ) {
        return write_direct_event(&writer, &handle_side_question_control(request).await).await;
    }
    if let Request::RoutingView { id } = request {
        return write_direct_event(&writer, &super::routing_view_request::reply(id).await).await;
    }
    if let Request::UpgradeRuntime { id, ref candidate } = request {
        return super::runtime_upgrade::handle(id, candidate, &writer, sessions).await;
    }
    if let Request::SessionContextBreakdown {
        id,
        session_id,
        model,
        cwd,
        summary_only,
        include_snapcompact,
    } = request
    {
        let event = super::context_breakdown::reply(
            id,
            session_id,
            (model.as_deref(), cwd.as_deref()),
            summary_only,
            include_snapcompact,
            sessions,
            provider_template,
        )
        .await;
        return write_direct_event(&writer, &event).await;
    }
    if let Request::SessionShakeTools {
        id,
        session_id,
        action,
    } = request
    {
        let event = super::shake_tools::reply(id, session_id, action, sessions).await;
        return write_direct_event(&writer, &event).await;
    }
    // Ping only snapshots connection presence. Reserving mutation admission here
    // races the next group-transfer RPC even after the caller receives Pong.
    // It must also remain available while a transfer owns exclusive admission.
    if let Request::Ping { id } = request {
        let event = super::session_connections::connection_presence_pong(
            id,
            client_connections,
            swarm_members,
        );
        write_direct_event(&writer, &event).await?;
        return Ok(());
    }
    if let Request::OpenrigHostIdentity { id } = request {
        let mut event = super::session_connections::connection_presence_pong(
            id,
            client_connections,
            swarm_members,
        );
        if let Some(host_id) = openrig_host_identity().await
            && let ServerEvent::Pong { capabilities, .. } = &mut event
        {
            capabilities.push(format!("openrig_self_host_id={host_id}"));
        }
        write_direct_event(&writer, &event).await?;
        return Ok(());
    }

    // Comm requests name their owning session. Scope admission to it so a
    // long-lived request (await_members can hold for an hour) only blocks
    // transfers of that session, not every /cloud move on the runtime.
    let gate = crate::runtime_activity::global();
    let _request_activity = match comm_request_session_id(&request) {
        Some(session) if !session.trim().is_empty() => gate.try_enter_session(session)?,
        _ => gate.try_enter()?,
    };
    write_direct_event(&writer, &ServerEvent::Ack { id: request.id() }).await?;

    let (client_event_tx, mut client_event_rx) = mpsc::unbounded_channel::<ServerEvent>();
    let writer_clone = Arc::clone(&writer);
    let event_handle = tokio::spawn(async move {
        while let Some(event) = client_event_rx.recv().await {
            if let Err(error) = write_direct_event(&writer_clone, &event).await {
                // Routine on client reload/disconnect; avoid dumping the full
                // event (an await response can embed whole completion reports).
                let event_desc = crate::logging::truncate_for_log(&format!("{:?}", event), 200);
                crate::logging::warn(&format!(
                    "lightweight control writer failed while sending {}: {}",
                    event_desc, error
                ));
                break;
            }
        }
    });

    // Auto-register the session referenced by a Comm* request as a swarm
    // member. Headless `jcode run` agents are in-process and never Subscribe,
    // so their session_id has no SwarmMember entry on the server and every
    // swarm action fails with "Not in a swarm" (#1748). Registering here makes
    // the swarm tool work for lightweight clients without a Subscribe frame.
    if let Some(session_id) = comm_request_session_id(&request) {
        ensure_lightweight_swarm_member(session_id, sessions, swarm_members, swarms_by_id).await;
    }

    match request {
        // Scheduled delivery opens a one-shot connection and names the target
        // session explicitly. Reuse its live agent, not a new subscribed agent.
        Request::AccountUsage { id } => {
            super::account_usage::request(id, client_event_tx.clone());
        }
        Request::InvalidateOpenAiUsage { id, account_label } => {
            super::provider_control::handle_invalidate_openai_usage(
                id,
                account_label,
                &client_event_tx,
            )
            .await;
        }
        Request::InvalidateAnthropicUsage { id, account_label } => {
            super::provider_control::handle_invalidate_anthropic_usage(
                id,
                account_label,
                &client_event_tx,
            )
            .await;
        }
        Request::NotifyAuthChanged {
            id, provider, auth, ..
        } => {
            super::provider_control::handle_unbound_notify_auth_changed(
                id,
                provider,
                auth,
                provider_template,
                sessions,
                &client_event_tx,
            )
            .await;
        }
        Request::NotifySession {
            id,
            session_id,
            message,
        } => {
            handle_notify_session(
                id,
                session_id,
                message,
                NotifySessionContext {
                    sessions,
                    soft_interrupt_queues,
                    swarm_members,
                    swarms_by_id,
                    event_history,
                    event_counter,
                    swarm_event_tx,
                    client_event_tx: &client_event_tx,
                },
            )
            .await;
        }
        Request::AppletAction {
            id,
            session_id,
            instance,
            action,
            state,
            source_key,
        } => {
            handle_applet_action(
                id,
                session_id,
                instance,
                action,
                state,
                source_key,
                NotifySessionContext {
                    sessions,
                    soft_interrupt_queues,
                    swarm_members,
                    swarms_by_id,
                    event_history,
                    event_counter,
                    swarm_event_tx,
                    client_event_tx: &client_event_tx,
                },
            )
            .await;
        }
        Request::CloseApplet {
            id,
            session_id,
            instance,
        } => {
            handle_close_applet(id, session_id, instance, &client_event_tx);
        }
        Request::CommShare {
            id,
            session_id: req_session_id,
            key,
            value,
            append,
        } => {
            handle_comm_share(
                id,
                req_session_id,
                key,
                value,
                append,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommRead {
            id,
            session_id: req_session_id,
            key,
        } => {
            handle_comm_read(
                id,
                req_session_id,
                key,
                &client_event_tx,
                swarm_members,
                shared_context,
            )
            .await;
        }
        Request::CommMessage {
            id,
            from_session,
            message,
            to_session,
            channel,
            delivery,
            wake,
            tldr,
            to_swarm,
            require_target,
        } => {
            handle_comm_message(
                id,
                from_session,
                message,
                to_session,
                channel,
                delivery,
                wake,
                tldr,
                to_swarm,
                require_target,
                &client_event_tx,
                sessions,
                soft_interrupt_queues,
                swarm_members,
                swarms_by_id,
                channel_subscriptions,
                event_history,
                event_counter,
                swarm_event_tx,
                client_connections,
            )
            .await;
        }
        Request::CommList {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list(
                id,
                req_session_id,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                file_touch,
                sessions,
                client_connections,
            )
            .await;
        }
        Request::CommListSessions {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_sessions(id, req_session_id, &client_event_tx, swarm_members).await;
        }
        Request::CommFocusSession {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_focus_session(
                id,
                req_session_id,
                target_session,
                &client_event_tx,
                swarm_members,
            )
            .await;
        }
        Request::CommListSwarms {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_swarms(
                id,
                req_session_id,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
            )
            .await;
        }
        Request::CommSetSwarmLabel {
            id,
            session_id: req_session_id,
            label,
        } => {
            handle_comm_set_swarm_label(
                id,
                req_session_id,
                label,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommListChannels {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_channels(
                id,
                req_session_id,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
            )
            .await;
        }
        Request::CommChannelMembers {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_channel_members(
                id,
                req_session_id,
                channel,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
            )
            .await;
        }
        Request::CommProposePlan {
            id,
            session_id: req_session_id,
            items,
        } => {
            handle_comm_propose_plan(
                id,
                req_session_id,
                items,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                swarm_plans,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommApprovePlan {
            id,
            session_id: req_session_id,
            proposer_session,
        } => {
            handle_comm_approve_plan(
                id,
                req_session_id,
                proposer_session,
                &client_event_tx,
                swarm_members,
                swarms_by_id,
                shared_context,
                swarm_plans,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommRejectPlan {
            id,
            session_id: req_session_id,
            proposer_session,
            reason,
        } => {
            handle_comm_reject_plan(
                id,
                req_session_id,
                proposer_session,
                reason,
                &client_event_tx,
                swarm_members,
                shared_context,
                swarm_coordinators,
                sessions,
                soft_interrupt_queues,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommSpawn {
            id,
            session_id: req_session_id,
            lane_id,
            working_dir,
            initial_message,
            request_nonce,
            spawn_mode,
            model,
            effort,
            speed,
            label,
            profile,
        } => {
            let spawn_mode = match parse_swarm_spawn_mode(id, spawn_mode, &client_event_tx) {
                Some(spawn_mode) => spawn_mode,
                None => return Ok(()),
            };
            handle_comm_spawn(
                id,
                req_session_id,
                lane_id,
                working_dir,
                initial_message,
                request_nonce,
                spawn_mode,
                model,
                effort,
                speed,
                label,
                profile,
                &client_event_tx,
                sessions,
                global_session_id,
                provider_template,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
                mcp_pool,
                soft_interrupt_queues,
                swarm_mutation_runtime,
                client_connections,
            )
            .await;
        }
        Request::CommListModels {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_list_models(id, &req_session_id, sessions, provider_template, |event| {
                let _ = client_event_tx.send(event);
            })
            .await;
        }
        Request::CommStop {
            id,
            session_id: req_session_id,
            target_session,
            force,
        } => {
            handle_comm_stop(
                id,
                req_session_id,
                target_session,
                force.unwrap_or(false),
                &client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
                soft_interrupt_queues,
                await_members_runtime,
            )
            .await;
        }
        Request::CommWorkerControl {
            id,
            session_id,
            target_session,
            control,
        } => {
            super::comm_worker_control::handle(
                id,
                session_id,
                target_session,
                control,
                &client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
            )
            .await;
        }
        Request::CommAnswerEscalatedAsk {
            id,
            session_id: req_session_id,
            worker_session_id,
            request_id,
            answers,
        } => {
            super::ask_escalation::handle_answer_request(
                id,
                &req_session_id,
                &worker_session_id,
                request_id,
                answers,
                &client_event_tx,
                swarm_members,
            )
            .await;
        }
        Request::CommAssignRole {
            id,
            session_id: req_session_id,
            target_session,
            role,
        } => {
            handle_comm_assign_role(
                id,
                req_session_id,
                target_session,
                role,
                &client_event_tx,
                sessions,
                swarm_members,
                swarms_by_id,
                swarm_coordinators,
                swarm_plans,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommSummary {
            id,
            session_id: req_session_id,
            target_session,
            limit,
        } => {
            handle_comm_summary(
                id,
                req_session_id,
                target_session,
                limit,
                sessions,
                swarm_members,
                &client_event_tx,
            )
            .await;
        }
        Request::CommStatus {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_status(
                id,
                req_session_id,
                target_session,
                sessions,
                swarm_members,
                client_connections,
                file_touch,
                &client_event_tx,
            )
            .await;
        }
        Request::CommReport {
            id,
            session_id: req_session_id,
            status,
            message,
            validation,
            follow_up,
            tldr,
        } => {
            let status = status.unwrap_or_else(|| "ready".to_string());
            let report = format_structured_completion_report(
                &message,
                validation.as_deref(),
                follow_up.as_deref(),
            );
            let detail = Some(truncate_detail(&message, 160));
            let report_changed = update_member_status_with_report_tldr(
                &req_session_id,
                &status,
                detail,
                Some(report),
                tldr,
                swarm_members,
                swarms_by_id,
                Some(event_history),
                Some(event_counter),
                Some(swarm_event_tx),
            )
            .await;
            super::comm_control::persist_worker_report(
                &req_session_id,
                (&status, report_changed),
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
            )
            .await;
            let _ = client_event_tx.send(ServerEvent::CommReportResponse {
                id,
                status,
                message: "Report recorded and delivered to the coordinator when applicable."
                    .to_string(),
            });
        }
        Request::CommPlanStatus {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_plan_status(
                id,
                req_session_id,
                swarm_members,
                swarm_plans,
                &client_event_tx,
            )
            .await;
        }
        Request::CommReadContext {
            id,
            session_id: req_session_id,
            target_session,
        } => {
            handle_comm_read_context(
                id,
                req_session_id,
                target_session,
                sessions,
                swarm_members,
                &client_event_tx,
            )
            .await;
        }
        Request::CommResyncPlan {
            id,
            session_id: req_session_id,
        } => {
            handle_comm_resync_plan(
                id,
                req_session_id,
                &CommResyncPlanContext {
                    client_event_tx: &client_event_tx,
                    swarm_members,
                    swarms_by_id,
                    swarm_plans,
                    swarm_coordinators,
                    event_history,
                    event_counter,
                    swarm_event_tx,
                },
            )
            .await;
        }
        Request::CommAssignTask {
            id,
            session_id: req_session_id,
            target_session,
            task_id,
            message,
        } => {
            handle_comm_assign_task(
                id,
                req_session_id,
                target_session,
                task_id,
                message,
                &client_event_tx,
                sessions,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommAssignNext {
            id,
            session_id: req_session_id,
            target_session,
            working_dir,
            prefer_spawn,
            spawn_if_needed,
            message,
            model,
            effort,
        } => {
            handle_comm_assign_next(
                id,
                req_session_id,
                target_session,
                working_dir,
                prefer_spawn,
                spawn_if_needed,
                message,
                model,
                effort,
                &client_event_tx,
                sessions,
                global_session_id,
                provider_template,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                mcp_pool,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommTaskControl {
            id,
            session_id: req_session_id,
            action,
            task_id,
            target_session,
            message,
        } => {
            handle_comm_task_control(
                id,
                req_session_id,
                action,
                task_id,
                target_session,
                message,
                &client_event_tx,
                sessions,
                soft_interrupt_queues,
                client_connections,
                swarm_members,
                swarms_by_id,
                swarm_plans,
                swarm_coordinators,
                event_history,
                event_counter,
                swarm_event_tx,
                swarm_mutation_runtime,
            )
            .await;
        }
        Request::CommSubscribeChannel {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_subscribe_channel(
                id,
                req_session_id,
                channel,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommUnsubscribeChannel {
            id,
            session_id: req_session_id,
            channel,
        } => {
            handle_comm_unsubscribe_channel(
                id,
                req_session_id,
                channel,
                &client_event_tx,
                swarm_members,
                channel_subscriptions,
                channel_subscriptions_by_session,
                event_history,
                event_counter,
                swarm_event_tx,
            )
            .await;
        }
        Request::CommAwaitMembers {
            id,
            session_id: req_session_id,
            target_status,
            session_ids: requested_ids,
            mode,
            timeout_secs,
            background,
            notify,
            wake,
        } => {
            handle_comm_await_members(
                id,
                req_session_id,
                target_status,
                requested_ids,
                mode,
                timeout_secs,
                background,
                notify,
                wake,
                CommAwaitMembersContext {
                    client_event_tx: &client_event_tx,
                    swarm_members,
                    swarms_by_id,
                    swarm_event_tx,
                    await_members_runtime,
                },
            )
            .await;
        }
        other => {
            let _ = client_event_tx.send(ServerEvent::Error {
                id: other.id(),
                message: "unsupported lightweight control request".to_string(),
                retry_after_secs: None,
            });
        }
    }

    drop(client_event_tx);
    let _ = event_handle.await;
    Ok(())
}

#[cfg(test)]
#[path = "client_lightweight_control/lightweight_swarm_member_tests.rs"]
mod lightweight_swarm_member_tests;
