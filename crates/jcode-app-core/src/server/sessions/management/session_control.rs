//! The lock-free control plane of a live session: soft-interrupt queues, the
//! stop signal and the queue registry that lets other components enqueue to a
//! session without taking its Agent lock.

use crate::provider::Provider;
use anyhow::Context;
use jcode_agent_runtime::{
    InterruptSignal, SoftInterruptMessage, SoftInterruptQueue, SoftInterruptSource,
};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex as StdMutex, Weak};

use super::state::*;

/// Process-global registry mapping session id -> background-tool signal.
///
/// The background-tool ("move tool to background", Alt+B/Ctrl+B) signal lives on
/// the `Agent`, so a `SessionControlHandle` can normally only obtain it by
/// locking the agent mutex. When a turn is busy (e.g. running `await_members`),
/// `refresh_session_control_handle` falls back to a lock-free `cancel_only`
/// handle that historically dropped the background signal entirely, which made
/// Alt+B/Ctrl+B silently no-op (`BACKGROUND_TOOL_SIGNAL_FIRE result=no_signal_handle`).
///
/// This registry is populated every time a full `SessionControlHandle` is built
/// (which always has both the session id and the correct signal), so the
/// lock-free fallback can still fire the background signal without the agent
/// lock. Entries are keyed by session id; renames/removals reuse
/// [`rename_background_tool_signal`]/[`remove_background_tool_signal`] alongside
/// the existing shutdown-signal lifecycle.
static BACKGROUND_TOOL_SIGNALS: LazyLock<StdMutex<HashMap<String, InterruptSignal>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

// The Agent keeps its provider Arc stable across model/account/route changes.
// Key by session rather than connection so a busy reattachment cannot accidentally
// configure the connection's provisional provider. Only weak references: a
// registered control must not keep an idle session alive.
pub(super) static SESSION_PROVIDERS: LazyLock<StdMutex<HashMap<String, Weak<dyn Provider>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Register (or replace) the background-tool signal for a session.
pub(super) fn register_background_tool_signal(session_id: &str, signal: InterruptSignal) {
    if let Ok(mut map) = BACKGROUND_TOOL_SIGNALS.lock() {
        map.insert(session_id.to_string(), signal);
    }
}

/// Look up the registered background-tool signal for a session, if any.
pub(super) fn background_tool_signal_for_session(session_id: &str) -> Option<InterruptSignal> {
    BACKGROUND_TOOL_SIGNALS
        .lock()
        .unwrap_or_else(|poison| {
            crate::logging::warn("Recovering poisoned background-tool signal registry");
            poison.into_inner()
        })
        .get(session_id)
        .cloned()
}

/// Move a session's out-of-band control registrations to a new session id.
pub(super) fn rename_background_tool_signal(old_session_id: &str, new_session_id: &str) {
    if old_session_id == new_session_id {
        return;
    }
    if let Ok(mut map) = SESSION_PROVIDERS.lock()
        && let Some(provider) = map.remove(old_session_id)
    {
        map.insert(new_session_id.to_string(), provider);
    }
    if let Ok(mut map) = BACKGROUND_TOOL_SIGNALS.lock()
        && let Some(signal) = map.remove(old_session_id)
    {
        map.insert(new_session_id.to_string(), signal);
    }
}

/// Drop a session's out-of-band control registrations.
pub(super) fn remove_background_tool_signal(session_id: &str) {
    if let Ok(mut map) = SESSION_PROVIDERS.lock() {
        map.remove(session_id);
    }
    if let Ok(mut map) = BACKGROUND_TOOL_SIGNALS.lock() {
        map.remove(session_id);
    }
}

fn enqueue_soft_interrupt(
    queue: &SoftInterruptQueue,
    content: String,
    images: Vec<(String, String)>,
    urgent: bool,
    source: SoftInterruptSource,
) -> bool {
    enqueue_soft_interrupt_with_id(queue, content, images, urgent, source, None)
}

fn enqueue_soft_interrupt_with_id(
    queue: &SoftInterruptQueue,
    content: String,
    images: Vec<(String, String)>,
    urgent: bool,
    source: SoftInterruptSource,
    client_id: Option<String>,
) -> bool {
    let content_bytes = content.len();
    let content_chars = content.chars().count();
    if let Ok(mut pending) = queue.lock() {
        let pending_before = pending.len();
        pending.push(SoftInterruptMessage {
            attachments: Vec::new(),
            active_skill: None,
            system_reminder: None,
            content,
            images,
            urgent,
            source,
            mode: jcode_agent_runtime::QueuedMessageMode::Steer,
            client_id,
            editing_by: None,
        });
        crate::logging::info(&format!(
            "SOFT_INTERRUPT_QUEUE_PUSH source={:?} urgent={} content_bytes={} content_chars={} pending_before={} pending_after={}",
            source,
            urgent,
            content_bytes,
            content_chars,
            pending_before,
            pending.len()
        ));
        true
    } else {
        crate::logging::warn(&format!(
            "SOFT_INTERRUPT_QUEUE_PUSH_FAILED source={:?} urgent={} content_bytes={} content_chars={} reason=queue_lock_poisoned",
            source, urgent, content_bytes, content_chars
        ));
        false
    }
}

/// Lock-free control-plane handles for a live session.
///
/// This intentionally exposes only out-of-band controls that are safe to use
/// while a turn owns the Agent mutex. Stateful operations such as history
/// mutation, model changes, or direct tool execution should continue to
/// coordinate through the Agent lock after the turn is idle/stopped.
#[derive(Clone)]
pub struct SessionControlHandle {
    pub session_id: String,
    soft_interrupt_queue: SoftInterruptQueue,
    queue_available: bool,
    background_tool_signal: Option<InterruptSignal>,
    stop_current_turn_signal: InterruptSignal,
}

impl SessionControlHandle {
    /// The provider uses interior synchronization for request configuration.
    /// Never use this to mutate conversation state or an already-sent request.
    pub fn with_provider(self, provider: Arc<dyn Provider>) -> Self {
        if let Ok(mut map) = SESSION_PROVIDERS.lock() {
            map.retain(|_, provider| provider.strong_count() > 0);
            map.insert(self.session_id.clone(), Arc::downgrade(&provider));
        }
        self
    }

    pub fn provider_handle(&self) -> Option<Arc<dyn Provider>> {
        Self::provider_for_session(&self.session_id)
    }

    pub(super) fn provider_for_session(session_id: &str) -> Option<Arc<dyn Provider>> {
        SESSION_PROVIDERS
            .lock()
            .unwrap_or_else(|poisoned| {
                crate::logging::warn("Recovering poisoned session provider registry");
                poisoned.into_inner()
            })
            .get(session_id)?
            .upgrade()
    }

    pub fn new(
        session_id: impl Into<String>,
        soft_interrupt_queue: SoftInterruptQueue,
        background_tool_signal: InterruptSignal,
        stop_current_turn_signal: InterruptSignal,
    ) -> Self {
        let session_id = session_id.into();
        // Mirror the signal into the process-global registry so the lock-free
        // `cancel_only` fallback (used while the agent mutex is busy, e.g. during
        // `await_members`) can still fire it. Without this, Alt+B/Ctrl+B silently
        // no-ops for busy turns.
        register_background_tool_signal(&session_id, background_tool_signal.clone());
        Self {
            session_id,
            soft_interrupt_queue,
            queue_available: true,
            background_tool_signal: Some(background_tool_signal),
            stop_current_turn_signal,
        }
    }

    pub fn cancel_only(
        session_id: impl Into<String>,
        soft_interrupt_queue: SoftInterruptQueue,
        stop_current_turn_signal: InterruptSignal,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            soft_interrupt_queue,
            queue_available: true,
            background_tool_signal: None,
            stop_current_turn_signal,
        }
    }

    /// Cancellation remains available while queue ownership is unresolved.
    pub(super) fn queue_unavailable(session_id: &str, stop: InterruptSignal) -> Self {
        let mut handle = Self::cancel_only(session_id, Arc::new(StdMutex::new(Vec::new())), stop);
        handle.queue_available = false;
        handle
    }

    pub(super) fn needs_queue_refresh(&self) -> bool {
        !self.queue_available
    }

    pub fn queue_soft_interrupt(
        &self,
        content: String,
        images: Vec<(String, String)>,
        urgent: bool,
        source: SoftInterruptSource,
    ) -> bool {
        self.queue_soft_interrupt_with_id(content, images, urgent, source, None)
    }

    pub fn queue_soft_interrupt_with_id(
        &self,
        content: String,
        images: Vec<(String, String)>,
        urgent: bool,
        source: SoftInterruptSource,
        client_id: Option<String>,
    ) -> bool {
        if !self.queue_available {
            return false;
        }
        let Ok(_delivery) = crate::turn_activity::global().admit_queue_delivery(&self.session_id)
        else {
            return false;
        };
        let queued = enqueue_soft_interrupt_with_id(
            &self.soft_interrupt_queue,
            content,
            images,
            urgent,
            source,
            client_id,
        );
        self.wake_blocking_waits(queued, source);
        queued
    }

    /// Accept a prompt under the same lock as steering injection. Persist
    /// before acknowledging it, and make retries with the same id idempotent.
    #[cfg(test)]
    pub(super) fn enqueue_user_prompt(
        &self,
        client_id: String,
        content: String,
        images: Vec<(String, String)>,
        mode: jcode_agent_runtime::QueuedMessageMode,
        system_reminder: Option<String>,
    ) -> anyhow::Result<bool> {
        self.enqueue_user_prompt_with_skill(
            client_id,
            content,
            images,
            mode,
            system_reminder,
            None,
            Vec::new(),
        )
    }

    /// Missing snapshots are valid for new sessions, but unreadable histories
    /// and malformed receipts cannot safely establish that a retry is new.
    fn user_prompt_was_delivered(&self, prompt_id: &str) -> anyhow::Result<bool> {
        if !crate::session::session_path(&self.session_id)?.try_exists()? {
            return Ok(false);
        }
        let session = crate::session::Session::load(&self.session_id)?;
        for message in session.iter_messages() {
            if let Some(receipt) = message.id.strip_prefix("queued:") {
                let ids: Vec<String> = serde_json::from_str(receipt)
                    .context("invalid queued prompt delivery receipt")?;
                if ids.iter().any(|id| id == prompt_id) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Keep queue insertion aligned with the request payload"
    )]
    pub(super) fn enqueue_user_prompt_with_skill(
        &self,
        client_id: String,
        content: String,
        images: Vec<(String, String)>,
        mode: jcode_agent_runtime::QueuedMessageMode,
        system_reminder: Option<String>,
        active_skill: Option<String>,
        attachments: Vec<jcode_session_types::Attachment>,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        anyhow::ensure!(!client_id.trim().is_empty(), "client_id must not be empty");
        let _delivery = crate::turn_activity::global().admit_queue_delivery(&self.session_id)?;
        if content.trim().is_empty()
            && images.is_empty()
            && attachments.is_empty()
            && let Some(reminder) = system_reminder.as_deref()
            && super::reload_recovery::is_delivered_continuation(&self.session_id, reminder)?
        {
            return Ok(true);
        }
        if !content.trim().is_empty() || !images.is_empty() || !attachments.is_empty() {
            super::reload_recovery::supersede(&self.session_id)?;
            crate::server::runtime_ssh_loops::set_user_stopped(&self.session_id, false);
        }
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        if let Some(existing) = queue
            .iter()
            .find(|message| message.client_id.as_deref() == Some(&client_id))
        {
            anyhow::ensure!(
                existing.content == content
                    && existing.images == images
                    && existing.attachments == attachments
                    && existing.active_skill == active_skill
                    && existing.system_reminder == system_reminder
                    && (existing.mode == mode
                        || existing.mode.is_in_flight()),
                "client_id already belongs to another prompt"
            );
            return Ok(false);
        }
        // The input can have been committed immediately before a lost ack.
        // Its receipt is part of the durable session history, so a retry must
        // never create a second turn even when the queue no longer contains it.
        if self.user_prompt_was_delivered(&client_id)? {
            return Ok(true);
        }
        queue.push(SoftInterruptMessage {
            attachments,
            active_skill,
            system_reminder,
            content,
            images,
            urgent: false,
            source: SoftInterruptSource::User,
            mode,
            client_id: Some(client_id),
            editing_by: None,
        });
        if let Err(error) = crate::soft_interrupt_store::overwrite(&self.session_id, &queue) {
            queue.pop();
            return Err(error);
        }
        drop(queue);
        if mode == jcode_agent_runtime::QueuedMessageMode::Steer {
            self.wake_blocking_waits(true, SoftInterruptSource::User);
        }
        Ok(false)
    }

    pub(super) fn queued_user_prompts(&self) -> anyhow::Result<Vec<crate::protocol::QueuedPrompt>> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        let queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        Ok(queue
            .iter()
            .filter_map(|message| {
                if message.source != SoftInterruptSource::User {
                    return None;
                }
                if message.mode.is_in_flight() {
                    return None;
                }
                Some(crate::protocol::QueuedPrompt {
                    client_id: message.client_id.clone()?,
                    content: message.content.clone(),
                    images: message.images.clone(),
                    attachments: message.attachments.clone(),
                    active_skill: message.active_skill.clone(),
                    editing: message.editing_by.is_some(),
                    mode: match message.mode {
                        jcode_agent_runtime::QueuedMessageMode::Steer => {
                            crate::protocol::QueuedPromptMode::Steer
                        }
                        jcode_agent_runtime::QueuedMessageMode::AfterYield => {
                            crate::protocol::QueuedPromptMode::AfterYield
                        }
                        jcode_agent_runtime::QueuedMessageMode::Queue => {
                            crate::protocol::QueuedPromptMode::Queue
                        }
                        jcode_agent_runtime::QueuedMessageMode::InFlight | jcode_agent_runtime::QueuedMessageMode::InFlightQueue => unreachable!(),
                    },
                })
            })
            .collect())
    }

    /// Hold a queued prompt open for editing, or release the hold. A held
    /// prompt keeps its position and mode but the drain never claims it, so an
    /// edit racing turn completion can never deliver both versions.
    pub(super) fn set_user_prompt_editing(
        &self,
        prompt_id: &str,
        holder: &str,
        editing: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        let item = queue
            .iter_mut()
            .find(|item| {
                item.source == SoftInterruptSource::User
                    && item.client_id.as_deref() == Some(prompt_id)
            })
            .ok_or_else(|| anyhow::anyhow!("queued prompt not found"))?;
        anyhow::ensure!(
            !item.mode.is_in_flight(),
            "queued prompt was already claimed for delivery"
        );
        if editing {
            item.editing_by = Some(holder.to_owned());
        } else if item
            .editing_by
            .as_deref()
            .is_some_and(|owner| owner == holder)
        {
            item.editing_by = None;
        }
        // Editing holds are connection-scoped, never persisted.
        drop(queue);
        Ok(())
    }

    /// Replace a still-queued prompt's content and images in place, keeping its
    /// queue position, delivery mode, and active skill. Runs under the same
    /// lock the drain uses.
    pub(super) fn edit_user_prompt(
        &self,
        prompt_id: &str,
        content: String,
        images: Vec<(String, String)>,
        attachments: Vec<jcode_session_types::Attachment>,
    ) -> anyhow::Result<crate::protocol::QueuedPromptEditOutcome> {
        use crate::protocol::QueuedPromptEditOutcome as Outcome;
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        anyhow::ensure!(
            !content.trim().is_empty() || !images.is_empty() || !attachments.is_empty(),
            "edited queued prompt must not be empty"
        );
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        let Some(index) = queue.iter().position(|item| {
            item.source == SoftInterruptSource::User && item.client_id.as_deref() == Some(prompt_id)
        }) else {
            return Ok(if self.user_prompt_was_delivered(prompt_id)? {
                Outcome::AlreadyDelivered
            } else {
                Outcome::NotFound
            });
        };
        if queue[index].mode.is_in_flight() {
            return Ok(Outcome::InFlight);
        }
        let previous_content = std::mem::replace(&mut queue[index].content, content);
        let previous_images = std::mem::replace(&mut queue[index].images, images);
        let previous_attachments = std::mem::replace(&mut queue[index].attachments, attachments);
        queue[index].editing_by = None;
        if let Err(error) = crate::soft_interrupt_store::overwrite(&self.session_id, &queue) {
            queue[index].content = previous_content;
            queue[index].images = previous_images;
            queue[index].attachments = previous_attachments;
            return Err(error);
        }
        Ok(Outcome::Edited)
    }

    /// Release every editing hold owned by a connection that went away, so a
    /// dropped client can never strand a queued prompt.
    #[cfg(test)]
    pub(crate) fn release_user_prompt_edits(&self, holder: &str) {
        let Ok(mut queue) = self.soft_interrupt_queue.lock() else {
            return;
        };
        for item in queue.iter_mut() {
            if item.editing_by.as_deref() == Some(holder) {
                item.editing_by = None;
            }
        }
    }

    pub(super) fn remove_user_prompt(
        &self,
        prompt_id: &str,
    ) -> anyhow::Result<crate::protocol::QueuedPromptRemovalOutcome> {
        use crate::protocol::QueuedPromptRemovalOutcome as Outcome;
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        if let Some(index) = queue.iter().position(|item| {
            item.source == SoftInterruptSource::User && item.client_id.as_deref() == Some(prompt_id)
        }) {
            if queue[index].mode.is_in_flight() {
                return Ok(Outcome::InFlight);
            }
            let removed = queue.remove(index);
            if let Err(error) = crate::soft_interrupt_store::overwrite(&self.session_id, &queue) {
                queue.insert(index, removed);
                return Err(error);
            }
            return Ok(Outcome::Removed);
        }
        Ok(if self.user_prompt_was_delivered(prompt_id)? {
            Outcome::AlreadyDelivered
        } else {
            Outcome::NotFound
        })
    }

    pub(super) fn reorder_user_prompts(&self, order: &[String]) -> anyhow::Result<()> {
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        let prompts: Vec<(usize, String)> = queue
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                if item.source == SoftInterruptSource::User
                    && !item.mode.is_in_flight()
                {
                    item.client_id.as_ref().map(|id| (index, id.clone()))
                } else {
                    None
                }
            })
            .collect();
        anyhow::ensure!(
            order.len() == prompts.len(),
            "order must contain every queued prompt exactly once"
        );
        let mut by_id = std::collections::HashMap::new();
        for (index, id) in &prompts {
            let item = &queue[*index];
            anyhow::ensure!(
                by_id.insert(id.clone(), item.clone()).is_none(),
                "duplicate queued prompt id"
            );
        }
        let mut reordered = Vec::with_capacity(order.len());
        for id in order {
            reordered.push(by_id.remove(id).ok_or_else(|| {
                anyhow::anyhow!("order must contain every queued prompt exactly once")
            })?);
        }
        let previous = queue.clone();
        for ((index, _), item) in prompts.into_iter().zip(reordered) {
            queue[index] = item;
        }
        if let Err(error) = crate::soft_interrupt_store::overwrite(&self.session_id, &queue) {
            *queue = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn set_user_prompt_mode(
        &self,
        client_id: &str,
        mode: jcode_agent_runtime::QueuedMessageMode,
        prioritize: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        let previous = queue.clone();
        let index = queue
            .iter()
            .position(|message| {
                message.source == SoftInterruptSource::User
                    && message.client_id.as_deref() == Some(client_id)
                    && !message.mode.is_in_flight()
            })
            .ok_or_else(|| anyhow::anyhow!("queued prompt not found"))?;
        queue[index].mode = mode;
        if prioritize {
            queue[index].urgent = true;
            let selected = queue.remove(index);
            queue.insert(0, selected);
        }
        if let Err(error) = crate::soft_interrupt_store::overwrite(&self.session_id, &queue) {
            *queue = previous;
            return Err(error);
        }
        drop(queue);
        if mode == jcode_agent_runtime::QueuedMessageMode::Steer {
            self.wake_blocking_waits(true, SoftInterruptSource::User);
        }
        Ok(())
    }

    /// Mark the requested snapshot for an interrupt without changing its order.
    pub(super) fn interrupt_user_prompts(&self, ids: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        anyhow::ensure!(!ids.is_empty(), "no queued prompts selected");
        let requested: std::collections::HashSet<_> = ids.iter().collect();
        anyhow::ensure!(requested.len() == ids.len(), "duplicate queued prompt id");
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        let found = queue
            .iter()
            .filter(|item| {
                item.source == SoftInterruptSource::User
                    && !item.mode.is_in_flight()
                    && item
                        .client_id
                        .as_ref()
                        .is_some_and(|id| requested.contains(id))
            })
            .count();
        anyhow::ensure!(
            found == ids.len(),
            "one or more queued prompts are no longer available"
        );
        let previous = queue.clone();
        for item in queue.iter_mut().filter(|item| {
            item.client_id
                .as_ref()
                .is_some_and(|id| requested.contains(id))
                && item.source == SoftInterruptSource::User
        }) {
            item.mode = jcode_agent_runtime::QueuedMessageMode::AfterYield;
            item.urgent = true;
        }
        if let Err(error) = crate::soft_interrupt_store::overwrite(&self.session_id, &queue) {
            *queue = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn take_user_prompt_batch(
        &self,
        interrupted_only: bool,
    ) -> anyhow::Result<Vec<SoftInterruptMessage>> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        let mut batch = Vec::new();
        for item in queue.iter_mut() {
            if !interrupted_only
                && item.source == SoftInterruptSource::User
                && item.mode == jcode_agent_runtime::QueuedMessageMode::Queue
                && item.editing_by.is_some()
            {
                break;
            }
            if item.source == SoftInterruptSource::User
                && item.client_id.is_some()
                && item.editing_by.is_none()
                && !item.mode.is_in_flight()
                && (!interrupted_only
                    || (item.mode == jcode_agent_runtime::QueuedMessageMode::AfterYield
                        && item.urgent))
            {
                if item.mode == jcode_agent_runtime::QueuedMessageMode::Queue {
                    if !batch.is_empty() {
                        break;
                    }
                    batch.push(item.clone());
                    item.mode = jcode_agent_runtime::QueuedMessageMode::InFlightQueue;
                    break;
                }
                batch.push(item.clone());
                item.mode = jcode_agent_runtime::QueuedMessageMode::InFlight;
            }
        }
        Ok(batch)
    }

    #[cfg(test)]
    pub(super) fn take_next_user_prompt(
        &self,
        interrupted_only: bool,
    ) -> anyhow::Result<Option<SoftInterruptMessage>> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        let Some(index) = queue.iter().position(|message| {
            message.source == SoftInterruptSource::User
                && message.client_id.is_some()
                && message.editing_by.is_none()
                && !message.mode.is_in_flight()
                && (!interrupted_only
                    || (message.mode == jcode_agent_runtime::QueuedMessageMode::AfterYield
                        && message.urgent))
        }) else {
            return Ok(None);
        };
        let prompt = queue[index].clone();
        queue[index].mode = jcode_agent_runtime::QueuedMessageMode::InFlight;
        Ok(Some(prompt))
    }

    pub(super) fn restore_user_prompt(&self, prompt: SoftInterruptMessage) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.queue_available,
            "session queue unavailable while reconnecting"
        );
        let _delivery = crate::turn_activity::global().admit_queue_delivery(&self.session_id)?;
        let mut queue = self
            .soft_interrupt_queue
            .lock()
            .map_err(|_| anyhow::anyhow!("queue lock poisoned"))?;
        if let Some(existing) = queue
            .iter_mut()
            .find(|item| item.client_id == prompt.client_id)
        {
            *existing = prompt;
        } else {
            queue.insert(0, prompt);
        }
        crate::soft_interrupt_store::overwrite(&self.session_id, &queue)
    }

    /// A queued user prompt must not sit behind a long blocking tool wait.
    fn wake_blocking_waits(&self, queued: bool, source: SoftInterruptSource) {
        if queued {
            crate::incoming_message::bind(&self.session_id, &self.soft_interrupt_queue);
            crate::incoming_message::notify_if_user(&self.session_id, source);
        }
    }

    /// Soft interrupts queued and not yet injected.
    pub fn pending_soft_interrupt_count(&self) -> usize {
        self.soft_interrupt_queue
            .lock()
            .map(|queue| queue.len())
            .unwrap_or(0)
    }

    /// Remove the queued interrupts whose caller id is in `client_ids`.
    ///
    /// Injection drains the queue under the same lock, so each requested id is
    /// reported exactly once: in `cancelled` if it was still queued, otherwise
    /// in `not_queued` (already injected, or never queued here). Order follows
    /// the request, and duplicate requested ids are reported once.
    pub fn cancel_soft_interrupts_by_id(
        &self,
        client_ids: &[String],
    ) -> (Vec<String>, Vec<String>) {
        if !self.queue_available {
            return (Vec::new(), client_ids.to_vec());
        }
        let mut requested: Vec<String> = Vec::new();
        for id in client_ids {
            if !requested.contains(id) {
                requested.push(id.clone());
            }
        }
        let removed: Vec<String> = match self.soft_interrupt_queue.lock() {
            Ok(mut queue) => {
                let cancellable: Vec<String> = requested
                    .iter()
                    .filter(|id| {
                        !queue.iter().any(|item| {
                            item.client_id.as_ref() == Some(*id)
                                && item.mode.is_in_flight()
                        })
                    })
                    .cloned()
                    .collect();
                // Queue mutation and its durable counterpart share this lock.
                // Otherwise a cancelled daemon-owned prompt reappears on reload.
                let mut removed = match crate::soft_interrupt_store::remove_by_client_ids(
                    &self.session_id,
                    &cancellable,
                ) {
                    Ok(removed) => removed,
                    Err(error) => {
                        crate::logging::warn(&format!(
                            "SOFT_INTERRUPT_QUEUE_CANCEL_PERSIST_FAILED session={} error={error}",
                            self.session_id
                        ));
                        return (Vec::new(), requested);
                    }
                };
                queue.retain(|message| match &message.client_id {
                    Some(id) if cancellable.contains(id) => {
                        if !removed.contains(id) {
                            removed.push(id.clone());
                        }
                        false
                    }
                    _ => true,
                });
                removed
            }
            Err(_) => {
                crate::logging::warn(&format!(
                    "SOFT_INTERRUPT_QUEUE_CANCEL_BY_ID_FAILED session={} reason=queue_lock_poisoned",
                    self.session_id
                ));
                Vec::new()
            }
        };
        let (cancelled, not_queued): (Vec<String>, Vec<String>) =
            requested.into_iter().partition(|id| removed.contains(id));
        crate::logging::info(&format!(
            "SOFT_INTERRUPT_QUEUE_CANCEL_BY_ID session={} cancelled={} not_queued={}",
            self.session_id,
            cancelled.len(),
            not_queued.len()
        ));
        (cancelled, not_queued)
    }

    pub fn clear_soft_interrupts(&self) {
        if !self.queue_available {
            return;
        }
        if let Ok(mut queue) = self.soft_interrupt_queue.lock() {
            let cleared = queue.len();
            queue.clear();
            crate::logging::info(&format!(
                "SOFT_INTERRUPT_QUEUE_CLEAR session={} cleared={}",
                self.session_id, cleared
            ));
        } else {
            crate::logging::warn(&format!(
                "SOFT_INTERRUPT_QUEUE_CLEAR_FAILED session={} reason=queue_lock_poisoned",
                self.session_id
            ));
        }
    }

    /// Fire the stop-current-turn signal. Returns the signal's fire epoch so
    /// callers that schedule a deferred [`reset_cancel_if_epoch`](Self::reset_cancel_if_epoch)
    /// can avoid erasing a newer cancel that fired in the meantime (issue #428).
    ///
    /// Also fires every cancel signal registered for currently running turns
    /// of this session. The handle's own signal can be a stale instance that
    /// the streaming turn never observes (reattach after reload/disconnect,
    /// server-initiated turns, headless recovery), which used to make Esc show
    /// "Interrupting..." while the model kept generating for minutes
    /// (issue #428).
    pub fn request_cancel(&self) -> u64 {
        if let Err(error) = super::reload_recovery::cancel(&self.session_id) {
            crate::logging::warn(&format!(
                "Cannot persist recovery cancellation for {}: {error}",
                self.session_id
            ));
        }
        crate::logging::info(&format!(
            "SESSION_CANCEL_SIGNAL_FIRE session={}",
            self.session_id
        ));
        crate::server::runtime_ssh_loops::set_user_stopped(&self.session_id, true);
        crate::mobile_push::mark_cancelled(&self.session_id);
        crate::turn_cancel_registry::mark_interruption(
            &self.session_id,
            crate::turn_cancel_registry::TurnInterruptionReason::Cancel,
        );
        self.stop_current_turn_signal.fire();
        let active_turn_signals =
            crate::turn_cancel_registry::active_turn_signals(&self.session_id);
        let mut fired_active = 0usize;
        for signal in &active_turn_signals {
            if signal.same_instance(&self.stop_current_turn_signal) {
                continue;
            }
            signal.fire();
            fired_active += 1;
        }
        if fired_active > 0 {
            crate::logging::info(&format!(
                "SESSION_CANCEL_ACTIVE_TURN_SIGNALS_FIRED session={} fired={} registered={}",
                self.session_id,
                fired_active,
                active_turn_signals.len()
            ));
        }
        self.stop_current_turn_signal.epoch()
    }

    pub fn reset_cancel(&self) {
        crate::logging::info(&format!(
            "SESSION_CANCEL_SIGNAL_RESET session={}",
            self.session_id
        ));
        self.stop_current_turn_signal.reset();
    }

    /// Reset the cancel signal only if no newer cancel fired since `epoch`
    /// was captured from [`request_cancel`](Self::request_cancel). Timed
    /// resets (used when the running turn is not owned by this connection)
    /// must use this instead of [`reset_cancel`](Self::reset_cancel):
    /// an unconditional deferred reset can erase a newer, not-yet-observed
    /// cancel, making repeated Esc presses appear to be ignored (issue #428).
    pub fn reset_cancel_if_epoch(&self, epoch: u64) -> bool {
        let reset = self.stop_current_turn_signal.reset_if_epoch(epoch);
        crate::logging::info(&format!(
            "SESSION_CANCEL_SIGNAL_RESET session={} epoch={} applied={}",
            self.session_id, epoch, reset
        ));
        reset
    }

    pub fn request_background_current_tool(&self) -> bool {
        // A call stopped on a human answer (`ask`, an MCP approval) stays in the
        // foreground. Moving it ends the turn while its prompt is still open,
        // and a queued message would then answer the prompt as free text.
        if crate::tool::interactive::has_pending(&self.session_id) {
            crate::logging::info(&format!(
                "BACKGROUND_TOOL_SIGNAL_FIRE session={} result=refused_pending_prompt",
                self.session_id
            ));
            return false;
        }
        // Prefer the directly-held signal; fall back to the process-global
        // registry for lock-free (`cancel_only`) handles built while the agent
        // mutex was busy. This is what makes Alt+B/Ctrl+B work during a busy
        // turn such as `await_members`.
        let signal = self
            .background_tool_signal
            .clone()
            .or_else(|| background_tool_signal_for_session(&self.session_id));
        if let Some(signal) = signal {
            signal.fire();
            crate::logging::info(&format!(
                "BACKGROUND_TOOL_SIGNAL_FIRE session={} result=sent",
                self.session_id
            ));
            true
        } else {
            crate::logging::warn(&format!(
                "BACKGROUND_TOOL_SIGNAL_FIRE session={} result=no_signal_handle",
                self.session_id
            ));
            false
        }
    }

    pub fn stop_current_turn_signal(&self) -> InterruptSignal {
        self.stop_current_turn_signal.clone()
    }
}

pub(super) async fn register_session_interrupt_queue(
    queues: &SessionInterruptQueues,
    session_id: &str,
    queue: SoftInterruptQueue,
) {
    crate::incoming_message::bind(session_id, &queue);
    let mut guard = queues.write().await;
    guard.insert(session_id.to_string(), queue);
}

pub(super) async fn rename_session_interrupt_queue(
    queues: &SessionInterruptQueues,
    old_session_id: &str,
    new_session_id: &str,
) {
    let mut guard = queues.write().await;
    if let Some(queue) = guard.remove(old_session_id) {
        crate::incoming_message::bind(new_session_id, &queue);
        guard.insert(new_session_id.to_string(), queue);
    }
}

pub(super) async fn remove_session_interrupt_queue(
    queues: &SessionInterruptQueues,
    session_id: &str,
) {
    let mut guard = queues.write().await;
    guard.remove(session_id);
    crate::incoming_message::forget(session_id);
}

pub(super) async fn queue_soft_interrupt_for_session(
    session_id: &str,
    content: String,
    urgent: bool,
    source: SoftInterruptSource,
    queues: &SessionInterruptQueues,
    sessions: &super::SessionAgents,
) -> bool {
    queue_soft_interrupt_with_images_for_session(
        session_id,
        content,
        Vec::new(),
        urgent,
        source,
        queues,
        sessions,
    )
    .await
}

/// Rewrite a still-queued file-activity notice in place. Returns `false` once the
/// notice has been delivered (or was never queued live), so the caller queues
/// a fresh one instead.
pub(super) async fn replace_pending_soft_interrupt(
    session_id: &str,
    previous: &str,
    content: String,
    queues: &SessionInterruptQueues,
) -> bool {
    let Some(queue) = queues.read().await.get(session_id).cloned() else {
        return false;
    };
    let Ok(mut pending) = queue.lock() else {
        return false;
    };
    match pending.iter_mut().find(|message| {
        message.source == SoftInterruptSource::FileActivity && message.content == previous
    }) {
        Some(message) => {
            message.content = content;
            true
        }
        None => false,
    }
}

pub(super) async fn queue_soft_interrupt_with_images_for_session(
    session_id: &str,
    content: String,
    images: Vec<(String, String)>,
    urgent: bool,
    source: SoftInterruptSource,
    queues: &SessionInterruptQueues,
    sessions: &super::SessionAgents,
) -> bool {
    let Ok(_delivery) = crate::turn_activity::global().admit_queue_delivery(session_id) else {
        return false;
    };
    if let Some(queue) = queues.read().await.get(session_id).cloned() {
        let queued = enqueue_soft_interrupt(&queue, content, images, urgent, source);
        if queued {
            crate::incoming_message::notify_if_user(session_id, source);
        }
        return queued;
    }

    let queue = {
        let guard = sessions.snapshot().await;
        guard.get(session_id).and_then(|agent| {
            agent
                .try_lock()
                // A busy Agent is normal; persist below and restore on its next turn.
                .ok()
                .map(|agent_guard| agent_guard.soft_interrupt_queue())
        })
    };

    if let Some(queue) = queue {
        register_session_interrupt_queue(queues, session_id, queue.clone()).await;
        let queued = enqueue_soft_interrupt(&queue, content, images, urgent, source);
        if queued {
            crate::incoming_message::notify_if_user(session_id, source);
        }
        queued
    } else {
        let session_exists = {
            let guard = sessions.snapshot().await;
            guard.contains_key(session_id)
        } || crate::session::session_exists(session_id);

        if !session_exists {
            return false;
        }

        crate::soft_interrupt_store::append(
            session_id,
            SoftInterruptMessage {
                attachments: Vec::new(),
                active_skill: None,
                system_reminder: None,
                content,
                images,
                urgent,
                source,
                mode: jcode_agent_runtime::QueuedMessageMode::Steer,
                client_id: None,
                editing_by: None,
            },
        )
        .map(|_| true)
        .unwrap_or_else(|err| {
            crate::logging::warn(&format!(
                "Failed to persist deferred soft interrupt for session {}: {}",
                session_id, err
            ));
            false
        })
    }
}
