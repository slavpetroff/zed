use std::slice::ChunksExact;

use crate::lsp_command::SemanticTokensEdit;

/// All the semantic tokens for a buffer, in the LSP format.
#[derive(Default, Debug, Clone)]
pub struct SemanticTokens {
    /// Each value is:
    /// data[5*i] - deltaLine: token line number, relative to the start of the previous token
    /// data[5*i+1] - deltaStart: token start character, relative to the start of the previous token (relative to 0 or the previous token’s start if they are on the same line)
    /// data[5*i+2] - length: the length of the token.
    /// data[5*i+3] - tokenType: will be looked up in SemanticTokensLegend.tokenTypes. We currently ask that tokenType < 65536.
    /// data[5*i+4] - tokenModifiers: each set bit will be looked up in SemanticTokensLegend.tokenModifiers
    ///
    /// See https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/ for more.
    data: Vec<u32>,

    pub server_id: Option<lsp::LanguageServerId>,
}

pub struct SemanticTokensIter<'a> {
    prev: Option<(u32, u32)>,
    data: ChunksExact<'a, u32>,
}

// A single item from `data`.
struct SemanticTokenValue {
    delta_line: u32,
    delta_start: u32,
    length: u32,
    token_type: u32,
    token_modifiers: u32,
}

/// A semantic token, independent of its position.
#[derive(Debug)]
pub struct SemanticToken {
    pub line: u32,
    pub start: u32,
    pub length: u32,
    pub token_type: u32,
    pub token_modifiers: u32,
}

impl SemanticTokens {
    pub fn from_full(data: Vec<u32>) -> Self {
        SemanticTokens {
            data,
            server_id: None,
        }
    }

    pub(crate) fn apply(&mut self, edits: &[SemanticTokensEdit]) {
        for edit in edits {
            let start = edit.start as usize;
            let end = start + edit.delete_count as usize;
            self.data.splice(start..end, edit.data.iter().copied());
        }
    }

    pub fn tokens(&self) -> SemanticTokensIter<'_> {
        SemanticTokensIter {
            prev: None,
            data: self.data.chunks_exact(5),
        }
    }

    pub fn data(&self) -> &[u32] {
        &self.data
    }
}

impl Iterator for SemanticTokensIter<'_> {
    type Item = SemanticToken;

    fn next(&mut self) -> Option<Self::Item> {
        let chunk = self.data.next()?;
        let token = SemanticTokenValue {
            delta_line: chunk[0],
            delta_start: chunk[1],
            length: chunk[2],
            token_type: chunk[3],
            token_modifiers: chunk[4],
        };

        let (line, start) = if let Some((last_line, last_start)) = self.prev {
            let line = last_line + token.delta_line;
            let start = if token.delta_line == 0 {
                last_start + token.delta_start
            } else {
                token.delta_start
            };
            (line, start)
        } else {
            (token.delta_line, token.delta_start)
        };

        self.prev = Some((line, start));

        Some(SemanticToken {
            line,
            start,
            length: token.length,
            token_type: token.token_type,
            token_modifiers: token.token_modifiers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create delta-encoded tokens
    fn encode_tokens(tokens: &[(u32, u32, u32, u32, u32)]) -> Vec<u32> {
        let mut data = Vec::new();
        let mut prev_line = 0;
        let mut prev_start = 0;

        for &(line, start, length, token_type, token_modifiers) in tokens {
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                start - prev_start
            } else {
                start
            };

            data.push(delta_line);
            data.push(delta_start);
            data.push(length);
            data.push(token_type);
            data.push(token_modifiers);

            prev_line = line;
            prev_start = start;
        }

        data
    }

    /// Helper to decode tokens to absolute positions
    fn decode_tokens(semantic_tokens: &SemanticTokens) -> Vec<(u32, u32, u32, u32, u32)> {
        semantic_tokens
            .tokens()
            .map(|token| {
                (
                    token.line,
                    token.start,
                    token.length,
                    token.token_type,
                    token.token_modifiers,
                )
            })
            .collect()
    }

    #[test]
    fn test_delta_encoding_decoding_roundtrip() {
        // Create tokens at various positions
        let tokens = vec![
            (0, 5, 3, 1, 0),   // Line 0, col 5
            (0, 10, 4, 2, 1),  // Line 0, col 10 (same line)
            (2, 3, 5, 1, 0),   // Line 2, col 3 (new line)
            (2, 15, 3, 3, 2),  // Line 2, col 15 (same line)
            (5, 0, 2, 1, 0),   // Line 5, col 0 (new line)
        ];

        let encoded = encode_tokens(&tokens);
        let semantic_tokens = SemanticTokens::from_full(encoded);
        let decoded = decode_tokens(&semantic_tokens);

        assert_eq!(tokens, decoded);
    }

    #[test]
    fn test_merge_non_overlapping_ranges() {
        // Range 1: lines 0-2
        let range1_tokens = vec![
            (0, 5, 3, 1, 0),
            (1, 3, 4, 2, 0),
            (2, 7, 2, 1, 0),
        ];

        // Range 2: lines 5-7 (non-overlapping)
        let range2_tokens = vec![
            (5, 0, 5, 1, 0),
            (6, 2, 3, 2, 0),
            (7, 10, 4, 1, 0),
        ];

        // Merge tokens
        let mut all_tokens = Vec::new();
        all_tokens.extend(range1_tokens.iter().cloned());
        all_tokens.extend(range2_tokens.iter().cloned());
        all_tokens.sort_by_key(|t| (t.0, t.1));

        // Re-encode
        let merged_data = encode_tokens(&all_tokens);
        let merged = SemanticTokens::from_full(merged_data);
        let decoded = decode_tokens(&merged);

        // Should have all 6 tokens in order
        assert_eq!(decoded.len(), 6);
        assert_eq!(decoded[0], (0, 5, 3, 1, 0));
        assert_eq!(decoded[5], (7, 10, 4, 1, 0));
    }

    #[test]
    fn test_merge_overlapping_ranges_deduplication() {
        // Range 1: lines 0-5
        let range1_tokens = vec![
            (0, 5, 3, 1, 0),
            (3, 7, 4, 2, 0),
            (5, 2, 2, 1, 0),
        ];

        // Range 2: lines 3-7 (overlaps at lines 3-5)
        let range2_tokens = vec![
            (3, 7, 4, 2, 0),  // Duplicate
            (5, 2, 2, 1, 0),  // Duplicate
            (7, 10, 3, 1, 0),
        ];

        // Merge tokens
        let mut all_tokens = Vec::new();
        all_tokens.extend(range1_tokens.iter().cloned());
        all_tokens.extend(range2_tokens.iter().cloned());
        all_tokens.sort_by_key(|t| (t.0, t.1));
        
        // Deduplicate
        all_tokens.dedup_by_key(|t| (t.0, t.1, t.2));

        // Should have 4 unique tokens (2 duplicates removed)
        assert_eq!(all_tokens.len(), 4);
        assert_eq!(all_tokens[0], (0, 5, 3, 1, 0));
        assert_eq!(all_tokens[1], (3, 7, 4, 2, 0));
        assert_eq!(all_tokens[2], (5, 2, 2, 1, 0));
        assert_eq!(all_tokens[3], (7, 10, 3, 1, 0));
    }

    #[test]
    fn test_empty_token_set() {
        let tokens = vec![];
        let encoded = encode_tokens(&tokens);
        let semantic_tokens = SemanticTokens::from_full(encoded);
        let decoded = decode_tokens(&semantic_tokens);

        assert_eq!(decoded.len(), 0);
    }

    #[test]
    fn test_single_token() {
        let tokens = vec![(5, 10, 3, 1, 0)];
        let encoded = encode_tokens(&tokens);
        let semantic_tokens = SemanticTokens::from_full(encoded);
        let decoded = decode_tokens(&semantic_tokens);

        assert_eq!(decoded, tokens);
    }

    #[test]
    fn test_tokens_on_same_line() {
        let tokens = vec![
            (5, 0, 3, 1, 0),
            (5, 5, 4, 2, 0),
            (5, 10, 2, 1, 0),
            (5, 15, 5, 3, 0),
        ];

        let encoded = encode_tokens(&tokens);
        let semantic_tokens = SemanticTokens::from_full(encoded);
        let decoded = decode_tokens(&semantic_tokens);

        assert_eq!(decoded, tokens);
    }

    #[test]
    fn test_out_of_order_merge() {
        // Ranges provided out of order
        let range3 = vec![(20, 5, 3, 1, 0)];
        let range1 = vec![(5, 0, 4, 2, 0)];
        let range2 = vec![(10, 7, 2, 1, 0)];

        let mut all_tokens = Vec::new();
        all_tokens.extend(range3);
        all_tokens.extend(range1);
        all_tokens.extend(range2);
        
        // Sort to correct order
        all_tokens.sort_by_key(|t| (t.0, t.1));

        // Should be in document order
        assert_eq!(all_tokens[0].0, 5);
        assert_eq!(all_tokens[1].0, 10);
        assert_eq!(all_tokens[2].0, 20);
    }

    #[test]
    fn test_merge_with_different_token_types() {
        let tokens = vec![
            (0, 0, 3, 1, 0),   // Type 1
            (0, 5, 4, 2, 0),   // Type 2
            (0, 10, 2, 3, 0),  // Type 3
            (1, 0, 5, 1, 1),   // Type 1, modifier 1
        ];

        let encoded = encode_tokens(&tokens);
        let semantic_tokens = SemanticTokens::from_full(encoded);
        let decoded = decode_tokens(&semantic_tokens);

        assert_eq!(decoded, tokens);
        // Verify different types preserved
        assert_eq!(decoded[0].3, 1);
        assert_eq!(decoded[1].3, 2);
        assert_eq!(decoded[2].3, 3);
        // Verify modifier preserved
        assert_eq!(decoded[3].4, 1);
    }
}
