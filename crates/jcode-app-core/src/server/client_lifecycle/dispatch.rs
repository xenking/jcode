//! Routes a decoded request to the handler for its domain.
//!
//! `route_of` matches every `Request` variant, so adding one without deciding
//! where it belongs does not compile.

use super::*;

enum Route {
    /// Answered before acknowledgment, never routed here.
    Early,
    Turn,
    Session,
    Attach,
    Settings,
    RuntimeCommand,
    Actions,
}

fn route_of(request: &Request) -> Route {
    match request {
        Request::ClientFocus { .. }
        | Request::ConfigureTools { .. }
        | Request::EditQueuedPrompt { .. }
        | Request::ForceStopTurn { .. }
        | Request::GetQueuedPrompts { .. }
        | Request::InterruptQueuedPrompt { .. }
        | Request::InterruptQueuedPrompts { .. }
        | Request::ListTools { .. }
        | Request::QueuePrompt { .. }
        | Request::RemoveQueuedPrompt { .. }
        | Request::ReorderQueuedPrompts { .. }
        | Request::ScreenContext { .. }
        | Request::SessionContextBreakdown { .. }
        | Request::SessionShakeTools { .. }
        | Request::SetQueuedPromptEditing { .. }
        | Request::SetQueuedPromptMode { .. }
        | Request::ToolResult { .. } => Route::Early,
        Request::BackgroundTasks { .. }
        | Request::BackgroundTool { .. }
        | Request::Cancel { .. }
        | Request::CancelSoftInterrupts { .. }
        | Request::Message { .. }
        | Request::SoftInterrupt { .. } => Route::Turn,
        Request::Clear { .. }
        | Request::GetState { .. }
        | Request::GetTodos { .. }
        | Request::OpenrigHostIdentity { .. }
        | Request::Ping { .. }
        | Request::PrepareDisconnect { .. }
        | Request::Rewind { .. }
        | Request::RewindTranscript { .. }
        | Request::RewindUndo { .. } => Route::Session,
        Request::DebugCommand { .. }
        | Request::GetCompactedHistory { .. }
        | Request::GetHistory { .. }
        | Request::GetHistoryImageChunk { .. }
        | Request::GetHistoryPage { .. }
        | Request::GetModelCatalog { .. }
        | Request::GetSessionImage { .. }
        | Request::ListSessionImages { .. }
        | Request::Reload { .. }
        | Request::ResumeSession { .. }
        | Request::Subscribe { .. }
        | Request::UpgradeRuntime { .. } => Route::Attach,
        Request::AccountUsage { .. }
        | Request::CancelPendingModelChange { .. }
        | Request::CycleModel { .. }
        | Request::InvalidateAnthropicUsage { .. }
        | Request::InvalidateOpenAiUsage { .. }
        | Request::NotifyAuthChanged { .. }
        | Request::PersistSession { .. }
        | Request::RefreshModels { .. }
        | Request::SetCompactionMode { .. }
        | Request::SetContextCap { .. }
        | Request::SetModel { .. }
        | Request::SetPremiumMode { .. }
        | Request::SetReasoningEffort { .. }
        | Request::SetRoute { .. }
        | Request::SetServiceTier { .. }
        | Request::SetSessionEnv { .. }
        | Request::SetSpeedMode { .. }
        | Request::SetTransport { .. }
        | Request::SwitchAnthropicAccount { .. }
        | Request::SwitchOpenAiAccount { .. } => Route::Settings,
        Request::RuntimeCommand { .. } => Route::RuntimeCommand,
        Request::AgentCapabilities { .. }
        | Request::AgentContext { .. }
        | Request::AgentRegister { .. }
        | Request::AgentTask { .. }
        | Request::AppletAction { .. }
        | Request::AskUserResponse { .. }
        | Request::ClientDebugCommand { .. }
        | Request::ClientDebugResponse { .. }
        | Request::CloseApplet { .. }
        | Request::CommAnswerEscalatedAsk { .. }
        | Request::CommApprovePlan { .. }
        | Request::CommAssignNext { .. }
        | Request::CommAssignRole { .. }
        | Request::CommAssignTask { .. }
        | Request::CommAwaitMembers { .. }
        | Request::CommChannelMembers { .. }
        | Request::CommFocusSession { .. }
        | Request::CommList { .. }
        | Request::CommListChannels { .. }
        | Request::CommListModels { .. }
        | Request::CommListSessions { .. }
        | Request::CommListSwarms { .. }
        | Request::CommMessage { .. }
        | Request::CommPlanStatus { .. }
        | Request::CommProposePlan { .. }
        | Request::CommRead { .. }
        | Request::CommReadContext { .. }
        | Request::CommRejectPlan { .. }
        | Request::CommReport { .. }
        | Request::CommResyncPlan { .. }
        | Request::CommSetSwarmLabel { .. }
        | Request::CommShare { .. }
        | Request::CommSpawn { .. }
        | Request::CommStatus { .. }
        | Request::CommStop { .. }
        | Request::CommSubscribeChannel { .. }
        | Request::CommSummary { .. }
        | Request::CommTaskControl { .. }
        | Request::CommUnsubscribeChannel { .. }
        | Request::CommWorkerControl { .. }
        | Request::Compact { .. }
        | Request::GetSideQuestionHistory { .. }
        | Request::InputShell { .. }
        | Request::ListSideQuestions { .. }
        | Request::NotifySession { .. }
        | Request::PromoteSideQuestion { .. }
        | Request::RenameSession { .. }
        | Request::ResumeAllSessions { .. }
        | Request::RoutingGateAnswer { .. }
        | Request::RoutingIntent { .. }
        | Request::RoutingView { .. }
        | Request::RunSubagent { .. }
        | Request::SetAgentModel { .. }
        | Request::SetFeature { .. }
        | Request::SetSessionSaved { .. }
        | Request::SetSubagentModel { .. }
        | Request::Split { .. }
        | Request::SplitSideQuestion { .. }
        | Request::StdinResponse { .. }
        | Request::Transcript { .. }
        | Request::Transfer { .. }
        | Request::TriggerMemoryExtraction { .. }
        | Request::VoiceActivity { .. } => Route::Actions,
    }
}

impl ClientConnection {
    pub(super) async fn route(
        &mut self,
        request: Request,
        ctx: RequestContext<'_>,
    ) -> Result<Outcome> {
        match route_of(&request) {
            Route::Early => unreachable!("{request:?} is answered before dispatch"),
            Route::Turn => self.turn_requests(request, ctx).await,
            Route::Session => self.session_requests(request).await,
            Route::Attach => self.attach_requests(request, ctx).await,
            Route::Settings => self.settings_requests(request).await,
            Route::RuntimeCommand => self.runtime_command_requests(request).await,
            Route::Actions => self.action_requests(request).await,
        }
    }
}
