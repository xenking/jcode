//! TypeScript declaration generation from tool JSON schemas.
//!
//! Mirrors Cloudflare Code Mode: every tool becomes a typed async function on
//! the `tools` object, so the model writes against a typed API instead of raw
//! JSON. Declarations are generated lazily (`types()` / `describeTool`) so a
//! large MCP catalog never inflates the tool description itself.

use super::ToolInfo;
use serde_json::Value;

const MAX_DEPTH: usize = 6;

/// Whether `name` is usable as `tools.<name>` without brackets.
pub fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c == '$' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

fn property_key(name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| format!("\"{name}\""))
    }
}

/// PascalCase type name for a tool's input, e.g. `mcp__fs__read-file` ->
/// `McpFsReadFileInput`.
pub fn input_type_name(tool: &str) -> String {
    let mut out = String::new();
    for part in tool.split(|c: char| !c.is_ascii_alphanumeric()) {
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
            out.extend(chars);
        }
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'T');
    }
    out.push_str("Input");
    out
}

fn comment(text: &str, indent: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let mut out = format!("{indent}/**\n");
    for line in text.lines().take(8) {
        out.push_str(&format!(
            "{indent} * {}\n",
            line.trim().replace("*/", "*\\/")
        ));
    }
    out.push_str(&format!("{indent} */\n"));
    out
}

/// Render a JSON schema as a TypeScript type expression.
pub fn schema_to_ts(schema: &Value) -> String {
    render(schema, 0)
}

fn render(schema: &Value, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return "unknown".into();
    }
    let Some(obj) = schema.as_object() else {
        return if schema == &Value::Bool(false) {
            "never".into()
        } else {
            "unknown".into()
        };
    };
    if let Some(values) = obj.get("enum").and_then(Value::as_array) {
        let parts: Vec<String> = values
            .iter()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "unknown".into()))
            .collect();
        if !parts.is_empty() {
            return parts.join(" | ");
        }
    }
    if let Some(value) = obj.get("const") {
        return serde_json::to_string(value).unwrap_or_else(|_| "unknown".into());
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = obj.get(key).and_then(Value::as_array) {
            let parts: Vec<String> = variants.iter().map(|v| render(v, depth + 1)).collect();
            if !parts.is_empty() {
                return parts.join(" | ");
            }
        }
    }
    if let Some(variants) = obj.get("allOf").and_then(Value::as_array) {
        let parts: Vec<String> = variants.iter().map(|v| render(v, depth + 1)).collect();
        if !parts.is_empty() {
            return parts.join(" & ");
        }
    }
    let ty = obj.get("type");
    if let Some(types) = ty.and_then(Value::as_array) {
        let parts: Vec<String> = types
            .iter()
            .map(|t| {
                let mut single = obj.clone();
                single.insert("type".into(), t.clone());
                render(&Value::Object(single), depth + 1)
            })
            .collect();
        return parts.join(" | ");
    }
    match ty.and_then(Value::as_str) {
        Some("string") => "string".into(),
        Some("number" | "integer") => "number".into(),
        Some("boolean") => "boolean".into(),
        Some("null") => "null".into(),
        Some("array") => {
            let item = obj
                .get("items")
                .map(|i| render(i, depth + 1))
                .unwrap_or_else(|| "unknown".into());
            if item.contains(' ') {
                format!("Array<{item}>")
            } else {
                format!("{item}[]")
            }
        }
        Some("object") | None if obj.contains_key("properties") || ty.is_some() => {
            render_object(obj, depth)
        }
        _ => "unknown".into(),
    }
}

fn render_object(obj: &serde_json::Map<String, Value>, depth: usize) -> String {
    let required: Vec<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let indent = "  ".repeat(depth + 1);
    let mut out = String::from("{\n");
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (name, prop) in props {
            if let Some(desc) = prop.get("description").and_then(Value::as_str) {
                out.push_str(&comment(desc, &indent));
            }
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            out.push_str(&format!(
                "{indent}{}{optional}: {};\n",
                property_key(name),
                render(prop, depth + 1)
            ));
        }
    }
    match obj.get("additionalProperties") {
        Some(Value::Bool(false)) => {}
        Some(extra @ Value::Object(_)) => {
            out.push_str(&format!(
                "{indent}[key: string]: {};\n",
                render(extra, depth + 1)
            ));
        }
        _ if obj.get("properties").is_none() => {
            out.push_str(&format!("{indent}[key: string]: unknown;\n"));
        }
        _ => {}
    }
    out.push_str(&"  ".repeat(depth));
    out.push('}');
    out
}

/// `declare const tools: { ... }` for every tool whose name matches.
pub fn declarations<'a>(tools: impl IntoIterator<Item = &'a ToolInfo>) -> String {
    let mut types = String::new();
    let mut members = String::new();
    for tool in tools {
        let type_name = input_type_name(&tool.name);
        types.push_str(&format!(
            "type {type_name} = {};\n",
            schema_to_ts(&tool.input_schema)
        ));
        members.push_str(&comment(first_paragraph(&tool.description), "  "));
        members.push_str(&format!(
            "  {}(args: {type_name}): Promise<any>;\n",
            property_key(&tool.name)
        ));
    }
    format!("{types}\ndeclare const tools: {{\n{members}}};\n")
}

fn first_paragraph(text: &str) -> &str {
    text.split("\n\n").next().unwrap_or("").trim()
}
