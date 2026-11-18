use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use jcode_session_types::{Attachment, StoredAttachment};
use std::{
    io::{Read, Write},
    path::Path,
};

const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 20 * 1024 * 1024;

pub(crate) struct PreparedAttachments {
    pub text: String,
    pub images: Vec<(String, String)>,
    pub items: Vec<StoredAttachment>,
}

/// Snapshot path-based files before acknowledging a queued prompt, so later
/// changes or removal of a client-local file cannot change its contents.
pub(super) fn normalize(attachments: Vec<Attachment>) -> Result<Vec<Attachment>> {
    let mut total = 0usize;
    let mut normalized = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        match attachment {
            Attachment::File {
                name,
                mime,
                size,
                data,
                path,
            } => {
                ensure!(
                    !name.is_empty() && !name.chars().any(char::is_control),
                    "Invalid attachment name"
                );
                ensure!(
                    !mime.is_empty() && !mime.chars().any(char::is_control),
                    "Invalid attachment MIME type"
                );
                ensure!(
                    size <= MAX_FILE_BYTES as u64,
                    "Attachment exceeds size limit"
                );
                let bytes = match (data, path) {
                    (Some(data), None) => {
                        ensure!(
                            data.len() <= MAX_FILE_BYTES.div_ceil(3) * 4,
                            "Attachment exceeds size limit"
                        );
                        STANDARD.decode(data).context("Invalid attachment base64")?
                    }
                    (None, Some(path)) => {
                        ensure!(
                            std::fs::metadata(&path)?.is_file(),
                            "Attachment path is not a file"
                        );
                        let mut options = std::fs::OpenOptions::new();
                        options.read(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            options.custom_flags(libc::O_NONBLOCK);
                        }
                        let file = options.open(path).context("Cannot open attachment")?;
                        ensure!(file.metadata()?.is_file(), "Attachment path is not a file");
                        let mut bytes = Vec::new();
                        file.take(MAX_FILE_BYTES as u64 + 1)
                            .read_to_end(&mut bytes)?;
                        bytes
                    }
                    _ => anyhow::bail!("Attachment needs exactly one of data or path"),
                };
                ensure!(
                    bytes.len() == size as usize,
                    "Attachment size does not match content"
                );
                total = total
                    .checked_add(bytes.len())
                    .context("Attachment size overflow")?;
                ensure!(
                    total <= MAX_TOTAL_BYTES,
                    "Attachments exceed total size limit"
                );
                normalized.push(Attachment::File {
                    name,
                    mime,
                    size,
                    data: Some(STANDARD.encode(bytes)),
                    path: None,
                });
            }
            Attachment::PastedText { text } => {
                total = total
                    .checked_add(text.len())
                    .context("Attachment size overflow")?;
                ensure!(
                    total <= MAX_TOTAL_BYTES,
                    "Attachments exceed total size limit"
                );
                normalized.push(Attachment::PastedText { text });
            }
            Attachment::Unknown => {}
        }
    }
    Ok(normalized)
}

pub(crate) fn prepare(dir: &Path, attachments: Vec<Attachment>) -> Result<PreparedAttachments> {
    let mut total = 0usize;
    let mut decoded = Vec::new();
    for attachment in attachments {
        match attachment {
            Attachment::File {
                name,
                mime,
                size,
                data,
                path,
            } => {
                ensure!(
                    !name.is_empty() && !name.chars().any(char::is_control),
                    "Invalid attachment name"
                );
                ensure!(
                    !mime.is_empty() && !mime.chars().any(char::is_control),
                    "Invalid attachment MIME type"
                );
                ensure!(
                    size <= MAX_FILE_BYTES as u64,
                    "Attachment exceeds size limit"
                );
                let bytes = match (data, path) {
                    (Some(data), None) => {
                        ensure!(
                            data.len() <= MAX_FILE_BYTES.div_ceil(3) * 4,
                            "Attachment exceeds size limit"
                        );
                        STANDARD.decode(data).context("Invalid attachment base64")?
                    }
                    (None, Some(path)) => {
                        ensure!(
                            std::fs::metadata(&path)?.is_file(),
                            "Attachment path is not a file"
                        );
                        let mut options = std::fs::OpenOptions::new();
                        options.read(true);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            options.custom_flags(libc::O_NONBLOCK);
                        }
                        let file = options.open(path).context("Cannot open attachment")?;
                        ensure!(file.metadata()?.is_file(), "Attachment path is not a file");
                        let mut bytes = Vec::new();
                        file.take(MAX_FILE_BYTES as u64 + 1)
                            .read_to_end(&mut bytes)?;
                        bytes
                    }
                    _ => anyhow::bail!("Attachment needs exactly one of data or path"),
                };
                ensure!(
                    bytes.len() == size as usize,
                    "Attachment size does not match content"
                );
                total = total
                    .checked_add(bytes.len())
                    .context("Attachment size overflow")?;
                ensure!(
                    total <= MAX_TOTAL_BYTES,
                    "Attachments exceed total size limit"
                );
                decoded.push((name, mime, bytes, false));
            }
            Attachment::PastedText { text } => {
                total = total
                    .checked_add(text.len())
                    .context("Attachment size overflow")?;
                ensure!(
                    total <= MAX_TOTAL_BYTES,
                    "Attachments exceed total size limit"
                );
                decoded.push((
                    "Pasted text".into(),
                    "text/plain".into(),
                    text.into_bytes(),
                    true,
                ));
            }
            Attachment::Unknown => {}
        }
    }
    let mut result = PreparedAttachments {
        text: String::new(),
        images: Vec::new(),
        items: Vec::new(),
    };
    for (name, mime, bytes, pasted) in decoded {
        if pasted {
            let text = String::from_utf8(bytes)?;
            result.text.push_str(&format!("\n\n[Pasted text]\n{text}"));
            result.items.push(StoredAttachment::PastedText { text });
            continue;
        }
        let mut saved_path = None;
        if mime.starts_with("image/") {
            result.text.push_str(&format!(
                "\n\n[Attached image: {name} ({mime}, {} bytes)]",
                bytes.len()
            ));
            result.images.push((mime.clone(), STANDARD.encode(&bytes)));
        } else if (mime.starts_with("text/")
            || matches!(
                mime.as_str(),
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/x-yaml"
            )
            || is_code_filename(&name))
            && std::str::from_utf8(&bytes).is_ok()
        {
            result.text.push_str(&format!(
                "\n\n[Attachment: {name} ({mime}, {} bytes)]\n{}",
                bytes.len(),
                std::str::from_utf8(&bytes)?
            ));
        } else {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(dir)?;
            let path = dir.join(uuid::Uuid::new_v4().to_string());
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options.open(&path)?.write_all(&bytes)?;
            result.text.push_str(&format!(
                "\n\n[Attachment: {name} ({mime}, {} bytes), saved at {}]",
                bytes.len(),
                path.display()
            ));
            saved_path = Some(path.to_string_lossy().into_owned());
        }
        result.items.push(StoredAttachment::File {
            name,
            mime,
            size: bytes.len() as u64,
            path: saved_path,
        });
    }
    Ok(result)
}

fn is_code_filename(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "rs" | "py"
                    | "js"
                    | "jsx"
                    | "ts"
                    | "tsx"
                    | "go"
                    | "c"
                    | "h"
                    | "cc"
                    | "cpp"
                    | "hpp"
                    | "java"
                    | "kt"
                    | "swift"
                    | "rb"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "sql"
                    | "html"
                    | "css"
                    | "json"
                    | "yaml"
                    | "yml"
                    | "toml"
                    | "md"
                    | "txt"
                    | "xml"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    include!("attachment_tests.rs");
}
