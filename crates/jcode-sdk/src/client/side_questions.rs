use super::*;

impl JcodeClient {
    /// Clone a session's complete persisted context into a new idle session.
    pub fn transfer_session(&self, session_id: &str) -> Result<SessionInfo> {
        match self
            .request_ok(ApiRequest::TransferSession {
                session_id: session_id.into(),
            })?
            .event
        {
            ApiEvent::SessionForked { session } => Ok(session),
            other => Err(unexpected("session_forked", &other)),
        }
    }

    pub fn fork_session(&self, session_id: &str) -> Result<SessionInfo> {
        match self
            .request_ok(ApiRequest::ForkSession {
                session_id: session_id.to_string(),
            })?
            .event
        {
            ApiEvent::SessionForked { session } => Ok(session),
            other => Err(unexpected("session_forked", &other)),
        }
    }

    fn side_question_request(&self, request: ApiRequest) -> Result<ApiEvent> {
        if !self.supports(crate::SIDE_QUESTIONS_CAPABILITY) {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "This runtime does not support side questions.",
            ));
        }
        self.request_ok(request).map(|frame| frame.event)
    }

    pub fn fork_side_question(&self, parent_session_id: &str) -> Result<crate::SideQuestionTopic> {
        match self.side_question_request(ApiRequest::ForkSideQuestion {
            session_id: parent_session_id.into(),
        })? {
            ApiEvent::SideQuestionForked { topic } => Ok(topic),
            other => Err(unexpected("side_question_forked", &other)),
        }
    }

    pub fn list_side_questions(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<crate::SideQuestionTopic>> {
        match self.side_question_request(ApiRequest::ListSideQuestions {
            parent_session_id: parent_session_id.into(),
        })? {
            ApiEvent::SideQuestions { topics } => Ok(topics),
            other => Err(unexpected("side_questions", &other)),
        }
    }

    pub fn get_side_question_history(&self, session_id: &str) -> Result<Vec<HistoryMessage>> {
        match self.side_question_request(ApiRequest::GetSideQuestionHistory {
            session_id: session_id.into(),
        })? {
            ApiEvent::History { messages, .. } => Ok(messages),
            other => Err(unexpected("history", &other)),
        }
    }

    pub fn promote_side_question(&self, session_id: &str) -> Result<()> {
        match self.side_question_request(ApiRequest::PromoteSideQuestion {
            session_id: session_id.into(),
        })? {
            ApiEvent::Ok => Ok(()),
            other => Err(unexpected("ok", &other)),
        }
    }
}
