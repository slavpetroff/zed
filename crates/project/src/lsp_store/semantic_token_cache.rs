use lsp::LanguageServerId;

/// A logic to apply when querying for new semantic tokens and deciding what to do with cached data.
#[derive(Debug, Clone, Copy)]
pub enum InvalidationStrategy {
    /// Language servers reset tokens via <a href="https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#workspace_semanticTokens_refresh">request</a>.
    /// Demands to re-query all semantic tokens needed and invalidate all cached entries.
    RefreshRequested(LanguageServerId),
    /// Buffer was edited. Try to use delta requests if supported by the server.
    BufferEdited,
    /// A new file got opened/new excerpt was added to a multibuffer/a buffer was scrolled to a new position.
    /// No invalidation should be done, query only for the new visible ranges.
    None,
}

impl InvalidationStrategy {
    pub fn should_invalidate(&self) -> bool {
        matches!(
            self,
            InvalidationStrategy::RefreshRequested(_) | InvalidationStrategy::BufferEdited
        )
    }
}
