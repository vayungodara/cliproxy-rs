//! Wire formats a request or response can be in (sdk/translator/formats.go).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Format {
    /// OpenAI Chat Completions.
    OpenAI,
    /// OpenAI Responses.
    OpenAIResponse,
    /// Anthropic Messages.
    Claude,
    /// Gemini generateContent.
    Gemini,
    /// Codex flavour of the Responses API.
    Codex,
    /// Antigravity envelope around Gemini.
    Antigravity,
    /// Gemini Interactions.
    Interactions,
}

impl Format {
    pub const ALL: [Format; 7] = [
        Format::OpenAI,
        Format::OpenAIResponse,
        Format::Claude,
        Format::Gemini,
        Format::Codex,
        Format::Antigravity,
        Format::Interactions,
    ];

    /// Identifier used by CLIProxyAPI in config and payload rules.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::OpenAI => "openai",
            Format::OpenAIResponse => "openai-response",
            Format::Claude => "claude",
            Format::Gemini => "gemini",
            Format::Codex => "codex",
            Format::Antigravity => "antigravity",
            Format::Interactions => "interactions",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }
}
