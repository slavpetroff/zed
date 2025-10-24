use std::{ops::Range, sync::Arc};

use collections::HashMap;
use futures::future::Shared;
use gpui::{App, Entity, Task};
use language::{Buffer, BufferRow, BufferSnapshot};
use lsp::LanguageServerId;
use text::OffsetRangeExt;

use crate::lsp_store::semantic_tokens::SemanticTokens;

pub type CacheSemanticTokens = HashMap<LanguageServerId, Arc<SemanticTokens>>;
pub type CacheSemanticTokensTask = Shared<Task<Result<CacheSemanticTokens, Arc<anyhow::Error>>>>;

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

pub struct BufferSemanticTokens {
    snapshot: BufferSnapshot,
    buffer_chunks: Vec<BufferChunk>,
    tokens_by_chunk: Vec<Option<CacheSemanticTokens>>,
    fetches_by_chunk: Vec<Option<CacheSemanticTokensTask>>,
    result_ids: HashMap<LanguageServerId, String>,
}

/// A range of rows representing a chunk of the buffer for semantic token queries.
/// Each chunk is queried independently and cached separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferChunk {
    pub id: usize,
    pub start: BufferRow,
    pub end: BufferRow,
}

impl std::fmt::Debug for BufferSemanticTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferSemanticTokens")
            .field("buffer_chunks", &self.buffer_chunks)
            .field("tokens_by_chunk", &self.tokens_by_chunk)
            .field("fetches_by_chunk", &self.fetches_by_chunk)
            .field("result_ids", &self.result_ids)
            .finish_non_exhaustive()
    }
}

const MAX_ROWS_IN_A_CHUNK: u32 = 50;

impl BufferSemanticTokens {
    pub fn new(buffer: &Entity<Buffer>, cx: &mut App) -> Self {
        let buffer = buffer.read(cx);
        let snapshot = buffer.snapshot();
        let buffer_point_range = (0..buffer.len()).to_point(&snapshot);
        let last_row = buffer_point_range.end.row;
        let buffer_chunks = (buffer_point_range.start.row..=last_row)
            .step_by(MAX_ROWS_IN_A_CHUNK as usize)
            .enumerate()
            .map(|(id, chunk_start)| BufferChunk {
                id,
                start: chunk_start,
                end: (chunk_start + MAX_ROWS_IN_A_CHUNK).min(last_row),
            })
            .collect::<Vec<_>>();

        Self {
            tokens_by_chunk: vec![None; buffer_chunks.len()],
            fetches_by_chunk: vec![None; buffer_chunks.len()],
            result_ids: HashMap::default(),
            snapshot,
            buffer_chunks,
        }
    }

    pub fn applicable_chunks(
        &self,
        ranges: &[Range<text::Anchor>],
    ) -> impl Iterator<Item = BufferChunk> {
        let row_ranges = ranges
            .iter()
            .map(|range| range.to_point(&self.snapshot))
            .map(|point_range| point_range.start.row..=point_range.end.row)
            .collect::<Vec<_>>();
        self.buffer_chunks
            .iter()
            .filter(move |chunk| -> bool {
                let chunk_range = chunk.start..=chunk.end;
                row_ranges.iter().any(|row_range| {
                    chunk_range.contains(&row_range.start())
                        || chunk_range.contains(&row_range.end())
                })
            })
            .copied()
    }

    pub fn cached_tokens(&self, chunk: &BufferChunk) -> Option<&CacheSemanticTokens> {
        self.tokens_by_chunk[chunk.id].as_ref()
    }

    pub fn fetched_tokens(&mut self, chunk: &BufferChunk) -> &mut Option<CacheSemanticTokensTask> {
        &mut self.fetches_by_chunk[chunk.id]
    }

    pub fn insert_new_tokens(
        &mut self,
        chunk: BufferChunk,
        server_id: LanguageServerId,
        tokens: Arc<SemanticTokens>,
        result_id: Option<String>,
    ) {
        let tokens_for_chunk = self.tokens_by_chunk[chunk.id].get_or_insert_with(HashMap::default);
        tokens_for_chunk.insert(server_id, tokens);
        
        if let Some(result_id) = result_id {
            self.result_ids.insert(server_id, result_id);
        }
    }

    pub fn result_id(&self, server_id: LanguageServerId) -> Option<&String> {
        self.result_ids.get(&server_id)
    }

    pub fn remove_server_data(&mut self, for_server: LanguageServerId) {
        for tokens in self.tokens_by_chunk.iter_mut() {
            if let Some(tokens) = tokens {
                tokens.remove(&for_server);
            }
        }
        self.result_ids.remove(&for_server);
    }

    pub fn clear(&mut self) {
        self.tokens_by_chunk = vec![None; self.buffer_chunks.len()];
        self.fetches_by_chunk = vec![None; self.buffer_chunks.len()];
        self.result_ids.clear();
    }
}
