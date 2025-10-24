use clock::Global;
use collections::HashMap;
use gpui::HighlightStyle;
use language::BufferSnapshot;
use lsp::SemanticTokenType;
use project::lsp_store::semantic_tokens::SemanticTokens;
use std::{ops::Range, sync::Arc};
use sum_tree::Bias;
use text::{OffsetUtf16, PointUtf16};
use theme::SyntaxTheme;

use crate::{editor_settings::RainbowConfig, rainbow::VariableColorCache};

#[derive(Debug, Default)]
pub struct SemanticTokenBufferContainer {
    pub tokens: Vec<MultibufferSemanticToken>,
    pub version: Global,
    /// Tracks whether this view was generated with rainbow cache available.
    /// Used to detect when tokens need regeneration due to rainbow highlighting toggle.
    pub had_rainbow_cache: bool,
}

/// A `SemanticToken`, but attached to a `MultiBuffer`.
#[derive(Debug)]
pub struct MultibufferSemanticToken {
    pub range: Range<usize>,
    pub style: HighlightStyle,

    // These are only used in the debug syntax tree.
    pub lsp_type: u32,
    pub lsp_modifiers: u32,
}

impl SemanticTokenBufferContainer {
    pub fn new(
        buffer_snapshot: &BufferSnapshot,
        lsp: &SemanticTokens,
        legend: &lsp::SemanticTokensLegend,
        variable_color_cache: Option<&Arc<VariableColorCache>>,
        syntax_theme: Option<&SyntaxTheme>,
        rainbow_config: RainbowConfig,
    ) -> Option<SemanticTokenBufferContainer> {
        let stylizer = SemanticTokenStylizer::new(legend, &rainbow_config);

        let mut tokens = lsp
            .tokens()
            .filter_map(|token| {
                let start = text::Unclipped(PointUtf16::new(token.line, token.start));
                let (start_offset, end_offset) = point_offset_to_offsets(
                    buffer_snapshot.clip_point_utf16(start, Bias::Left),
                    OffsetUtf16(token.length as usize),
                    &buffer_snapshot.text,
                );

                let style = stylizer.convert(
                    syntax_theme,
                    token.token_type,
                    token.token_modifiers,
                    &buffer_snapshot.text,
                    start_offset..end_offset,
                    variable_color_cache,
                )?;

                Some(MultibufferSemanticToken {
                    range: start_offset..end_offset,
                    style,
                    lsp_type: token.token_type,
                    lsp_modifiers: token.token_modifiers,
                })
            })
            .collect::<Vec<_>>();

        // These should be sorted, but we rely on it for binary searching, so let's be sure.
        tokens.sort_by_key(|token| token.range.start);

        Some(SemanticTokenBufferContainer {
            tokens,
            version: buffer_snapshot.version().clone(),
            had_rainbow_cache: variable_color_cache.is_some(),
        })
    }

    pub fn tokens_in_range(&self, range: Range<usize>) -> &[MultibufferSemanticToken] {
        let start = self
            .tokens
            .binary_search_by_key(&range.start, |token| token.range.start)
            .unwrap_or_else(|next_ix| next_ix);

        let end = self
            .tokens
            .binary_search_by_key(&range.end, |token| token.range.start)
            .unwrap_or_else(|next_ix| next_ix);

        &self.tokens[start..end]
    }
}

fn point_offset_to_offsets(
    point: PointUtf16,
    length: OffsetUtf16,
    buffer: &text::BufferSnapshot,
) -> (usize, usize) {
    let start = buffer.as_rope().point_utf16_to_offset(point);
    let start_offset = buffer.as_rope().offset_to_offset_utf16(start);
    let end_offset = start_offset + length;
    let end = buffer.as_rope().offset_utf16_to_offset(end_offset);

    (start, end)
}

/// Stylizer for LSP semantic tokens with encapsulated rainbow highlighting logic.
struct SemanticTokenStylizer<'a> {
    token_types: Vec<&'a str>,
    modifier_mask: HashMap<&'a str, u32>,
    rainbow_enabled: bool,
    rainbow_token_types: &'a [crate::editor_settings::RainbowTokenType],
}

impl<'a> SemanticTokenStylizer<'a> {
    pub fn new(
        legend: &'a lsp::SemanticTokensLegend,
        rainbow_config: &'a RainbowConfig,
    ) -> Self {
        let token_types = legend.token_types.iter().map(|s| s.as_str()).collect();
        let modifier_mask = legend
            .token_modifiers
            .iter()
            .enumerate()
            .map(|(i, modifier)| (modifier.as_str(), 1 << i))
            .collect();

        SemanticTokenStylizer {
            token_types,
            modifier_mask,
            rainbow_enabled: rainbow_config.enabled,
            rainbow_token_types: &rainbow_config.token_types,
        }
    }

    pub fn token_type(&self, token_type: u32) -> Option<&'a str> {
        self.token_types.get(token_type as usize).copied()
    }

    pub fn has_modifier(&self, token_modifiers: u32, modifier: &str) -> bool {
        let Some(mask) = self.modifier_mask.get(modifier) else {
            return false;
        };
        (token_modifiers & mask) != 0
    }

    fn apply_rainbow(
        &self,
        buffer: &text::BufferSnapshot,
        range: Range<usize>,
        variable_color_cache: Option<&Arc<VariableColorCache>>,
        theme: Option<&'a SyntaxTheme>,
    ) -> Option<HighlightStyle> {
        let cache = variable_color_cache?;
        let theme = theme?;
        let identifier: String = buffer.text_for_range(range).collect();
        let style = cache.get_or_insert(&identifier, theme);
        style.color.as_ref()?;
        Some(style)
    }

    pub fn convert(
        &self,
        theme: Option<&'a SyntaxTheme>,
        token_type: u32,
        modifiers: u32,
        buffer: &text::BufferSnapshot,
        range: Range<usize>,
        variable_color_cache: Option<&Arc<VariableColorCache>>,
    ) -> Option<HighlightStyle> {
        let token_type_name = self.token_type(token_type)?;
        let has_modifier = |modifier| self.has_modifier(modifiers, modifier);

        if self.rainbow_enabled && variable_color_cache.is_some() && theme.is_some() {
            let should_apply_rainbow =
                self.rainbow_token_types
                    .iter()
                    .any(|rainbow_type| match rainbow_type {
                        crate::editor_settings::RainbowTokenType::Parameter => {
                            token_type_name == SemanticTokenType::PARAMETER.as_str()
                        }
                        crate::editor_settings::RainbowTokenType::Variable => {
                            token_type_name == SemanticTokenType::VARIABLE.as_str()
                                && !has_modifier("defaultLibrary")
                                && !has_modifier("constant")
                        }
                        crate::editor_settings::RainbowTokenType::Property => {
                            token_type_name == SemanticTokenType::PROPERTY.as_str()
                        }
                    });

            if should_apply_rainbow {
                if let Some(style) = self.apply_rainbow(buffer, range, variable_color_cache, theme)
                {
                    return Some(style);
                }
            }
        }

        let choices: &[&str] = match token_type_name {
            // Types
            token if token == SemanticTokenType::NAMESPACE.as_str() => {
                &["namespace", "module", "type"]
            }
            token if token == SemanticTokenType::CLASS.as_str() => &[
                "type.class.definition",
                "type.definition",
                "type.class",
                "class",
                "type",
            ],
            token
                if token == SemanticTokenType::ENUM.as_str()
                    && (has_modifier("declaration") || has_modifier("definition")) =>
            {
                &[
                    "type.enum.definition",
                    "type.definition",
                    "type.enum",
                    "enum",
                    "type",
                ]
            }
            token if token == SemanticTokenType::ENUM.as_str() => &["type.enum", "enum", "type"],
            token
                if token == SemanticTokenType::INTERFACE.as_str()
                    && (has_modifier("declaration") || has_modifier("definition")) =>
            {
                &[
                    "type.interface.definition",
                    "type.definition",
                    "type.interface",
                    "interface",
                    "type",
                ]
            }
            token if token == SemanticTokenType::INTERFACE.as_str() => {
                &["type.interface", "interface", "type"]
            }
            token
                if token == SemanticTokenType::STRUCT.as_str()
                    && (has_modifier("declaration") || has_modifier("definition")) =>
            {
                &[
                    "type.struct.definition",
                    "type.definition",
                    "type.struct",
                    "struct",
                    "type",
                ]
            }
            token if token == SemanticTokenType::STRUCT.as_str() => {
                &["type.struct", "struct", "type"]
            }
            token
                if token == SemanticTokenType::TYPE_PARAMETER.as_str()
                    && (has_modifier("declaration") || has_modifier("definition")) =>
            {
                &[
                    "type.parameter.definition",
                    "type.definition",
                    "type.parameter",
                    "type",
                ]
            }
            token if token == SemanticTokenType::TYPE_PARAMETER.as_str() => {
                &["type.parameter", "type"]
            }
            token
                if token == SemanticTokenType::TYPE.as_str()
                    && (has_modifier("declaration") || has_modifier("definition")) =>
            {
                &["type.definition", "type"]
            }
            token if token == SemanticTokenType::TYPE.as_str() => &["type"],

            // References
            token if token == SemanticTokenType::PARAMETER.as_str() => &["parameter"],
            token
                if token == SemanticTokenType::VARIABLE.as_str()
                    && has_modifier("defaultLibrary")
                    && has_modifier("constant") =>
            {
                &["constant.builtin", "constant"]
            }
            token
                if token == SemanticTokenType::VARIABLE.as_str()
                    && has_modifier("defaultLibrary") =>
            {
                &["variable.builtin", "variable"]
            }
            token if token == SemanticTokenType::VARIABLE.as_str() && has_modifier("constant") => {
                &["constant"]
            }
            token if token == SemanticTokenType::VARIABLE.as_str() => &["variable"],
            "const" => &["const", "constant", "variable"],
            token if token == SemanticTokenType::PROPERTY.as_str() => &["property"],
            token if token == SemanticTokenType::ENUM_MEMBER.as_str() => {
                &["type.enum.member", "type.enum", "variant"]
            }
            token if token == SemanticTokenType::DECORATOR.as_str() => {
                &["function.decorator", "function.annotation"]
            }

            // Declarations in the docs, but in practice, also references
            token
                if token == SemanticTokenType::FUNCTION.as_str()
                    && has_modifier("defaultLibrary") =>
            {
                &["function.builtin", "function"]
            }
            token if token == SemanticTokenType::FUNCTION.as_str() => &["function"],
            token
                if token == SemanticTokenType::METHOD.as_str()
                    && has_modifier("defaultLibrary") =>
            {
                &["function.builtin", "function.method", "function"]
            }
            token if token == SemanticTokenType::METHOD.as_str() => {
                &["function.method", "function"]
            }
            token if token == SemanticTokenType::MACRO.as_str() => &["function.macro", "function"],
            "label" => &["label"],

            // Tokens
            token
                if token == SemanticTokenType::COMMENT.as_str()
                    && has_modifier("documentation") =>
            {
                &["comment.documentation", "comment.doc", "comment"]
            }
            token if token == SemanticTokenType::COMMENT.as_str() => &["comment"],
            token if token == SemanticTokenType::STRING.as_str() => &["string"],
            token if token == SemanticTokenType::KEYWORD.as_str() => &["keyword"],
            token if token == SemanticTokenType::NUMBER.as_str() => &["number"],
            token if token == SemanticTokenType::REGEXP.as_str() => &["string.regexp", "string"],
            token if token == SemanticTokenType::OPERATOR.as_str() => &["operator"],

            // Not in the VS Code docs, but in the LSP spec.
            token if token == SemanticTokenType::MODIFIER.as_str() => &["keyword.modifier"],

            // Language specific bits.

            // C#. This is part of the spec, but not used elsewhere.
            token if token == SemanticTokenType::EVENT.as_str() => &["type.event", "type"],

            // Rust
            token if token == "lifetime" => &["symbol", "type.parameter", "type"],

            _ => {
                return None;
            }
        };

        // Theme color lookup: try each choice in order until we find one with a color
        if let Some(theme) = theme {
            for choice in choices {
                if let Some(style) = theme.get_opt(choice) {
                    if style.color.is_some() {
                        return Some(style);
                    }
                }
            }
        }

        // No color found in theme: return empty style to preserve tree-sitter syntax + rainbow colors
        Some(HighlightStyle::default())
    }
}
