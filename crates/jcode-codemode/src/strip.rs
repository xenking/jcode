//! TypeScript type stripping for codemode scripts.
//!
//! The sandbox is QuickJS, which runs JavaScript and nothing else, but
//! [`crate::ts`] hands the model TypeScript declarations for every tool, and
//! models write what they are shown: `const x: string`, `interface`, `as`
//! casts, generics, `satisfies`. Each of those used to be a syntax error that
//! read like a broken sandbox rather than a type annotation.
//!
//! Deno and Bun both answer this without a type checker: types are erased, not
//! checked, before evaluation, because the types were only ever hints for the
//! model. This module does the same in-process with oxc's parser, semantic
//! builder, transformer and codegen, all linked into the harness binary. No
//! `tsc`, no Node, no subprocess, no network.
//!
//! Two properties matter as much as correctness:
//!
//! * Plain JavaScript must reach QuickJS byte for byte, so its stack traces
//!   point at the lines the model wrote. [`strip`] parses as JavaScript first
//!   and returns the input untouched when that succeeds, which is also cheaper
//!   than the full transform.
//! * Line numbers must survive the rewrite. Codegen reprints the whole program,
//!   so [`Stripped`] carries the codegen source map as a stripped-line to
//!   original-line table and rewrites the positions QuickJS reports back to the
//!   script the model actually wrote ([`Stripped::remap_error`]).

use std::path::PathBuf;

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_diagnostics::OxcDiagnostic;
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};

/// Name given to the script in diagnostics and in the codegen source map.
const SOURCE_NAME: &str = "codemode.ts";

/// A script whose types have been erased.
#[derive(Debug, Clone)]
pub struct Stripped {
    /// JavaScript ready for QuickJS.
    pub code: String,
    /// `lines[i]` is the 1-based line of the input that 1-based line `i + 1` of
    /// [`Self::code`] came from. Empty means the input was already JavaScript
    /// and `code` is byte-identical to it.
    lines: Vec<u32>,
}

impl Stripped {
    /// Line in the caller's source that `line` of [`Self::code`] came from.
    pub fn original_line(&self, line: u32) -> u32 {
        let index = line.saturating_sub(1) as usize;
        match self.lines.get(index) {
            Some(&mapped) if mapped > 0 => mapped,
            _ => line,
        }
    }

    /// Rewrite the `file:line:col` positions in a QuickJS stack back to the
    /// script the model wrote. `line_offset` is how many lines the engine
    /// prepended when it wrapped the body in an async function, so pass `0`
    /// when the script was evaluated unwrapped.
    pub fn remap_error(&self, message: &str, line_offset: u32) -> String {
        let mut out = String::with_capacity(message.len());
        let bytes = message.as_bytes();
        let mut copied = 0;
        let mut cursor = 0;
        while let Some(found) = message[cursor..].find(':') {
            let at = cursor + found;
            // `:` is one byte, so `at + 1` is always a character boundary.
            let Some((consumed, line, column)) = parse_position(bytes, at) else {
                cursor = at + 1;
                continue;
            };
            let line = if line > line_offset {
                self.original_line(line - line_offset)
            } else {
                line
            };
            out.push_str(&message[copied..at]);
            out.push_str(&format!(":{line}:{column}"));
            copied = at + consumed;
            cursor = copied;
        }
        out.push_str(&message[copied..]);
        out
    }
}

/// Match `:<line>:<col>` starting at `start` (which must be a `:`). Returns the
/// bytes consumed and the parsed numbers.
fn parse_position(bytes: &[u8], start: usize) -> Option<(usize, u32, u32)> {
    let digits = |from: usize| -> Option<(usize, u32)> {
        let mut end = from;
        let mut value: u32 = 0;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            value = value
                .saturating_mul(10)
                .saturating_add(u32::from(bytes[end] - b'0'));
            end += 1;
        }
        (end > from).then_some((end, value))
    };
    let (line_end, line) = digits(start + 1)?;
    if bytes.get(line_end) != Some(&b':') {
        return None;
    }
    let (col_end, column) = digits(line_end + 1)?;
    Some((col_end - start, line, column))
}

/// A script that could not be turned into JavaScript, with the position the
/// model has to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripError {
    /// 1-based line in the script the model wrote.
    pub line: u32,
    /// 1-based column in the script the model wrote.
    pub column: u32,
    pub message: String,
}

impl std::fmt::Display for StripError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}:{}: {}", self.line, self.column, self.message)
    }
}

impl std::error::Error for StripError {}

/// Erase TypeScript types from `source`.
///
/// Plain JavaScript is returned unchanged. Anything else is parsed as
/// TypeScript, has its types removed, and is reprinted as JavaScript with a
/// line map back to `source`.
pub fn strip(source: &str) -> Result<Stripped, StripError> {
    let allocator = Allocator::default();
    let (_, plain_javascript_failure) = parse(&allocator, source, SourceType::mjs());
    if plain_javascript_failure.is_none() {
        return Ok(Stripped {
            code: source.to_string(),
            lines: Vec::new(),
        });
    }

    let (mut program, failure) = parse(&allocator, source, SourceType::ts());
    if let Some(diagnostic) = failure {
        return Err(at_position(source, &diagnostic));
    }

    // The TypeScript pass needs real scope information: enums and namespaces
    // rewrite references, and the transformer panics on an empty `Scoping`.
    // `enum_eval` precomputes member values; without it the enum lowering
    // silently emits wrong reverse mappings.
    let scoping = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic
        .into_scoping();
    let options = TransformOptions::default();
    let transformed = Transformer::new(&allocator, std::path::Path::new(SOURCE_NAME), &options)
        .build_with_scoping(scoping, &mut program);
    if let Some(diagnostic) = transformed
        .diagnostics
        .iter()
        .find(|d| d.severity == oxc_diagnostics::Severity::Error)
    {
        return Err(at_position(source, diagnostic));
    }

    let generated = Codegen::new()
        .with_options(CodegenOptions {
            source_map_path: Some(PathBuf::from(SOURCE_NAME)),
            ..CodegenOptions::default()
        })
        .build(&program);
    let lines = match &generated.map {
        Some(map) => {
            let table = map.generate_lookup_table();
            let count = generated.code.lines().count();
            let mut lines = Vec::with_capacity(count);
            let mut previous = 1;
            for line in 0..count as u32 {
                let mapped = map
                    .lookup_token_approx(&table, line, 0)
                    .map(|token| token.get_src_line() + 1)
                    .unwrap_or(previous);
                previous = mapped;
                lines.push(mapped);
            }
            lines
        }
        // Only reachable if the source map feature is off; positions then stay
        // as printed, which is still a readable script.
        None => Vec::new(),
    };

    Ok(Stripped {
        code: generated.code,
        lines,
    })
}

/// Parse `source` as `source_type`, returning the program and the first error
/// diagnostic, if any.
///
/// `allow_return_outside_function` matches the sandbox: the engine wraps the
/// script in `(async () => { ... })()`, so `return` at the top of the body is
/// legal even though it is illegal in a bare module.
fn parse<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    source_type: SourceType,
) -> (Program<'a>, Option<OxcDiagnostic>) {
    let parsed = Parser::new(allocator, source, source_type)
        .with_options(ParseOptions {
            allow_return_outside_function: true,
            ..ParseOptions::default()
        })
        .parse();
    let failure = parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == oxc_diagnostics::Severity::Error)
        .cloned();
    (parsed.program, failure)
}

/// Turn an oxc diagnostic into a position in the model's script.
fn at_position(source: &str, diagnostic: &OxcDiagnostic) -> StripError {
    let offset = diagnostic
        .labels
        .iter()
        .next()
        .map(|label| label.span().start)
        .unwrap_or(0);
    let (line, column) = line_column(source, offset);
    StripError {
        line,
        column,
        message: diagnostic.message.trim_end_matches('.').to_string(),
    }
}

/// 1-based line and column of a byte offset.
fn line_column(source: &str, offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(source.len());
    let before = source.get(..offset).unwrap_or_default();
    let line = before.matches('\n').count() as u32 + 1;
    let column = before
        .rfind('\n')
        .map_or(before.chars().count(), |newline| {
            before[newline + 1..].chars().count()
        }) as u32
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(source: &str) -> String {
        strip(source).expect("script must strip").code
    }

    #[test]
    fn plain_javascript_passes_through_byte_for_byte() {
        let source = "const a = 1;\nconsole.log(`x ${a}`);\nreturn a + 1;  // keep\n";
        let stripped = strip(source).unwrap();
        assert_eq!(stripped.code, source);
        assert!(stripped.lines.is_empty(), "no map needed for a no-op");
        assert_eq!(stripped.original_line(7), 7);
    }

    #[test]
    fn strips_type_annotations() {
        let out = code("const name: string = 'x';\nconst n: number = 1;\nreturn [name, n];");
        assert!(!out.contains(": string"), "{out}");
        assert!(!out.contains(": number"), "{out}");
    }

    #[test]
    fn strips_interfaces_and_type_aliases() {
        let out = code(
            "interface A { x: string }\ntype B = { y: number };\nconst a: A = { x: 'x' };\nreturn a.x;",
        );
        assert!(!out.contains("interface"), "{out}");
        assert!(!out.contains("type B"), "{out}");
        assert!(out.contains("const a = {"), "{out}");
    }

    #[test]
    fn strips_as_casts_and_satisfies() {
        let out = code(
            "const a = { x: 1 } as Record<string, number>;\nconst b = { y: 2 } satisfies { y: number };\nreturn [a, b];",
        );
        assert!(!out.contains("as Record"), "{out}");
        assert!(!out.contains("satisfies"), "{out}");
    }

    #[test]
    fn strips_non_null_assertions() {
        let out = code("const m = new Map();\nconst v = m.get('k')!;\nreturn v!.toString();");
        assert!(!out.contains(")!"), "{out}");
        assert!(!out.contains("v!"), "{out}");
    }

    #[test]
    fn strips_generics_on_calls() {
        let out = code(
            "const xs = new Map<string, number>();\nxs.set('a', 1);\nreturn await tools.search<{ q: string }>({ q: 'a' });",
        );
        assert!(!out.contains("<string, number>"), "{out}");
        assert!(!out.contains("<{ q: string }>"), "{out}");
        assert!(out.contains("tools.search({"), "{out}");
    }

    #[test]
    fn strips_optional_parameters() {
        let out = code(
            "function f(a?: string, b: number = 2): string { return `${a}${b}`; }\nreturn f();",
        );
        assert!(out.contains("function f(a, b = 2)"), "{out}");
        assert!(!out.contains(": string"), "{out}");
    }

    #[test]
    fn compiles_enums_to_javascript() {
        let out = code("enum Mode { Fast, Slow }\nreturn Mode.Fast;");
        assert!(!out.contains("enum"), "{out}");
        assert!(out.contains("var Mode"), "{out}");
    }

    #[test]
    fn keeps_top_level_await_and_return() {
        let source = "const x: number = await tools.echo({ a: 1 });\nreturn x;";
        let out = code(source);
        assert!(out.contains("await tools.echo"), "{out}");
        assert!(out.contains("return x"), "{out}");
    }

    #[test]
    fn syntax_errors_report_a_position() {
        let err = strip("const a = ;\n").unwrap_err();
        assert_eq!(err.line, 1, "{err}");
        assert!(err.column > 1, "{err}");
        assert!(err.to_string().starts_with("line 1:"), "{err}");
        assert!(!err.message.is_empty(), "{err}");
    }

    #[test]
    fn syntax_errors_report_the_line_after_type_removal_candidates() {
        let err = strip("type A = string;\n\nconst a: A = ;\n").unwrap_err();
        assert_eq!(err.line, 3, "{err}");
    }

    #[test]
    fn line_numbers_survive_stripping() {
        let source = "type A = string;\ninterface B { x: A }\nconst a: A = 'x';\nconsole.log(a);\nreturn a;\n";
        let stripped = strip(source).unwrap();
        let last = stripped.code.lines().last().unwrap();
        assert!(last.contains("return"), "{}", stripped.code);
        // The stripped program has fewer lines, and the map points the last
        // one back at the model's own last line.
        assert!(stripped.code.lines().count() < source.lines().count());
        let last_line_of_code = stripped.code.lines().count() as u32;
        assert_eq!(stripped.original_line(last_line_of_code), 5);
    }

    #[test]
    fn remaps_error_positions_back_to_the_model_script() {
        let source =
            "type A = string;\ninterface B { x: A }\nconst a: A = 'x';\nnull.boom(a);\nreturn a;\n";
        let stripped = strip(source).unwrap();
        // Line 3 of the evaluated source is line 2 of the stripped script,
        // which is line 4 of what the model wrote: two type lines are gone.
        let remapped = stripped.remap_error("Error: boom\n    at <eval> (codemode.ts:3:5)", 1);
        assert!(remapped.contains(":4:5"), "{remapped}");
    }

    #[test]
    fn remap_leaves_the_plain_javascript_path_alone() {
        let source = "null.boom(1);\n";
        let stripped = strip(source).unwrap();
        // One wrapper line prepended: line 2 of the evaluated script is line 1.
        assert_eq!(
            stripped.remap_error("(codemode.ts:2:3)", 1),
            "(codemode.ts:1:3)"
        );
    }

    #[test]
    fn remap_keeps_messages_without_positions() {
        let stripped = strip("const a: number = 1;\nreturn a;").unwrap();
        assert_eq!(stripped.remap_error("Error: nope", 1), "Error: nope");
    }

    #[test]
    fn remap_keeps_multibyte_text_intact() {
        let stripped = strip("const s: string = 'да';\nnull.boom(s);").unwrap();
        let remapped = stripped.remap_error("Error: не найдено — (codemode.ts:3:5)", 1);
        assert_eq!(remapped, "Error: не найдено — (codemode.ts:2:5)");
    }
}
