//! Semantic highlighting management for the editor.
//!
//! This module handles the fetching, caching, and refresh logic for LSP semantic tokens.
//! Unlike inlay hints which work with chunks, semantic tokens operate on the full visible viewport.
//!
//! ## Architecture
//!
//! - `SemanticTokenRefreshReason`: Enum describing why a refresh is needed
//! - `SemanticHighlightingState`: Tracks pending requests, failures, and tasks per buffer
//! - Editor extension methods: High-level refresh logic that coordinates with LSP

use std::collections::HashMap;
use std::time::Duration;

use gpui::{Context, Task};
use lsp::LanguageServerId;
use project::lsp_store::semantic_token_cache::InvalidationStrategy as SemanticTokensInvalidationStrategy;
use text::BufferId;

use crate::Editor;

/// Reason for requesting a semantic token refresh.
///
/// This enum consolidates the various conditions that trigger semantic token updates,
/// making the refresh API cleaner and more explicit about intent.
#[derive(Debug, Clone)]
pub enum SemanticTokenRefreshReason {
    /// New lines became visible through scrolling (non-invalidating).
    NewLinesShown,
    /// A specific buffer was edited, requiring its tokens to be refreshed.
    BufferEdited(BufferId),
    /// A language server explicitly requested a refresh.
    RefreshRequested(LanguageServerId),
    /// Editor settings changed, requiring a full refresh.
    SettingsChanged,
}

/// Tracks the state of semantic highlighting for buffers in the editor.
///
/// This structure maintains per-buffer state for pending requests, failure counts,
/// and active refresh tasks, similar to how inlay hints track their state.
#[derive(Debug)]
pub struct SemanticHighlightingState {
    /// Failure count per buffer (used for exponential backoff).
    pub failure_counts: HashMap<BufferId, u32>,

    /// Active refresh tasks per buffer.
    /// Tasks are automatically cancelled when dropped (replaced or removed).
    pub refresh_tasks: HashMap<BufferId, Task<()>>,

    /// Debounce for invalidating edits (ms).
    pub invalidate_debounce: Option<Duration>,

    /// Debounce for non-invalidating scrolls (ms).
    pub append_debounce: Option<Duration>,
}

impl SemanticHighlightingState {
    pub fn new() -> Self {
        Self {
            failure_counts: HashMap::default(),
            refresh_tasks: HashMap::default(),
            invalidate_debounce: Some(Duration::from_millis(50)),
            append_debounce: Some(Duration::from_millis(100)),
        }
    }

    /// Record a failure for a buffer.
    pub fn record_failure(&mut self, buffer_id: BufferId) -> u32 {
        let count = self.failure_counts.entry(buffer_id).or_insert(0);
        *count += 1;
        *count
    }

    /// Clear the failure count on success.
    pub fn clear_failure(&mut self, buffer_id: BufferId) {
        self.failure_counts.remove(&buffer_id);
    }

    /// Get the failure count for a buffer.
    pub fn failure_count(&self, buffer_id: BufferId) -> u32 {
        self.failure_counts.get(&buffer_id).copied().unwrap_or(0)
    }

    /// Check if a buffer should be skipped due to too many failures.
    pub fn should_skip_buffer(&self, buffer_id: BufferId) -> bool {
        self.failure_count(buffer_id) >= 3
    }
}

impl Editor {
    /// Determine the invalidation strategy based on the refresh reason.
    ///
    /// This is the equivalent of `refresh_editor_data` in inlay hints - it translates
    /// the high-level reason into the specific invalidation strategy needed by LSP.
    fn semantic_highlighting_invalidation_strategy(
        &self,
        reason: &SemanticTokenRefreshReason,
        _cx: &Context<Self>,
    ) -> Option<SemanticTokensInvalidationStrategy> {
        // Early return if no project
        self.project.as_ref()?;

        let strategy = match reason {
            SemanticTokenRefreshReason::RefreshRequested(server_id) => {
                SemanticTokensInvalidationStrategy::RefreshRequested(*server_id)
            }
            SemanticTokenRefreshReason::BufferEdited(_)
            | SemanticTokenRefreshReason::SettingsChanged => {
                SemanticTokensInvalidationStrategy::BufferEdited
            }
            SemanticTokenRefreshReason::NewLinesShown => SemanticTokensInvalidationStrategy::None,
        };

        Some(strategy)
    }

    /// Main entry point for refreshing semantic tokens.
    ///
    /// This method coordinates the entire refresh process:
    /// 1. Determines invalidation strategy via helper
    /// 2. Collects visible buffers that need tokens
    /// 3. Spawns LSP fetch tasks with proper error handling
    ///
    /// The logic mirrors `refresh_inlay_hints` but is adapted for full-viewport
    /// semantic tokens rather than chunked inlay hints.
    pub(crate) fn refresh_semantic_tokens(
        &mut self,
        reason: SemanticTokenRefreshReason,
        cx: &mut Context<Self>,
    ) {
        let Some(invalidation_strategy) =
            self.semantic_highlighting_invalidation_strategy(&reason, cx)
        else {
            return;
        };

        // Calculate debounce duration based on reason (mirrors inlay hints pattern)
        let debounce = match &reason {
            // Settings changes and explicit refresh requests have no debounce
            SemanticTokenRefreshReason::SettingsChanged
            | SemanticTokenRefreshReason::RefreshRequested(_) => None,
            // Buffer edits and scrolls use debouncing
            _ => {
                if invalidation_strategy.should_invalidate() {
                    self.semantic_highlighting_state.invalidate_debounce
                } else {
                    self.semantic_highlighting_state.append_debounce
                }
            }
        };

        // Determine fetch strategy based on reason (mirrors inlay hints pattern)
        let ignore_previous_fetches = match reason {
            SemanticTokenRefreshReason::SettingsChanged
            | SemanticTokenRefreshReason::RefreshRequested(_)
            | SemanticTokenRefreshReason::NewLinesShown => true, // Always replace on scroll/new content
            SemanticTokenRefreshReason::BufferEdited(_) => false,
        };

        // IMPORTANT: No early exit! We MUST call visible_excerpts() to see new buffers.
        // Inlay hints doesn't have an early exit here for this reason.
        let mut visible_excerpts = self.visible_excerpts(cx);

        log::debug!(
            "[SEMANTIC TOKENS] refresh called with reason: {:?}, visible_excerpts() returned {} excerpts",
            reason,
            visible_excerpts.len()
        );

        // Filter visible excerpts for BufferEdited - match inlay hints pattern
        // We filter by language, not buffer_id, so editing one Rust file refreshes all visible Rust buffers
        if let SemanticTokenRefreshReason::BufferEdited(buffer_id) = reason {
            let Some(affected_language) = self
                .buffer()
                .read(cx)
                .buffer(buffer_id)
                .and_then(|buffer| buffer.read(cx).language().cloned())
            else {
                return;
            };

            log::debug!(
                "[SEMANTIC TOKENS] BufferEdited for buffer {:?} with language {:?}, filtering visible excerpts by language",
                buffer_id,
                affected_language.name()
            );

            visible_excerpts.retain(|_, (visible_buffer, _, _)| {
                visible_buffer.read(cx).language() == Some(&affected_language)
            });
        }

        let mut buffers_to_fetch: HashMap<BufferId, gpui::Entity<language::Buffer>> =
            HashMap::default();

        // Collect unique visible buffers that need semantic tokens
        let mut skipped_unregistered = 0;
        let mut skipped_failed = 0;
        let mut all_visible_buffer_ids: Vec<BufferId> = Vec::new();

        for (_, (buffer, _, _)) in visible_excerpts {
            let buffer_id = buffer.read(cx).remote_id();
            all_visible_buffer_ids.push(buffer_id);

            // Auto-register visible buffers that aren't registered yet
            // This ensures all visible buffers can get semantic tokens, not just the first one
            if !self.registered_buffers.contains_key(&buffer_id) {
                log::debug!(
                    "[SEMANTIC TOKENS] Auto-registering unregistered visible buffer {:?}",
                    buffer_id
                );
                self.register_buffer(buffer_id, cx);
            }

            // Check registration again after auto-register attempt
            if !self.registered_buffers.contains_key(&buffer_id) {
                skipped_unregistered += 1;
                continue;
            }

            // Skip buffers that have failed too many times
            if self
                .semantic_highlighting_state
                .should_skip_buffer(buffer_id)
            {
                skipped_failed += 1;
                continue;
            }

            buffers_to_fetch
                .entry(buffer_id)
                .or_insert_with(|| buffer.clone());
        }

        log::debug!(
            "[SEMANTIC TOKENS] ALL visible buffer IDs from excerpts: {:?}",
            all_visible_buffer_ids
        );
        log::debug!(
            "[SEMANTIC TOKENS] Collected {} unique buffers to fetch (skipped {} unregistered, {} failed): {:?}",
            buffers_to_fetch.len(),
            skipped_unregistered,
            skipped_failed,
            buffers_to_fetch.keys().collect::<Vec<_>>()
        );

        // Spawn fetch tasks for collected buffers
        let project = self.project.clone().unwrap();
        for (buffer_id, buffer) in buffers_to_fetch {
            // Check if we should replace an existing task
            // If a task exists and we're not invalidating/forcing, skip spawning a new one
            if let Some(_existing_task) = self
                .semantic_highlighting_state
                .refresh_tasks
                .get(&buffer_id)
            {
                if !invalidation_strategy.should_invalidate() && !ignore_previous_fetches {
                    log::debug!(
                        "[SEMANTIC TOKENS] Skipping buffer {:?} - task already exists",
                        buffer_id
                    );
                    continue;
                }
                // Task will be replaced below (dropped automatically)
                log::debug!(
                    "[SEMANTIC TOKENS] Replacing existing task for buffer {:?}",
                    buffer_id
                );
            }

            let project = project.clone();

            let task = cx.spawn(async move |editor, cx| {
                // Debounce if needed (mirrors inlay hints pattern)
                if let Some(debounce) = debounce {
                    cx.background_executor().timer(debounce).await;
                }

                let lsp_task = project.update(cx, |project, cx| {
                    project.lsp_store().update(cx, |store, cx| {
                        store.semantic_tokens(buffer, invalidation_strategy, cx)
                    })
                });

                let failed = match lsp_task {
                    Ok(task) => {
                        if let Err(e) = task.await {
                            log::warn!(
                                "Failed to fetch semantic tokens for buffer {buffer_id}: {e:#}"
                            );
                            true
                        } else {
                            log::debug!(
                                "[SEMANTIC TOKENS] Successfully fetched tokens for buffer {buffer_id}"
                            );
                            false
                        }
                    }
                    Err(e) => {
                        log::warn!(
                            "Failed to start semantic tokens request for buffer {buffer_id}: {e:#}"
                        );
                        true
                    }
                };

                editor
                    .update(cx, |editor, _| {
                        editor.semantic_highlighting_state.refresh_tasks.remove(&buffer_id);
                        if failed {
                            let count = editor
                                .semantic_highlighting_state
                                .record_failure(buffer_id);
                            if count >= 3 {
                                log::warn!(
                                    "Buffer {buffer_id} has failed semantic tokens {count} times, stopping automatic retries"
                                );
                            }
                        } else {
                            editor.semantic_highlighting_state.clear_failure(buffer_id);
                        }
                    })
                    .ok();
            });

            // Store task - replaces any existing task (which gets dropped/cancelled automatically)
            self.semantic_highlighting_state
                .refresh_tasks
                .insert(buffer_id, task);
        }
    }
}
