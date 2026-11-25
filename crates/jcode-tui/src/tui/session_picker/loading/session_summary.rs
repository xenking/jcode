use super::*;

#[derive(Deserialize)]
pub(super) struct SessionSummary {
    #[serde(default)]
    pub(super) parent_id: Option<String>,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) custom_title: Option<String>,
    pub(super) created_at: chrono::DateTime<chrono::Utc>,
    pub(super) updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub(super) last_active_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub(super) messages: SessionMessageSummaryData,
    #[serde(default)]
    pub(super) working_dir: Option<String>,
    #[serde(default)]
    pub(super) short_name: Option<String>,
    #[serde(default)]
    pub(super) provider_key: Option<String>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) is_canary: bool,
    #[serde(default)]
    pub(super) is_debug: bool,
    /// `/btw` side-question forks stay off the list until promoted.
    #[serde(default)]
    pub(super) hidden: bool,
    #[serde(default)]
    pub(super) saved: bool,
    #[serde(default)]
    pub(super) save_label: Option<String>,
    #[serde(default)]
    pub(super) status: SessionStatus,
}

#[derive(Clone, Debug, Default)]
pub(super) struct SessionMessageSummaryData {
    pub(super) visible_message_count: usize,
    pub(super) user_message_count: usize,
    pub(super) assistant_message_count: usize,
    pub(super) estimated_tokens: usize,
    pub(super) first_user_prompt: Option<String>,
    /// A fork boundary overrides the opening intent of earlier snapshot/journal data.
    pub(super) saw_fork_marker: bool,
    pub(super) search_text: SourceBuckets,
    /// Newest stored message timestamp, of any role (prompts, replies, tools).
    pub(super) last_message_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl SessionMessageSummaryData {
    pub(super) fn add_message(&mut self, message: &SessionMessageSummary) {
        if let Some(ts) = message.timestamp {
            self.last_message_at = Some(self.last_message_at.map_or(ts, |cur| cur.max(ts)));
        }
        if message.is_fork_marker {
            self.first_user_prompt = None;
            self.saw_fork_marker = true;
        }
        if !summary_message_is_visible_conversation(message) {
            return;
        }

        self.visible_message_count += 1;
        match message.role {
            Role::User => {
                self.user_message_count += 1;
                if self.first_user_prompt.is_none() {
                    self.first_user_prompt = message.content_text.clone();
                }
            }
            Role::Assistant => self.assistant_message_count += 1,
        }
        if let Some(usage) = &message.token_usage {
            self.estimated_tokens = self
                .estimated_tokens
                .saturating_add(usage.total_tokens() as usize);
        }
        if let Some(content) = &message.content_search {
            self.search_text
                .merge(content, INITIAL_TRANSCRIPT_SEARCH_BUDGET_BYTES);
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        self.last_message_at = match (self.last_message_at, other.last_message_at) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.visible_message_count = self
            .visible_message_count
            .saturating_add(other.visible_message_count);
        self.user_message_count = self
            .user_message_count
            .saturating_add(other.user_message_count);
        self.assistant_message_count = self
            .assistant_message_count
            .saturating_add(other.assistant_message_count);
        self.estimated_tokens = self.estimated_tokens.saturating_add(other.estimated_tokens);
        if other.saw_fork_marker || self.first_user_prompt.is_none() {
            self.first_user_prompt = other.first_user_prompt;
        }
        self.saw_fork_marker |= other.saw_fork_marker;
        self.search_text
            .merge(&other.search_text, INITIAL_TRANSCRIPT_SEARCH_BUDGET_BYTES);
    }
}

impl<'de> Deserialize<'de> for SessionMessageSummaryData {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(SessionMessageSummaryDataVisitor)
    }
}

pub(super) struct SessionMessageSummaryDataVisitor;

impl<'de> Visitor<'de> for SessionMessageSummaryDataVisitor {
    type Value = SessionMessageSummaryData;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("session message summary array")
    }

    fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut counts = SessionMessageSummaryData::default();
        loop {
            let Some(message) = seq.next_element_seed(SessionMessageSummarySeed {
                // Keep sampling each turn even after the session-wide index is
                // full. `add_message` retains a bounded head + moving tail.
                content_excerpt_budget: MESSAGE_SEARCH_EXCERPT_BYTES,
            })?
            else {
                break;
            };
            counts.add_message(&message);
        }
        Ok(counts)
    }
}

pub(super) struct SessionMessageSummarySeed {
    pub(super) content_excerpt_budget: usize,
}

impl<'de> DeserializeSeed<'de> for SessionMessageSummarySeed {
    type Value = SessionMessageSummary;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(SessionMessageSummaryVisitor {
            content_excerpt_budget: self.content_excerpt_budget,
        })
    }
}

pub(super) struct SessionMessageSummary {
    pub(super) role: Role,
    // `/resume` only needs role/display/token metadata for the initial list.
    // Borrowing content as `RawValue` lets serde skip nested content without
    // allocating or walking it. After display metadata is known, we inspect only
    // the raw prefix for old snapshots that predate `display_role`.
    pub(super) content_starts_with_system_reminder: bool,
    pub(super) is_fork_marker: bool,
    /// Bounded per-source samples of the content, classified by block type.
    pub(super) content_search: Option<SourceBuckets>,
    pub(super) content_text: Option<String>,
    pub(super) display_role: Option<StoredDisplayRole>,
    pub(super) token_usage: Option<SessionTokenUsageSummary>,
    pub(super) timestamp: Option<chrono::DateTime<chrono::Utc>>,
}

impl<'de> Deserialize<'de> for SessionMessageSummary {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_map(SessionMessageSummaryVisitor {
            content_excerpt_budget: MESSAGE_SEARCH_EXCERPT_BYTES,
        })
    }
}

pub(super) struct SessionMessageSummaryVisitor {
    pub(super) content_excerpt_budget: usize,
}

impl<'de> Visitor<'de> for SessionMessageSummaryVisitor {
    type Value = SessionMessageSummary;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("session message summary")
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut role: Option<Role> = None;
        let mut content: Option<&'de RawValue> = None;
        let mut display_role: Option<StoredDisplayRole> = None;
        let mut token_usage: Option<SessionTokenUsageSummary> = None;
        let mut timestamp: Option<chrono::DateTime<chrono::Utc>> = None;

        while let Some(key) = map.next_key::<Cow<'de, str>>()? {
            match key.as_ref() {
                "role" => {
                    role = Some(map.next_value()?);
                }
                "timestamp" => {
                    timestamp = map.next_value()?;
                }
                "content" => {
                    content = Some(map.next_value()?);
                }
                "display_role" => {
                    display_role = map.next_value()?;
                }
                "token_usage" => {
                    token_usage = map.next_value()?;
                }
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }

        let role = role.ok_or_else(|| serde::de::Error::missing_field("role"))?;
        // Inspect explicit fork reminders even when display_role hides them.
        // Decode the text so escaped newlines and block-form content work alike.
        let is_fork_marker = matches!(role, Role::User)
            && content.is_some_and(raw_content_starts_with_system_reminder)
            && content
                .and_then(raw_value_display_text)
                .is_some_and(|text| {
                    text.trim_start()
                        .strip_prefix("<system-reminder>")
                        .is_some_and(|body| {
                            body.trim_start()
                                .starts_with("This session was forked (split) from session ")
                        })
                });
        let conversational_role =
            matches!(display_role, None | Some(StoredDisplayRole::JarvisUser));
        let content_starts_with_system_reminder = matches!(role, Role::User)
            && display_role.is_none()
            && content.is_some_and(raw_content_starts_with_system_reminder);
        let (content_search, content_text) =
            if conversational_role && !content_starts_with_system_reminder {
                (
                    content.and_then(|raw| {
                        classified_search_excerpt(raw, &role, self.content_excerpt_budget)
                    }),
                    content.and_then(raw_value_display_text),
                )
            } else {
                (None, None)
            };
        Ok(SessionMessageSummary {
            role,
            content_starts_with_system_reminder,
            is_fork_marker,
            content_search,
            content_text,
            display_role,
            token_usage,
            timestamp,
        })
    }
}

pub(super) fn summary_message_is_visible_conversation(message: &SessionMessageSummary) -> bool {
    matches!(
        message.display_role,
        None | Some(StoredDisplayRole::JarvisUser)
    ) && !message.content_starts_with_system_reminder
}

pub(super) fn raw_content_starts_with_system_reminder(raw: &RawValue) -> bool {
    let raw = raw.get().trim_start();
    json_string_raw_starts_with_system_reminder(raw)
        || first_text_field_raw_starts_with_system_reminder(raw)
}

pub(super) fn json_string_raw_starts_with_system_reminder(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix('"') else {
        return false;
    };

    rest.trim_start().starts_with("<system-reminder>")
}

pub(super) fn first_text_field_raw_starts_with_system_reminder(raw: &str) -> bool {
    const RAW_SYSTEM_REMINDER_SEARCH_BYTES: usize = 2048;
    let mut end = raw.len().min(RAW_SYSTEM_REMINDER_SEARCH_BYTES);
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    let haystack = &raw[..end];
    let mut search_start = 0;
    while let Some(relative_key_idx) = haystack[search_start..].find("\"text\"") {
        let key_idx = search_start + relative_key_idx;
        let previous = haystack[..key_idx]
            .chars()
            .rev()
            .find(|ch| !ch.is_whitespace());
        let after_key = haystack[key_idx + "\"text\"".len()..].trim_start();
        if matches!(previous, Some('{') | Some(','))
            && let Some(after_colon) = after_key.strip_prefix(':')
        {
            return json_string_raw_starts_with_system_reminder(after_colon.trim_start());
        }

        search_start = key_idx + "\"text\"".len();
    }

    false
}

#[derive(Deserialize)]
pub(super) struct SessionTokenUsageSummary {
    pub(super) input_tokens: u64,
    pub(super) output_tokens: u64,
    #[serde(default)]
    pub(super) cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub(super) cache_creation_input_tokens: Option<u64>,
}

impl SessionTokenUsageSummary {
    pub(super) fn total_tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens.unwrap_or(0)
            + self.cache_creation_input_tokens.unwrap_or(0)
    }
}

#[derive(Deserialize)]
pub(super) struct SessionJournalSummaryMeta {
    #[serde(default)]
    pub(super) parent_id: Option<String>,
    #[serde(default)]
    pub(super) title: Option<String>,
    #[serde(default)]
    pub(super) custom_title: Option<String>,
    pub(super) updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub(super) working_dir: Option<String>,
    #[serde(default)]
    pub(super) short_name: Option<String>,
    #[serde(default)]
    pub(super) provider_key: Option<String>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) is_canary: bool,
    #[serde(default)]
    pub(super) is_debug: bool,
    #[serde(default)]
    pub(super) saved: Option<bool>,
    #[serde(default)]
    pub(super) save_label: Option<String>,
    #[serde(default)]
    pub(super) status: SessionStatus,
    #[serde(default)]
    pub(super) last_active_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Deserialize)]
pub(super) struct SessionJournalSummaryEntry {
    pub(super) meta: SessionJournalSummaryMeta,
    #[serde(default)]
    pub(super) append_messages: SessionMessageSummaryData,
}
