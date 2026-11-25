use super::*;

pub(super) fn required_subscribe_working_dir(
    working_dir: Option<&str>,
) -> std::result::Result<&str, String> {
    let working_dir = working_dir
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
        .ok_or_else(|| "Subscribe requires the client's working directory".to_string())?;
    if !Path::new(working_dir).is_absolute() {
        return Err("Subscribe working_dir must be an absolute path".to_string());
    }
    Ok(working_dir)
}

pub(super) fn initial_subscribe_working_dir(
    request: &Request,
) -> std::result::Result<String, String> {
    match request {
        Request::Subscribe {
            working_dir,
            continue_on_disconnect,
            ..
        } => validated_subscribe_working_dir(working_dir.as_deref(), *continue_on_disconnect)
            .map(str::to_string),
        _ => Err(
            "Client must Subscribe with a working_dir before sending stateful requests".to_string(),
        ),
    }
}

/// A reattachment names an existing session, not a new client working directory.
/// Prefer the target's cwd before provisional initialization. The client's
/// cwd can initialize a new session or a placeholder, never an attach target.
pub(super) async fn resolve_target_subscribe_working_dir(
    request: &mut Request,
    sessions: &SessionAgents,
    members: &Arc<RwLock<HashMap<String, SwarmMember>>>,
) -> std::result::Result<(), String> {
    let Request::Subscribe {
        working_dir,
        target_session_id: Some(target),
        ..
    } = request
    else {
        return Ok(());
    };
    let live = sessions.read().await.get(target).cloned();
    let resolved = if let Some(live) = live {
        super::super::subscribe_working_dir::live_session_working_dir(&live, target, members).await
    } else {
        match crate::session::Session::load_startup_stub(target) {
            Ok(session) => session.working_dir().map(str::to_owned),
            Err(error)
                if working_dir.is_some()
                    && error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "Unknown session '{target}' or session could not be loaded for attachment: {error:#}"
                ));
            }
        }
    };
    if let Some(resolved) = resolved {
        *working_dir = Some(resolved);
    }
    if working_dir.is_some() {
        Ok(())
    } else {
        Err(format!(
            "Unknown session '{target}' or session has no working directory"
        ))
    }
}

pub(super) fn validated_subscribe_working_dir(
    working_dir: Option<&str>,
    remote_continuation: bool,
) -> std::result::Result<&str, String> {
    let working_dir = required_subscribe_working_dir(working_dir)?;
    if remote_continuation && !Path::new(working_dir).is_dir() {
        return Err(format!(
            "Remote working directory must exist and be a directory on the server: {working_dir}"
        ));
    }
    Ok(working_dir)
}

pub(super) fn new_session_system_prompt<'a>(
    provisional_session: bool,
    target_session_id: Option<&str>,
    system_prompt: Option<&'a str>,
) -> Option<&'a str> {
    if provisional_session && target_session_id.is_none() {
        system_prompt
    } else {
        None
    }
}

pub(super) fn initial_subscribe_terminal_env(request: &Request) -> Vec<(String, String)> {
    match request {
        Request::Subscribe { terminal_env, .. } => terminal_env.clone(),
        _ => Vec::new(),
    }
}
