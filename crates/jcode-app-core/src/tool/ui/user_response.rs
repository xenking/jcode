//! User-facing Markdown responses rendered inline by Desktop from tool input.
use super::{Tool, ToolContext, ToolOutput};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

pub(super) struct UserResponseTool;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    content: String,
    title: Option<String>,
    #[serde(rename = "intent")]
    _intent: Option<String>,
    #[serde(rename = "accept_large_output")]
    _accept_large_output: Option<bool>,
}

#[async_trait]
impl Tool for UserResponseTool {
    fn name(&self) -> &str {
        "user_response"
    }

    fn description(&self) -> &str {
        "Reply in Desktop Markdown. Do not duplicate as assistant text."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "content": {"type": "string", "description": "The complete user-visible response as Markdown."},
                "title": {"type": "string", "description": "Optional response heading."},
                "intent": super::intent_schema_property()
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolOutput> {
        super::interactive::require_desktop(
            &ctx.session_id,
            super::interactive::DesktopTool::UserResponse,
        )?;
        let input: Input = serde_json::from_value(input)?;
        let mut output = ToolOutput::new(input.content);
        output.title = input.title;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ToolContext {
        ToolContext {
            session_id: "user-response-test".into(),
            message_id: "msg".into(),
            tool_call_id: "call".into(),
            working_dir: None,
            stdin_request_tx: None,
            graceful_shutdown_signal: None,
            execution_mode: crate::tool::ToolExecutionMode::AgentTurn,
        }
    }

    #[tokio::test]
    async fn user_response_preserves_markdown_and_optional_title() {
        let _desktop = crate::tool::interactive::attach_desktop(&context().session_id);
        for title in [None, Some("Summary")] {
            let content = "# Answer\n\n**Markdown** with Unicode: 日本語\n";
            let result = UserResponseTool
                .execute(
                    json!({
                        "content": content, "title": title,
                        "intent": "Respond", "accept_large_output": null
                    }),
                    context(),
                )
                .await
                .unwrap();
            assert_eq!(result.output, content);
            assert_eq!(result.title.as_deref(), title);
            assert!(result.metadata.is_none());
            assert!(result.images.is_empty());
        }
        let result = UserResponseTool
            .execute(json!({"content": ""}), context())
            .await
            .unwrap();
        assert_eq!(result.output, "");
        assert!(result.title.is_none());
    }

    #[tokio::test]
    async fn user_response_rejects_invalid_input() {
        // Use a distinct session so other tests cannot grant this call access.
        let mut ctx = context();
        ctx.session_id.push_str("-invalid");
        let _desktop = crate::tool::interactive::attach_desktop(&ctx.session_id);
        for input in [
            json!({}),
            json!({"content": null}),
            json!({"content": 42}),
            json!({"content": "ok", "title": 42}),
            json!({"content": "ok", "unexpected": true}),
        ] {
            assert!(UserResponseTool.execute(input, ctx.clone()).await.is_err());
        }
    }

    #[test]
    fn user_response_builtin_schema() {
        let registry = crate::tool::Registry::empty();
        let mut base = crate::tool::Registry::base_tools(&registry.skills);
        let def = base.remove("user_response").unwrap().to_definition();
        assert_eq!(def.input_schema["required"], json!(["content", "intent"]));
        assert_eq!(def.input_schema["additionalProperties"], false);
        assert!(def.description.contains("Do not duplicate"));
    }
}
