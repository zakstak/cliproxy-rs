//! Wire formats a request or response can be in (sdk/translator/formats.go).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Format {
    /// OpenAI Chat Completions.
    OpenAI,
    /// OpenAI Responses.
    OpenAIResponse,
    /// Anthropic Messages.
    Claude,
    /// Codex flavour of the Responses API.
    Codex,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::OpenAI, Format::OpenAIResponse, Format::Claude, Format::Codex];

    /// Identifier used by CLIProxyAPI in config and payload rules.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::OpenAI => "openai",
            Format::OpenAIResponse => "openai-response",
            Format::Claude => "claude",
            Format::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }
}
