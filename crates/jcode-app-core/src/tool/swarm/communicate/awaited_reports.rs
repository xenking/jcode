use super::{
    AwaitedMemberStatus, HashMap, REQUEST_ID, Request, ServerEvent, ToolContext, check_error,
    latest_assistant_report, send_request,
};

pub(super) async fn fetch_awaited_member_reports(
    ctx: &ToolContext,
    members: &[AwaitedMemberStatus],
) -> HashMap<String, String> {
    let mut reports = HashMap::new();
    for member in members
        .iter()
        .filter(|member| member.done && member.completion_report.is_none())
    {
        let request = Request::CommReadContext {
            id: REQUEST_ID,
            session_id: ctx.session_id.clone(),
            target_session: member.session_id.clone(),
        };
        match send_request(request).await {
            Ok(ServerEvent::CommContextHistory { messages, .. }) => {
                if let Some(report) = latest_assistant_report(&messages) {
                    reports.insert(member.session_id.clone(), report);
                }
            }
            Ok(response) => {
                if check_error(&response).is_some() {
                    continue;
                }
            }
            Err(_) => continue,
        }
    }
    reports
}
