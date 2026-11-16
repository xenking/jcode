//! Bounded wire pages, reassembled only at record boundaries for the UI.
use crate::{
    ApiEvent, ApiRequest, HistoryMessage, JcodeClient, RenderedImage, RenderedImageAnchor,
};
use crate::{Error, ErrorKind, Result};
use serde::Deserialize;

fn protocol(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Protocol, message)
}

#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub messages: Vec<HistoryMessage>,
    pub images: Vec<RenderedImage>,
    pub before: Option<String>,
    /// Genuine user prompts preceding the first message, if supplied by the server.
    pub prompt_offset: Option<usize>,
}

#[derive(Deserialize)]
struct Fragment {
    index: usize,
    start: usize,
    total: usize,
    text: String,
}
#[derive(Deserialize)]
struct Record {
    message: HistoryMessage,
    images: Vec<RenderedImage>,
    #[serde(default)]
    prompt_offset: Option<usize>,
}

pub(crate) fn page(
    client: &JcodeClient,
    session_id: &str,
    before: Option<String>,
) -> Result<HistoryPage> {
    let mut cursor = before;
    let mut rows: Vec<String> = Vec::new();
    let mut partial: Option<(usize, usize, usize, Vec<String>)> = None;
    loop {
        let frame = client.request(ApiRequest::GetHistoryPage {
            session_id: session_id.into(),
            before: cursor.clone(),
            limit: 128,
        })?;
        let (fragments, next) = match frame.event {
            ApiEvent::HistoryPage {
                fragments, before, ..
            } => (fragments, before),
            ApiEvent::Error { code, message } => {
                return Err(Error::new(ErrorKind::Harness(code), message));
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::UnexpectedReply,
                    "Unexpected history page reply",
                ));
            }
        };
        if fragments.is_empty() && next.is_some() {
            return Err(protocol("Empty history page with cursor"));
        }
        for value in fragments {
            let frag: Fragment =
                serde_json::from_value(value).map_err(|e| protocol(e.to_string()))?;
            if frag
                .start
                .checked_add(frag.text.len())
                .is_none_or(|end| end > frag.total)
            {
                return Err(protocol("Invalid history fragment"));
            }
            if let Some((index, start, total, mut pieces)) = partial.take() {
                if frag.index != index
                    || frag.total != total
                    || frag.start + frag.text.len() != start
                {
                    return Err(protocol("Discontinuous history fragment"));
                }
                pieces.push(frag.text);
                if frag.start == 0 {
                    pieces.reverse();
                    rows.push(pieces.concat());
                } else {
                    partial = Some((index, frag.start, total, pieces));
                }
            } else {
                if frag.start + frag.text.len() != frag.total {
                    return Err(protocol("Incomplete history fragment tail"));
                }
                if frag.start == 0 {
                    rows.push(frag.text);
                } else {
                    partial = Some((frag.index, frag.start, frag.total, vec![frag.text]));
                }
            }
        }
        cursor = next;
        if partial.is_none() {
            break;
        }
        if cursor.is_none() {
            return Err(protocol("Truncated history record"));
        }
    }
    rows.reverse();
    let mut messages = Vec::with_capacity(rows.len());
    let mut images = Vec::new();
    let mut users = 0;
    let mut prompt_offset = None;
    for row in rows {
        let record: Record = serde_json::from_str(&row)
            .map_err(|e| protocol(format!("Invalid history record: {e}")))?;
        if messages.is_empty() {
            prompt_offset = record.prompt_offset;
        }
        for mut image in record.images {
            if let Some(index) = &mut image.history_message_index {
                *index += messages.len();
            }
            if let Some(RenderedImageAnchor::UserPrompt { ordinal }) = &mut image.anchor {
                *ordinal += users;
            }
            images.push(image);
        }
        if record.message.role == "user" {
            users += 1;
        }
        messages.push(record.message);
    }
    Ok(HistoryPage {
        messages,
        images,
        before: cursor,
        prompt_offset,
    })
}

pub(crate) fn image(client: &JcodeClient, session_id: &str, key: &str) -> Result<String> {
    let mut result = String::new();
    loop {
        let frame = client.request(ApiRequest::GetHistoryImageChunk {
            session_id: session_id.into(),
            key: key.into(),
            offset: result.len(),
        })?;
        match frame.event {
            ApiEvent::HistoryImageChunk {
                key: actual,
                offset,
                total,
                data,
                ..
            } => {
                if actual != key
                    || offset != result.len()
                    || data.is_empty() && offset < total
                    || offset.checked_add(data.len()).is_none_or(|end| end > total)
                {
                    return Err(protocol("Invalid image chunk"));
                }
                result.push_str(&data);
                if result.len() == total {
                    return Ok(result);
                }
            }
            ApiEvent::Error { code, message } => {
                return Err(Error::new(ErrorKind::Harness(code), message));
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::UnexpectedReply,
                    "Unexpected image chunk reply",
                ));
            }
        }
    }
}
