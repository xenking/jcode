use std::path::Path;

pub(super) fn split(raw: &str) -> Vec<String> {
    shlex::split(raw).unwrap_or_else(|| raw.split_whitespace().map(str::to_owned).collect())
}

pub(super) fn render(
    mut rest: &str,
    raw: &str,
    names: &[String],
    skill_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> String {
    let args = split(raw);
    let mut out = String::with_capacity(rest.len());
    while let Some(dollar) = rest.find('$') {
        let preceding = &rest[..dollar];
        rest = &rest[dollar + 1..];
        if let Some(literal) = preceding.strip_suffix('\\') {
            out.push_str(literal);
            out.push('$');
            continue;
        }
        out.push_str(preceding);
        if let Some((key, after)) = rest.strip_prefix('{').and_then(|text| text.split_once('}')) {
            let path = match key {
                "JCODE_SKILL_DIR" | "CLAUDE_SKILL_DIR" => Some(skill_dir),
                "JCODE_PROJECT_DIR" | "CLAUDE_PROJECT_DIR" => Some(project_dir),
                _ => None,
            };
            if let Some(path) = path {
                if let Some(path) = path {
                    out.push_str(&path.to_string_lossy());
                }
                rest = after;
                continue;
            }
        }
        let end = rest
            .bytes()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == b'_')
            .count();
        let (key, suffix) = rest.split_at(end);
        if key == "ARGUMENTS" {
            if let Some((index, after)) = suffix.strip_prefix('[').and_then(|s| s.split_once(']'))
                && !index.is_empty()
                && index.bytes().all(|ch| ch.is_ascii_digit())
            {
                if let Some(arg) = index.parse::<usize>().ok().and_then(|i| args.get(i)) {
                    out.push_str(arg);
                } else {
                    out.push_str("$ARGUMENTS[");
                    out.push_str(index);
                    out.push(']');
                }
                rest = after;
                continue;
            }
            out.push_str(raw);
        } else if let Some(index) = names.iter().position(|name| name == key) {
            if let Some(arg) = args.get(index) {
                out.push_str(arg);
            }
        } else if let Some(arg) = key.parse::<usize>().ok().and_then(|i| args.get(i)) {
            out.push_str(arg);
        } else {
            out.push('$');
            out.push_str(key);
        }
        rest = suffix;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_bind_once_without_touching_unknown_variables() {
        assert_eq!(
            render(
                r"$0 $ARGUMENTS[9] $ARGUMENTS $first $empty \$0 $unknown ${CLAUDE_SKILL_DIR} ${JCODE_PROJECT_DIR} 😀$",
                "'$first with spaces'",
                &["first".into(), "empty".into()],
                Some(Path::new("/installed/skill")),
                Some(Path::new("/project")),
            ),
            "$first with spaces $ARGUMENTS[9] '$first with spaces' $first with spaces  $0 $unknown /installed/skill /project 😀$",
        );
    }
}
