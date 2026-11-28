//! A deliberately small, non-evaluating subset of A2UI Express.
//!
//! The entire program is one assignment, `root = Macro("arg", ...)`, with
//! JSON string literals and whitespace. No variables, expressions, nested calls,
//! URLs, host actions, or general A2UI Express conformance are provided.
//! Input is limited to 64 KiB. Supported calls are:
//!
//! - `ReviewChange(title, body, change_id)`: a card with an optional note
//!   input bound to `note` and review buttons. Both actions carry `change_id`.
//! - `GreetingButton(label, action, recipient)`: a button whose custom action
//!   carries `recipient`. Action names are dot-separated ASCII identifiers and
//!   cannot start with `host.`.
//! - `TaskTable(title)`: read-only native table bound to this session's tasks.
//!
//! All arguments above are JSON strings. Input state is sent by the host along
//! with actions; the note is not copied into action arguments.

use crate::view::{Action, ButtonVariant, Node, NodeKind, TextStyle, Tone, View};
use serde_json::{Value, json};

const MAX_SOURCE_BYTES: usize = 64 * 1024;

// The tool advertises exactly the calls this dispatcher accepts.
macro_rules! catalog {
    ($($name:ident($($arg:ident),+) => $expand:ident);+ $(;)?) => {
        pub const TOOL_HINT: &str = concat!(
            "Use plain text for explanations. Use native Desktop UI when a choice or editable input helps. ",
            "Prefer express: root = Macro(\"...\", ...). ",
            $(stringify!($name), "(", stringify!($($arg),+), "). ",)+
            "Set wait=true for the action and state. For custom layouts use view; action=catalog gives node shapes."
        );

        fn expand(name: &str, parser: &mut Parser<'_>) -> Result<View, String> {
            match name {
                $(stringify!($name) => {
                    $(let $arg = parser.string()?;)+
                    $expand([$($arg),+])
                },)+
                _ => Err("unknown Express macro; use action=catalog for supported calls".into()),
            }
        }
    }
}

catalog! {
    ReviewChange(title, body, change_id) => review;
    GreetingButton(label, event, recipient) => greeting;
    TaskTable(title) => tasks;
}

fn tasks([title]: [String; 1]) -> Result<View, String> {
    let mut node = Node::new(NodeKind::SessionTasks { title }).keyed("root");
    node.fallback = Some("Live tasks require a compatible Jcode Desktop transcript.".into());
    Ok(node)
}

fn review([title, body, change_id]: [String; 3]) -> Result<View, String> {
    Ok(review_change(title, body, change_id))
}

fn greeting([label, event, recipient]: [String; 3]) -> Result<View, String> {
    if !valid_action(&event) {
        return Err("invalid custom action name".into());
    }
    Ok(button(
        "root",
        label,
        ButtonVariant::Primary,
        event,
        json!({"recipient": recipient}),
    ))
}

/// Compile the documented, bounded Express subset into native keyed nodes.
pub fn compile_express(source: &str) -> Result<View, String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err("Express source exceeds 64 KiB".into());
    }
    let mut parser = Parser {
        rest: source,
        has_arg: false,
    };
    if parser.identifier()? != "root" {
        return Err("expected root assignment".into());
    }
    parser.symbol('=')?;
    let name = parser.identifier()?;
    parser.symbol('(')?;
    let view = expand(name, &mut parser)?;
    parser.symbol(')')?;
    if !parser.rest.trim().is_empty() {
        return Err("unexpected trailing input".into());
    }
    Ok(view)
}

fn valid_action(name: &str) -> bool {
    !name.starts_with("host.")
        && name.split('.').all(|part| {
            let mut chars = part.bytes();
            matches!(chars.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
                && chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

fn button(key: &str, label: String, variant: ButtonVariant, action: String, args: Value) -> Node {
    Node::new(NodeKind::Button {
        label,
        variant,
        icon: None,
        on_press: Action { action, args },
        disabled: false,
    })
    .keyed(key)
}

fn review_change(title: String, body: String, change_id: String) -> Node {
    let args = json!({"change_id": change_id});
    Node::new(NodeKind::Card {
        title: Some(title),
        children: vec![
            Node::new(NodeKind::Text {
                text: body,
                style: TextStyle::Body,
                tone: Tone::Default,
                max_lines: None,
                selectable: true,
            })
            .keyed("body"),
            Node::new(NodeKind::Input {
                bind: "note".into(),
                placeholder: "Optional note".into(),
                multiline: true,
                on_submit: None,
            })
            .keyed("note"),
            button(
                "approve",
                "Approve".into(),
                ButtonVariant::Primary,
                "review.approve".into(),
                args.clone(),
            ),
            button(
                "request_changes",
                "Request changes".into(),
                ButtonVariant::Secondary,
                "review.request_changes".into(),
                args,
            ),
        ],
    })
    .keyed("root")
}

struct Parser<'a> {
    rest: &'a str,
    has_arg: bool,
}

impl<'a> Parser<'a> {
    fn whitespace(&mut self) {
        self.rest = self.rest.trim_start();
    }

    fn identifier(&mut self) -> Result<&'a str, String> {
        self.whitespace();
        let len = self
            .rest
            .bytes()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_')
            .count();
        if len == 0 || self.rest.as_bytes()[0].is_ascii_digit() {
            return Err("expected identifier".into());
        }
        let (identifier, rest) = self.rest.split_at(len);
        self.rest = rest;
        Ok(identifier)
    }

    fn symbol(&mut self, symbol: char) -> Result<(), String> {
        self.whitespace();
        self.rest = self
            .rest
            .strip_prefix(symbol)
            .ok_or_else(|| format!("expected {symbol}"))?;
        Ok(())
    }

    fn string(&mut self) -> Result<String, String> {
        if self.has_arg {
            self.symbol(',')?;
        }
        self.has_arg = true;
        self.whitespace();
        if !self.rest.starts_with('"') {
            return Err("expected JSON string".into());
        }
        let mut stream = serde_json::Deserializer::from_str(self.rest).into_iter::<String>();
        let value = stream
            .next()
            .ok_or("expected JSON string")?
            .map_err(|err| format!("invalid JSON string: {err}"))?;
        self.rest = &self.rest[stream.byte_offset()..];
        Ok(value)
    }
}
