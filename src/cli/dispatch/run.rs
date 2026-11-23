use super::{ProviderChoice, Result, commands, server, spawn_owned_server, spawn_server};

pub(super) async fn run_one_shot(
    provider: ProviderChoice,
    model: Option<&str>,
    provider_profile: Option<&str>,
    resume: Option<&str>,
    input: Option<String>,
    persistence: bool,
    output: commands::RunOutput<'_>,
) -> Result<()> {
    let message = commands::RunInput::from(input).read()?;
    register_on_demand_server_spawner_for_run(
        provider,
        model.map(str::to_owned),
        provider_profile.map(str::to_owned),
    );
    commands::run_single_message_command(
        &provider,
        model,
        commands::RunPersistence::from_args(resume, persistence),
        &message,
        output,
    )
    .await
}

/// Let a headless one-shot `jcode run` start a server on demand.
///
/// `jcode run` hosts its agent in-process, so there is no server for tools
/// that need one. Registering the spawner here (rather than at startup) keeps
/// it to the `run` command: interactive and client launches already bootstrap
/// a daemon through their own paths. Nothing is started by registering. The
/// first tool that cannot reach a server triggers the spawn, so runs that never
/// use `swarm` keep their current startup cost.
///
/// Which server gets started follows the dialed socket:
///
/// - Shared socket (no `--socket`): the long-lived daemon, exactly as
///   `jcode server start` or a TUI launch would start it. An ephemeral server
///   here would squat the path every later client uses and then vanish with
///   this run.
/// - Custom `--socket`: a server owned by this run. The caller asked for an
///   isolated socket, so nobody else is waiting on it, and the run must not
///   leave a daemon behind on a one-off path.
fn register_on_demand_server_spawner_for_run(
    provider_choice: ProviderChoice,
    model: Option<String>,
    provider_profile: Option<String>,
) {
    crate::server_spawn::register_on_demand_server_spawner(Box::new(move || {
        let model = model.clone();
        let provider_profile = provider_profile.clone();
        Box::pin(async move {
            let model = model.as_deref();
            let provider_profile = provider_profile.as_deref();
            if server::socket_path_is_custom() {
                spawn_owned_server(&provider_choice, model, provider_profile).await
            } else {
                spawn_server(&provider_choice, model, provider_profile).await
            }
        })
    }));
}
