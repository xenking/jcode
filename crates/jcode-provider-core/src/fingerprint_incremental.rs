use super::*;
use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const RESPONSES_FIELDS: &[&str] = &[
    "context_management",
    "include",
    "input",
    "instructions",
    "model",
    "parallel_tool_calls",
    "prompt_cache_key",
    "prompt_cache_retention",
    "reasoning",
    "service_tier",
    "tool_choice",
    "tools",
];

/// Each provider instance gets its own prefix checkpoint. Checkpoints share
/// the immutable normalized request with transport and never copy its JSON.
#[derive(Debug)]
pub struct ResponsesFingerprintCache(u64);

impl Default for ResponsesFingerprintCache {
    fn default() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

// We keep the prior Value only as an exact equality witness, not an encoded
// copy or a pointer key. Limit retained requests across all provider forks.
// Weight includes serialized bytes plus a per-item allowance for Value nodes.
const CACHE_WEIGHT_LIMIT: usize = 32 * 1024 * 1024;
const CACHE_ENTRY_LIMIT: usize = 32;
static PREFIXES: Mutex<PrefixCache> = Mutex::new(PrefixCache {
    entries: VecDeque::new(),
    weight: 0,
});

struct PrefixCache {
    entries: VecDeque<(u64, CachedPrefix)>,
    weight: usize,
}

impl PrefixCache {
    fn take(&mut self, id: u64) -> Option<CachedPrefix> {
        let index = self.entries.iter().position(|(key, _)| *key == id)?;
        let (_, entry) = self.entries.remove(index)?;
        self.weight -= entry.weight;
        Some(entry)
    }

    fn insert(&mut self, id: u64, entry: CachedPrefix, evicted: &mut Vec<CachedPrefix>) {
        if entry.weight > CACHE_WEIGHT_LIMIT {
            evicted.push(entry);
            return;
        }
        evicted.extend(self.take(id));
        while self.weight + entry.weight > CACHE_WEIGHT_LIMIT
            || self.entries.len() >= CACHE_ENTRY_LIMIT
        {
            let (_, removed) = self.entries.pop_front().expect("cache over budget");
            self.weight -= removed.weight;
            evicted.push(removed);
        }
        self.weight += entry.weight;
        self.entries.push_back((id, entry));
    }
}

impl Drop for ResponsesFingerprintCache {
    fn drop(&mut self) {
        self.clear();
    }
}

impl ResponsesFingerprintCache {
    pub fn clear(&self) {
        drop(
            PREFIXES
                .lock()
                .ok()
                .and_then(|mut cache| cache.take(self.0)),
        );
    }

    /// Fingerprint effective full input once, sharing item hashes with the WS
    /// prefix validator. Request, array and item hashes remain SHA-256 of the
    /// exact legacy compact JSON bytes, even when the wire request is a delta.
    pub fn log_request(
        &self,
        model: &str,
        request: &Arc<Value>,
        stable_item_count: usize,
        tool_count: usize,
        extra_fields: &[(&str, String)],
    ) -> Vec<u64> {
        let items = request["input"].as_array().map_or(&[][..], Vec::as_slice);
        let fingerprints = self.fingerprints(request, stable_item_count);
        capture_request_input(
            "openai",
            model,
            "openai_responses_full",
            &fingerprints,
            items,
        );
        log_canonical_fingerprints(
            "openai",
            model,
            "openai_responses_full",
            &fingerprints,
            items.len(),
            Some(tool_count),
            extra_fields,
        );
        fingerprints.items.0
    }

    pub(super) fn fingerprints(
        &self,
        request: &Arc<Value>,
        stable_item_count: usize,
    ) -> CanonicalFingerprints {
        let previous = PREFIXES
            .lock()
            .ok()
            .and_then(|mut cache| cache.take(self.0));
        if let Ok((fingerprints, checkpoint)) = encode_request(request, stable_item_count, previous)
        {
            // Free evicted request trees outside the global lock.
            let mut evicted = Vec::new();
            if let Some(checkpoint) = checkpoint
                && let Ok(mut cache) = PREFIXES.lock()
            {
                cache.insert(self.0, checkpoint, &mut evicted);
            }
            return fingerprints;
        }

        // Value serialization cannot normally fail. Keep the legacy fallback.
        let empty_tools = Value::Array(Vec::new());
        let fields = RESPONSES_FIELDS
            .iter()
            .map(|key| (*key, request.get(key).unwrap_or(&Value::Null)));
        let items = request["input"].as_array().map_or(&[][..], Vec::as_slice);
        object_fingerprints(
            fields,
            items,
            request.get("instructions"),
            Some(request.get("tools").unwrap_or(&empty_tools)),
        )
        .unwrap_or_else(|| CanonicalFingerprints {
            request: (stable_hash_str(""), 0),
            items: item_fingerprints(items),
            system: request.get("instructions").map(stable_json_fingerprint),
            tools: Some(stable_json_fingerprint(
                request.get("tools").unwrap_or(&empty_tools),
            )),
        })
    }
}

struct CachedPrefix {
    witness: Arc<Value>,
    request: JsonDigest,
    input: JsonDigest,
    hashes: Vec<u64>,
    weight: usize,
}

fn same_json_encoding(left: &Value, right: &Value) -> bool {
    match (left, right) {
        // Value equality alone considers -0.0 equal to 0.0. Their bytes differ.
        (Value::Number(left), Value::Number(right)) => {
            left == right && left.as_f64().map(f64::to_bits) == right.as_f64().map(f64::to_bits)
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(l, r)| same_json_encoding(l, r))
        }
        // Compare iteration order as well, including with preserve_order.
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|((lk, lv), (rk, rv))| lk == rk && same_json_encoding(lv, rv))
        }
        _ => left == right,
    }
}

#[derive(Clone)]
struct JsonDigest {
    context: Context,
    len: usize,
}

impl Default for JsonDigest {
    fn default() -> Self {
        Self {
            context: Context::new(&SHA256),
            len: 0,
        }
    }
}

impl JsonDigest {
    fn update(&mut self, bytes: &[u8]) {
        #[cfg(test)]
        hash_work::record(bytes.len());
        self.context.update(bytes);
        self.len += bytes.len();
    }

    fn finish(self) -> (u64, usize) {
        (hash_prefix(self.context.finish().as_ref()), self.len)
    }
}

#[derive(Default)]
struct Digests {
    request: JsonDigest,
    input: Option<JsonDigest>,
    field: Option<JsonDigest>,
}

impl Write for Digests {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.request.update(bytes);
        if let Some(input) = &mut self.input {
            input.update(bytes);
        }
        if let Some(field) = &mut self.field {
            field.update(bytes);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type Encoder = BufWriter<Digests>;

fn fingerprint_field(encoder: &mut Encoder, value: &Value) -> std::io::Result<(u64, usize)> {
    encoder.flush()?;
    encoder.get_mut().field = Some(JsonDigest::default());
    serde_json::to_writer(&mut *encoder, value)?;
    encoder.flush()?;
    Ok(encoder
        .get_mut()
        .field
        .take()
        .expect("active field digest")
        .finish())
}

fn encode_items(
    encoder: &mut Encoder,
    items: &[Value],
    offset: usize,
    hashes: &mut Vec<u64>,
) -> std::io::Result<()> {
    for (index, item) in items.iter().enumerate() {
        if index + offset != 0 {
            encoder.write_all(b",")?;
        }
        hashes.push(fingerprint_field(encoder, item)?.0);
    }
    Ok(())
}

fn encode_request(
    request: &Arc<Value>,
    stable_item_count: usize,
    mut previous: Option<CachedPrefix>,
) -> std::io::Result<(CanonicalFingerprints, Option<CachedPrefix>)> {
    let items = request["input"].as_array().map_or(&[][..], Vec::as_slice);
    let stable_item_count = stable_item_count.min(items.len());
    let mut checkpoint = None;
    let mut encoder = BufWriter::with_capacity(8 * 1024, Digests::default());
    let mut system = None;
    let mut tools = None;
    let mut input = None;
    encoder.write_all(b"{")?;
    for (index, key) in RESPONSES_FIELDS.iter().enumerate() {
        if index != 0 {
            encoder.write_all(b",")?;
        }
        serde_json::to_writer(&mut encoder, key)?;
        encoder.write_all(b":")?;
        let value = &request[*key];
        match *key {
            "input" if value.is_array() => {
                encoder.flush()?;
                let mut hashes = Vec::with_capacity(items.len());
                if let Some(previous) = previous.take().filter(|previous| {
                    let count = previous.hashes.len();
                    count <= stable_item_count
                        && RESPONSES_FIELDS[..index]
                            .iter()
                            .all(|key| same_json_encoding(&previous.witness[*key], &request[*key]))
                        && previous.witness["input"]
                            .as_array()
                            .expect("cached input is an array")[..count]
                            .iter()
                            .zip(items)
                            .all(|(old, new)| same_json_encoding(old, new))
                }) {
                    encoder.get_mut().request = previous.request;
                    encoder.get_mut().input = Some(previous.input);
                    hashes.extend(previous.hashes);
                } else {
                    encoder.get_mut().input = Some(JsonDigest::default());
                    encoder.write_all(b"[")?;
                }

                let offset = hashes.len();
                encode_items(
                    &mut encoder,
                    &items[offset..stable_item_count],
                    offset,
                    &mut hashes,
                )?;
                encoder.flush()?;
                if stable_item_count != 0 {
                    checkpoint = Some(CachedPrefix {
                        witness: Arc::clone(request),
                        request: encoder.get_ref().request.clone(),
                        input: encoder
                            .get_ref()
                            .input
                            .clone()
                            .expect("active input digest"),
                        hashes: hashes.clone(),
                        weight: 0,
                    });
                }
                encode_items(
                    &mut encoder,
                    &items[stable_item_count..],
                    stable_item_count,
                    &mut hashes,
                )?;
                encoder.write_all(b"]")?;
                encoder.flush()?;
                let hash = encoder
                    .get_mut()
                    .input
                    .take()
                    .expect("active input digest")
                    .finish()
                    .0;
                input = Some((hashes, hash));
            }
            "instructions" if request.get(key).is_some() => {
                system = Some(fingerprint_field(&mut encoder, value)?);
            }
            "tools" if request.get(key).is_some() => {
                tools = Some(fingerprint_field(&mut encoder, value)?);
            }
            _ => serde_json::to_writer(&mut encoder, value)?,
        }
    }
    encoder.write_all(b"}")?;
    encoder.flush()?;
    let request = encoder.get_ref().request.clone().finish();
    if let Some(checkpoint) = checkpoint.as_mut() {
        checkpoint.weight = request.1.saturating_add(items.len().saturating_mul(1024));
    }
    Ok((
        CanonicalFingerprints {
            request,
            items: input.unwrap_or_else(|| item_fingerprints(items)),
            system,
            tools: tools.or_else(|| Some(stable_json_fingerprint(&[] as &[Value]))),
        },
        checkpoint,
    ))
}
