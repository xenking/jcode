//! The one token estimator: per-model-family, content-aware, dependency free.
//!
//! A flat "4 chars per token" rule is wrong in both directions: on the
//! recorded sessions it under-counted Anthropic context by ~38% and
//! over-counted OpenAI context by ~18% (median), and it charged Cyrillic text
//! by UTF-8 bytes. Instead every character is classified once (letters, word
//! starts, digits, whitespace, punctuation, Cyrillic, CJK, other) and the
//! per-class counts are weighted by a family table fitted against
//! provider-reported usage (see `docs/TOKEN_ESTIMATION.md`). The model is
//! linear, so estimates are monotonic and (up to rounding and a word split at
//! the seam) additive.
//!
//! Estimates are only for decisions made before a provider reports usage. When
//! a reported count exists for the same content, use it and estimate only the
//! delta since.

use std::fmt;

/// Tokenizer family. Pick it from the model name; anything unknown is
/// [`TokenFamily::Generic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TokenFamily {
    Anthropic,
    OpenAi,
    DeepSeek,
    /// No recorded usage yet: estimated with the generic table.
    Gemini,
    #[default]
    Generic,
}

impl TokenFamily {
    pub fn from_model(model: &str) -> Self {
        let model = model.to_ascii_lowercase();
        let name = model.rsplit('/').next().unwrap_or(&model);
        let o_series = name
            .strip_prefix('o')
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()));
        if model.contains("claude") {
            Self::Anthropic
        } else if model.contains("gemini") {
            Self::Gemini
        } else if model.contains("deepseek") {
            Self::DeepSeek
        } else if o_series
            || name.starts_with("gpt")
            || name.starts_with("chatgpt")
            || model.contains("codex")
        {
            Self::OpenAi
        } else {
            Self::Generic
        }
    }

    fn calibration(self) -> &'static Calibration {
        match self {
            Self::Anthropic => &ANTHROPIC,
            Self::OpenAi => &OPENAI,
            Self::DeepSeek => &DEEPSEEK,
            Self::Gemini | Self::Generic => &GENERIC,
        }
    }

    /// Tokens the system prompt and tool catalog typically add on top of the
    /// message list, for use when neither has been measured.
    pub fn system_overhead_tokens(self) -> usize {
        self.calibration().system_overhead
    }

    /// Tokens for content known only by its size in bytes (no text to count),
    /// at the typical prose/code/tool-output mix. Coarser than [`Estimator`];
    /// use it only when the text is not available.
    pub fn tokens_for_bytes(self, bytes: usize) -> usize {
        bytes.saturating_mul(10) / self.calibration().bytes_per_token_tenths
    }

    /// Byte length of the longest prefix of `text` that estimates to at most
    /// `tokens`, on a char boundary.
    pub fn prefix_within(self, text: &str, tokens: usize) -> usize {
        let mut estimator = Estimator::new(self);
        let mut buf = [0u8; 4];
        for (at, ch) in text.char_indices() {
            estimator.scan(ch.encode_utf8(&mut buf));
            if estimator.tokens() > tokens {
                return at;
            }
        }
        text.len()
    }

    /// Bytes of mixed prose/code/tool output that fit in `tokens`. Use it to
    /// turn a token budget into a truncation cap; it is a lower bound, so
    /// truncating to it errs toward fitting.
    pub fn bytes_for_tokens(self, tokens: usize) -> usize {
        tokens.saturating_mul(self.calibration().bytes_per_token_tenths) / 10
    }
}

/// Milli-tokens per unit. Fitted by `scripts/` data in `docs/TOKEN_ESTIMATION.md`.
struct Calibration {
    /// Indexed by [`Class`].
    text: [u32; CLASSES],
    /// Per byte of reasoning/thinking text, thinking signature, opaque
    /// encrypted reasoning and provider-native JSON.
    reasoning: u32,
    signature: u32,
    encrypted: u32,
    native: u32,
    /// Per text block and per tool call (the call plus its result), covering
    /// the ids and framing the provider adds.
    text_block: u32,
    tool_call: u32,
    system_overhead: usize,
    bytes_per_token_tenths: usize,
}

const CLASSES: usize = 10;

#[derive(Clone, Copy)]
enum Class {
    Letters,
    Words,
    Digits,
    Space,
    SpaceRuns,
    Punct,
    Cyrillic,
    CyrillicWords,
    Cjk,
    Other,
}

// CJK weights are priors (little data); the Gemini row is the generic table.
const ANTHROPIC: Calibration = Calibration {
    text: [337, 141, 873, 78, 114, 900, 0, 2395, 1500, 1293],
    reasoning: 1,
    signature: 111,
    encrypted: 2,
    native: 250,
    text_block: 10_795,
    tool_call: 114_759,
    system_overhead: 31_000,
    bytes_per_token_tenths: 23,
};
const OPENAI: Calibration = Calibration {
    text: [84, 471, 761, 26, 447, 580, 73, 838, 1100, 1160],
    reasoning: 373,
    signature: 124,
    encrypted: 2,
    native: 250,
    text_block: 14_411,
    tool_call: 63_311,
    system_overhead: 17_500,
    bytes_per_token_tenths: 34,
};
const DEEPSEEK: Calibration = Calibration {
    text: [150, 430, 833, 43, 128, 545, 58, 1592, 900, 810],
    reasoning: 252,
    signature: 149,
    encrypted: 2,
    native: 250,
    text_block: 14_913,
    tool_call: 103_290,
    system_overhead: 19_000,
    bytes_per_token_tenths: 34,
};
const GENERIC: Calibration = Calibration {
    text: [122, 574, 838, 12, 265, 580, 0, 2176, 1200, 1211],
    reasoning: 240,
    signature: 149,
    encrypted: 2,
    native: 250,
    text_block: 13_373,
    tool_call: 93_787,
    system_overhead: 18_000,
    bytes_per_token_tenths: 30,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Run {
    None,
    Letter,
    Digit,
    Space,
    Punct,
    Cyrillic,
    Cjk,
    Other,
}

/// Accumulates one request's worth of content. Add every piece, then read
/// [`Estimator::tokens`]. Implements [`fmt::Write`] so serializers can stream
/// into it without building a `String`.
pub struct Estimator {
    cal: &'static Calibration,
    counts: [u64; CLASSES],
    extra_milli: u64,
    bytes_fed: usize,
    run: Run,
    any: bool,
}

impl Estimator {
    pub fn new(family: TokenFamily) -> Self {
        Self {
            cal: family.calibration(),
            counts: [0; CLASSES],
            extra_milli: 0,
            bytes_fed: 0,
            run: Run::None,
            any: false,
        }
    }

    /// Ordinary text: prose, code, tool output, JSON.
    pub fn text(&mut self, text: &str) -> &mut Self {
        self.run = Run::None;
        self.scan(text);
        self
    }

    /// Reasoning or thinking text that is replayed to the provider.
    pub fn reasoning(&mut self, text: &str) -> &mut Self {
        self.bytes(text.len(), self.cal.reasoning)
    }

    /// Provider signature over a thinking block.
    pub fn signature(&mut self, bytes: usize) -> &mut Self {
        self.bytes(bytes, self.cal.signature)
    }

    /// Opaque encrypted reasoning payload.
    pub fn encrypted(&mut self, bytes: usize) -> &mut Self {
        self.bytes(bytes, self.cal.encrypted)
    }

    /// Provider-native JSON (for example search results) of this many bytes.
    pub fn native_json(&mut self, bytes: usize) -> &mut Self {
        self.bytes(bytes, self.cal.native)
    }

    /// Framing of one text block.
    pub fn text_block(&mut self) -> &mut Self {
        self.fixed_milli(self.cal.text_block as u64)
    }

    /// Framing of one tool call and its result.
    pub fn tool_call(&mut self) -> &mut Self {
        self.fixed_milli(self.cal.tool_call as u64)
    }

    /// A cost already known in tokens (images, native compaction blobs).
    pub fn fixed_tokens(&mut self, tokens: usize) -> &mut Self {
        self.fixed_milli((tokens as u64).saturating_mul(1000))
    }

    /// Bytes of text and opaque payload fed so far (excludes fixed costs).
    pub fn bytes_fed(&self) -> usize {
        self.bytes_fed
    }

    pub fn tokens(&self) -> usize {
        let weighted: u64 = self
            .counts
            .iter()
            .zip(self.cal.text)
            .map(|(count, weight)| count.saturating_mul(u64::from(weight)))
            .sum();
        let milli = weighted.saturating_add(self.extra_milli);
        let tokens = usize::try_from(milli.saturating_add(500) / 1000).unwrap_or(usize::MAX);
        if self.any { tokens.max(1) } else { 0 }
    }

    fn bytes(&mut self, bytes: usize, milli_per_byte: u32) -> &mut Self {
        self.bytes_fed = self.bytes_fed.saturating_add(bytes);
        self.fixed_milli((bytes as u64).saturating_mul(u64::from(milli_per_byte)))
    }

    fn fixed_milli(&mut self, milli: u64) -> &mut Self {
        self.any |= milli > 0;
        self.extra_milli = self.extra_milli.saturating_add(milli);
        self
    }

    fn scan(&mut self, text: &str) {
        self.any |= !text.is_empty();
        self.bytes_fed = self.bytes_fed.saturating_add(text.len());
        let mut run = self.run;
        let c = &mut self.counts;
        for ch in text.chars() {
            let next = if ch.is_ascii() {
                if ch.is_ascii_alphabetic() {
                    c[Class::Letters as usize] += 1;
                    c[Class::Words as usize] += u64::from(run != Run::Letter);
                    Run::Letter
                } else if ch.is_ascii_digit() {
                    c[Class::Digits as usize] += 1;
                    Run::Digit
                } else if ch.is_whitespace() {
                    c[Class::Space as usize] += 1;
                    c[Class::SpaceRuns as usize] += u64::from(run != Run::Space);
                    Run::Space
                } else {
                    c[Class::Punct as usize] += 1;
                    Run::Punct
                }
            } else if ('\u{400}'..='\u{4ff}').contains(&ch) {
                c[Class::Cyrillic as usize] += 1;
                c[Class::CyrillicWords as usize] += u64::from(run != Run::Cyrillic);
                Run::Cyrillic
            } else if is_cjk(ch) {
                c[Class::Cjk as usize] += 1;
                Run::Cjk
            } else if ch.is_whitespace() {
                c[Class::Space as usize] += 1;
                c[Class::SpaceRuns as usize] += u64::from(run != Run::Space);
                Run::Space
            } else {
                c[Class::Other as usize] += 1;
                Run::Other
            };
            run = next;
        }
        self.run = run;
    }

    /// Feed pre-counted classes (used by the fixture test).
    #[cfg(test)]
    fn from_counts(family: TokenFamily, counts: [u64; CLASSES]) -> Self {
        let mut estimator = Self::new(family);
        estimator.counts = counts;
        estimator.any = true;
        estimator
    }
}

impl fmt::Write for Estimator {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.scan(text);
        Ok(())
    }
}

fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3000..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF)
}

/// Tokens for a plain string.
pub fn estimate_text(family: TokenFamily, text: &str) -> usize {
    Estimator::new(family).text(text).tokens()
}

#[cfg(test)]
#[path = "token_estimate_tests.rs"]
mod tests;
