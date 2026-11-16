use super::*;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

pub(super) const RECONNECT_TIMEOUT: Duration = Duration::from_secs(120);
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

pub(super) enum DaemonTransport {
    Direct {
        reader: Mutex<Lines<BufReader<ReadHalf>>>,
        writer: Mutex<WriteHalf>,
    },
    Managed(ManagedConnection),
}

pub(super) struct ManagedConnection {
    requests: mpsc::Sender<DaemonRequest>,
    events: Mutex<mpsc::Receiver<Result<ServerEvent>>>,
    task: tokio::task::JoinHandle<()>,
}

struct DaemonRequest {
    request: Request,
    sent: oneshot::Sender<Result<()>>,
}

enum ConnectionState {
    Connected(Box<DaemonSession>),
    Reconnecting { cause: String },
    Failed { cause: String },
}

struct ReconnectSession {
    session_id: String,
    working_dir: PathBuf,
    env: Option<HashMap<String, String>>,
    model: Arc<Mutex<Option<String>>>,
    ui_state: Arc<Mutex<SessionUiState>>,
}

impl Drop for ManagedConnection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ManagedConnection {
    pub(super) async fn send(&self, request: &Request) -> Result<()> {
        let (sent, result) = oneshot::channel();
        self.requests
            .send(DaemonRequest {
                request: request.clone(),
                sent,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Jcode daemon connection manager stopped"))?;
        result
            .await
            .context("Jcode daemon connection manager stopped")?
    }

    pub(super) async fn read_event(&self) -> Result<ServerEvent> {
        self.events
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("Jcode daemon connection manager stopped"))?
    }
}

impl DaemonSession {
    pub(super) fn maintain_connection(self, runtime: AcpRuntime) -> Result<Self> {
        let context = ReconnectSession {
            session_id: self.session_id.clone(),
            working_dir: self
                .working_dir
                .clone()
                .context("cannot maintain ACP connection without a workspace")?,
            env: self.session_env.clone(),
            model: Arc::clone(&self.selected_model),
            ui_state: Arc::clone(&self.ui_state),
        };
        let (requests, incoming) = mpsc::channel(16);
        let (events, outgoing) = mpsc::channel(64);
        let ui_state = Arc::clone(&self.ui_state);
        let selected_model = Arc::clone(&self.selected_model);
        let working_dir = self.working_dir.clone();
        let session_env = self.session_env.clone();
        let next_request_id = self.next_request_id.load(Ordering::Relaxed);
        Ok(Self {
            session_id: self.session_id.clone(),
            transport: DaemonTransport::Managed(ManagedConnection {
                requests,
                events: Mutex::new(outgoing),
                task: tokio::spawn(maintain(runtime, context, self, incoming, events)),
            }),
            next_request_id: AtomicU64::new(next_request_id),
            active_prompt_id: Mutex::new(None),
            prompt_running: AtomicBool::new(false),
            cancel_requested: AtomicBool::new(false),
            ui_state,
            session_env,
            selected_model,
            working_dir,
            active_skill: Mutex::new(None),
        })
    }
}

async fn maintain(
    runtime: AcpRuntime,
    context: ReconnectSession,
    session: DaemonSession,
    mut requests: mpsc::Receiver<DaemonRequest>,
    events: mpsc::Sender<Result<ServerEvent>>,
) {
    let mut state = ConnectionState::Connected(Box::new(session));
    let mut pending = None;
    let mut retry_request = None;
    let mut reload_id = None;
    loop {
        match state {
            ConnectionState::Connected(ref session) => {
                tokio::select! {
                    request = async {
                        match retry_request.take() {
                            Some(request) => Some(request),
                            None => requests.recv().await,
                        }
                    } => {
                        let Some(request) = request else { return };
                        match session.send(&request.request).await {
                            Ok(()) => {
                                if !matches!(request.request, Request::Cancel { .. }) {
                                    pending = Some(request.request);
                                }
                                if request.sent.send(Ok(())).is_err() {
                                    crate::logging::info("ACP request acknowledgement receiver was dropped");
                                }
                            }
                            Err(error) => {
                                let cause = format!("Jcode daemon disconnected while sending request: {error:#}");
                                if request.sent.send(Err(anyhow::anyhow!(cause.clone()))).is_err() {
                                    crate::logging::warn(&format!("ACP request error receiver was dropped: {cause}"));
                                }
                                if pending.take().is_some()
                                    && events.send(Err(anyhow::anyhow!(cause.clone()))).await.is_err() {
                                    return;
                                }
                                state = ConnectionState::Reconnecting { cause };
                            }
                        }
                    }
                    event = session.read_event() => {
                        match event {
                            Ok(event) => {
                                if let ServerEvent::Reloading { reload_id: id, .. } = &event {
                                    reload_id = id.clone();
                                }
                                if let Some(request) = &pending {
                                    let complete = completes_request(request, &event);
                                    if complete
                                        && let Request::SetModel { model, .. } = request
                                        && matches!(&event, ServerEvent::ModelChanged { error: None, .. }) {
                                        *context.model.lock().await = Some(model.clone());
                                    }
                                    if events.send(Ok(event)).await.is_err() { return; }
                                    if complete { pending = None; }
                                }
                            }
                            Err(error) => {
                                let cause = format!("Jcode daemon disconnected; interrupted request was not replayed: {error:#}");
                                if pending.take().is_some()
                                    && events.send(Err(anyhow::anyhow!(cause.clone()))).await.is_err() {
                                    return;
                                }
                                state = ConnectionState::Reconnecting { cause };
                            }
                        }
                    }
                }
            }
            ConnectionState::Reconnecting { ref cause } => {
                crate::logging::info(&format!("ACP reconnecting: {cause}"));
                let timeout = runtime.reconnect_timeout;
                let reconnect = reconnect_with_backoff(timeout, || {
                    restore(&runtime, &context, reload_id.as_deref())
                });
                match reconnect.await {
                    Ok(session) => {
                        reload_id = None;
                        pending = None;
                        state = ConnectionState::Connected(Box::new(session));
                    }
                    Err(error) => {
                        let cause = format!(
                            "Failed to reconnect to Jcode daemon within {timeout:?}: {error:#}. Retry the ACP request to try again"
                        );
                        if let Some(request) = retry_request.take()
                            && request
                                .sent
                                .send(Err(anyhow::anyhow!(cause.clone())))
                                .is_err()
                        {
                            crate::logging::warn(&format!(
                                "ACP retry error receiver was dropped: {cause}"
                            ));
                        }
                        while let Ok(request) = requests.try_recv() {
                            if request
                                .sent
                                .send(Err(anyhow::anyhow!(cause.clone())))
                                .is_err()
                            {
                                crate::logging::warn(&format!(
                                    "ACP queued request error receiver was dropped: {cause}"
                                ));
                            }
                        }
                        reload_id = None;
                        state = ConnectionState::Failed { cause };
                    }
                }
            }
            ConnectionState::Failed { ref cause } => {
                let Some(request) = requests.recv().await else {
                    return;
                };
                retry_request = Some(request);
                state = ConnectionState::Reconnecting {
                    cause: cause.clone(),
                };
            }
        }
    }
}

fn completes_request(request: &Request, event: &ServerEvent) -> bool {
    let id = request.id();
    match event {
        ServerEvent::Done { id: event_id } | ServerEvent::Error { id: event_id, .. } => {
            *event_id == id
        }
        ServerEvent::History { id: event_id, .. } => {
            *event_id == id
                && matches!(
                    request,
                    Request::GetHistory { .. } | Request::GetModelCatalog { .. }
                )
        }
        ServerEvent::ModelChanged { id: event_id, .. } => {
            *event_id == id && matches!(request, Request::SetModel { .. })
        }
        ServerEvent::ReasoningEffortChanged { id: event_id, .. } => {
            *event_id == id && matches!(request, Request::SetReasoningEffort { .. })
        }
        ServerEvent::Reloading { .. } => true,
        _ => false,
    }
}

async fn restore(
    runtime: &AcpRuntime,
    context: &ReconnectSession,
    reload_id: Option<&str>,
) -> Result<DaemonSession> {
    if let Some(reload_id) = reload_id {
        let status = crate::server::await_reload_handoff_for(
            &runtime.socket_path,
            Duration::from_secs(30),
            reload_id,
            None,
        )
        .await;
        match status {
            crate::server::ReloadWaitStatus::Ready => {}
            crate::server::ReloadWaitStatus::Failed(_) | crate::server::ReloadWaitStatus::Idle => {
                crate::server::replacement_spawn_permission(&runtime.socket_path, None)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            other => anyhow::bail!("daemon reload handoff is not ready: {other:?}"),
        }
    }
    let (reader, writer) = runtime.connect_daemon().await?;
    let mut attaching = runtime.clone();
    attaching.session_env = SessionEnvMode::None;
    let mut session = attaching
        .attach_existing_session_connected(
            context.session_id.clone(),
            context.working_dir.clone(),
            false,
            reader,
            writer,
        )
        .await?;
    anyhow::ensure!(
        session.session_id == context.session_id,
        "daemon attached a different ACP session"
    );
    session.session_env = context.env.clone();
    session.apply_session_env().await?;
    if let Some(model) = context.model.lock().await.clone() {
        let id = session.next_id();
        session.send(&Request::SetModel { id, model }).await?;
        wait_for_model_changed(&session, id).await?;
    }
    *context.ui_state.lock().await = session.ui_state.lock().await.clone();
    session.ui_state = Arc::clone(&context.ui_state);
    session.selected_model = Arc::clone(&context.model);
    Ok(session)
}

pub(super) async fn reconnect_with_backoff<T, F, Fut>(
    timeout: Duration,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_error = String::from("daemon unavailable");
    let result = tokio::time::timeout(timeout, async {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            tokio::time::sleep(backoff).await;
            match attempt().await {
                Ok(connection) => return connection,
                Err(error) => last_error = format!("{error:#}"),
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    })
    .await;
    result.map_err(|_| anyhow::anyhow!(last_error))
}
