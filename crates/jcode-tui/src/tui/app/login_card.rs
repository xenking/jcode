use crate::tui::ui::OAUTH_LOGIN_DETAILS_MARKER;

pub(in crate::tui::app) fn encode_oauth_login_card(
    provider_label: &str,
    account_label: Option<&str>,
    url: &str,
    browser_opened: bool,
    callback_available: bool,
    paste_hint: &str,
    details: &str,
) -> String {
    let browser = if browser_opened {
        "Browser launch requested"
    } else {
        "Open the sign-in link manually"
    };
    let waiting = if callback_available {
        "Waiting for browser approval and local callback"
    } else {
        "Waiting for pasted callback (local listener unavailable)"
    };
    let summary = format!(
        "✓ Sign-in link ready · {browser}\n◌ {waiting}\n[Open {provider_label} sign-in]({url})\n{paste_hint} · /cancel"
    );
    let account_section = account_label
        .map(|label| format!("Account: {label}\n\n"))
        .unwrap_or_default();
    let trimmed_details = details.trim();
    let extra = if trimmed_details.is_empty() {
        String::new()
    } else {
        format!("\n\n{trimmed_details}")
    };
    jcode_tui_messages::encode_collapsible_swarm_content(
        "Sign-in details",
        &format!(
            "{summary}{OAUTH_LOGIN_DETAILS_MARKER}{account_section}Full sign-in URL:\n{url}{extra}"
        ),
    )
}

#[allow(clippy::too_many_arguments)]
pub(in crate::tui::app) fn encode_device_login_card(
    provider_label: &str,
    url: &str,
    user_code: Option<&str>,
    clipboard_copied: bool,
    browser_opened: bool,
    waiting_line: &str,
    footer_line: &str,
    details: &str,
) -> String {
    let headline = if user_code.is_some() {
        if browser_opened {
            "✓ Device code ready · Browser launch requested"
        } else {
            "✓ Device code ready · Open on any browser or device"
        }
    } else if browser_opened {
        "✓ Sign-in link ready · Browser launch requested"
    } else {
        "✓ Sign-in link ready · Open the sign-in link manually"
    };
    let mut summary_lines = vec![headline.to_string()];
    if let Some(code) = user_code {
        if clipboard_copied {
            summary_lines.push(format!("Code: {code}  (copied to clipboard)"));
        } else {
            summary_lines.push(format!("Code: {code}"));
        }
    }
    summary_lines.push(format!("◌ {waiting_line}"));
    summary_lines.push(format!("[Open {provider_label} sign-in]({url})"));
    if !footer_line.is_empty() {
        summary_lines.push(footer_line.to_string());
    }
    let summary = summary_lines.join("\n");
    let trimmed_details = details.trim();
    let extra = if trimmed_details.is_empty() {
        String::new()
    } else {
        format!("\n\n{trimmed_details}")
    };
    jcode_tui_messages::encode_collapsible_swarm_content(
        "Sign-in details",
        &format!("{summary}{OAUTH_LOGIN_DETAILS_MARKER}Full sign-in URL:\n{url}{extra}"),
    )
}
