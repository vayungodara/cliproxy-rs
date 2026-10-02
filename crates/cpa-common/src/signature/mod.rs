//! Thought-signature provenance (internal/signature): which provider family can replay
//! a signed reasoning block, and the request sanitizers built on that decision.
//!
//! Every validator is a structural check on the signature bytes; nothing here verifies
//! cryptography. Detection order and envelope rules follow Go exactly because a wrong
//! claim either drops valid history or replays a foreign signature upstream (400).

mod claude;
mod claude_messages;
mod gemini;
mod gemini_sanitize;
mod gpt;
mod grok;
mod kimi;
pub(crate) mod wire;

pub use claude::*;
pub use claude_messages::*;
pub use gemini::*;
pub use gemini_sanitize::*;
pub use gpt::*;
pub use grok::*;
pub use kimi::*;

/// A validation failure. The text matches Go's error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

pub(crate) fn err<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error(message.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Unknown,
    Claude,
    Gemini,
    GeminiBypass,
    Gpt,
    Kimi,
    /// Target-only: detection never returns it.
    Grok,
    Swe,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
            Self::GeminiBypass => "gemini_bypass",
            Self::Gpt => "gpt",
            Self::Kimi => "kimi",
            Self::Grok => "grok",
            Self::Swe => "swe",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    Unknown,
    ClaudeThinking,
    GeminiModelPart,
    GeminiFunctionCall,
    GptReasoning,
}

impl BlockKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::ClaudeThinking => "claude_thinking",
            Self::GeminiModelPart => "gemini_model_part",
            Self::GeminiFunctionCall => "gemini_function_call",
            Self::GptReasoning => "gpt_reasoning",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Action {
    #[default]
    None,
    Preserve,
    DropBlock,
    DropSignature,
    ReplaceWithGeminiBypass,
    NoCompatibleReplacement,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Preserve => "preserve",
            Self::DropBlock => "drop_block",
            Self::DropSignature => "drop_signature",
            Self::ReplaceWithGeminiBypass => "replace_with_gemini_bypass",
            Self::NoCompatibleReplacement => "no_compatible_replacement",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub target_provider: Provider,
    pub detected_provider: Provider,
    pub block_kind: BlockKind,
    pub compatible: bool,
    pub action: Action,
    pub replacement_signature: String,
    pub normalized_signature: String,
    pub reason: String,
}

/// `SignatureProviderFromModelName`.
pub fn provider_from_model_name(model: &str) -> Provider {
    let lower = model.trim().to_lowercase();
    let has = |s: &str| lower.contains(s);
    let starts = |s: &str| lower.starts_with(s);
    if has("claude") {
        Provider::Claude
    } else if has("gemini") {
        Provider::Gemini
    } else if has("gpt") || has("openai") || has("codex") || starts("o1") || starts("o3") || starts("o4") {
        Provider::Gpt
    } else if has("kimi") || has("moonshot") || starts("k2") || starts("k3") {
        Provider::Kimi
    } else if has("grok") {
        Provider::Grok
    } else if has("swe-") {
        Provider::Swe
    } else {
        Provider::Unknown
    }
}

/// Base64 first characters a self-describing envelope can start with (Claude CAIS
/// `C`, Claude/Gemini `E`, Claude double layer `R`, GPT Fernet `g`).
const SELF_DESCRIBING_FIRST_CHARS: &[u8] = b"CERg";

/// The alphanumeric base64 core plus `extra`, as Go's `base64AlphabetSet`.
pub(crate) fn in_base64_alphabet(b: u8, extra: &[u8]) -> bool {
    b.is_ascii_alphanumeric() || extra.contains(&b)
}

/// First byte outside the alphabet with the rune Go's `utf8.DecodeRuneInString`
/// reports there (U+FFFD for an invalid sequence).
pub(crate) fn first_invalid_char(sig: &str, extra: &[u8]) -> Option<(usize, u32)> {
    let index = sig.bytes().position(|b| !in_base64_alphabet(b, extra))?;
    let rune = sig[index..].chars().next().map_or(0xfffd, |c| c as u32);
    Some((index, rune))
}

pub(crate) fn maybe_self_describing_envelope(sig: &str) -> bool {
    sig.as_bytes()
        .first()
        .is_some_and(|b| SELF_DESCRIBING_FIRST_CHARS.contains(b))
}

/// `DetectSignatureProvider`.
pub fn detect_provider(raw: &str) -> Provider {
    detect_provider_for_block(raw, BlockKind::Unknown)
}

/// `DetectSignatureProviderForBlock`. UUID-shaped payloads are never claimed as
/// replay-safe; Gemini targets replace them with the bypass sentinel.
pub fn detect_provider_for_block(raw: &str, block_kind: BlockKind) -> Provider {
    let sig = raw.trim();
    if sig.is_empty() {
        return Provider::Unknown;
    }
    if let Some((prefixed, unprefixed)) = split_provider_prefix(sig) {
        return match prefixed {
            Provider::Gemini if is_gemini_bypass(unprefixed) => Provider::GeminiBypass,
            Provider::Gemini if is_recognized_gemini_signature(unprefixed, block_kind) => Provider::Gemini,
            Provider::Claude
                if is_valid_claude_thinking_signature(unprefixed, ClaudeValidation::STRICT)
                    || is_valid_claude_cais_signature(unprefixed) =>
            {
                Provider::Claude
            }
            Provider::Gpt if is_valid_gpt_reasoning_signature(unprefixed) => Provider::Gpt,
            Provider::Swe if unprefixed.starts_with("sealed.v1.") => Provider::Swe,
            _ => Provider::Unknown,
        };
    }
    if sig.contains('#') {
        return Provider::Unknown;
    }
    if is_gemini_bypass(sig) {
        return Provider::GeminiBypass;
    }
    if sig.starts_with("sealed.v1.") {
        return Provider::Swe;
    }
    if maybe_self_describing_envelope(sig) {
        if is_valid_gpt_reasoning_signature(sig) {
            return Provider::Gpt;
        }
        if is_valid_claude_cais_signature(sig) {
            return Provider::Claude;
        }
        if is_valid_claude_thinking_signature(sig, ClaudeValidation::STRICT) {
            return Provider::Claude;
        }
        if is_recognized_gemini_signature(sig, block_kind) {
            return Provider::Gemini;
        }
    }
    if is_valid_kimi_thinking_signature(sig) {
        return Provider::Kimi;
    }
    Provider::Unknown
}

/// `IsSignatureCompatibleWithProvider`.
pub fn is_compatible_with_provider(target: Provider, raw: &str) -> bool {
    decide_compatibility(target, raw, BlockKind::Unknown).compatible
}

/// `DecideSignatureCompatibility`.
pub fn decide_compatibility(target: Provider, raw: &str, block_kind: BlockKind) -> Decision {
    decide_compatibility_for_model(target, "", raw, block_kind)
}

/// `DecideSignatureCompatibilityForModel`.
pub fn decide_compatibility_for_model(
    target: Provider,
    target_model: &str,
    raw: &str,
    block_kind: BlockKind,
) -> Decision {
    let target = normalize_target(target);
    let detected = detect_provider_for_block(raw, block_kind);
    let mut decision = Decision {
        target_provider: target,
        detected_provider: detected,
        block_kind,
        compatible: false,
        action: Action::None,
        replacement_signature: String::new(),
        normalized_signature: String::new(),
        reason: String::new(),
    };
    if provider_matches_target(target, detected) {
        decision.compatible = true;
        decision.action = Action::Preserve;
        decision.normalized_signature = normalize_compatible_signature(target, raw, block_kind);
        decision.reason = claude_compatible_reason(target, raw, target_model);
        return decision;
    }
    let (action, reason) = match target {
        Provider::Gemini => {
            if matches!(
                block_kind,
                BlockKind::GeminiFunctionCall | BlockKind::GeminiModelPart | BlockKind::Unknown
            ) {
                decision.replacement_signature = GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.into();
                (Action::ReplaceWithGeminiBypass, "missing or incompatible signature")
            } else {
                (
                    Action::DropBlock,
                    "signature is not compatible with Gemini and this block is not a bypass-safe Gemini model part",
                )
            }
        }
        Provider::Claude => (
            Action::DropBlock,
            "Claude has no cross-provider bypass sentinel for thinking blocks",
        ),
        Provider::Gpt => (
            Action::DropBlock,
            "GPT reasoning encrypted_content cannot be synthesized from another provider signature",
        ),
        Provider::Swe => (
            Action::DropBlock,
            "SWE requires sealed.v1 signature from its own backend",
        ),
        Provider::Kimi => (
            Action::DropSignature,
            "Kimi does not validate replayed thinking signatures, so the block survives without one",
        ),
        Provider::Grok => (
            Action::DropBlock,
            "xAI verifies encrypted_content on replay and rejects foreign or mutated blobs",
        ),
        _ => (Action::NoCompatibleReplacement, "unknown target provider"),
    };
    decision.action = action;
    decision.reason = reason.into();
    decision
}

/// `SplitSignatureProviderPrefix`: `Some((provider, rest))` for this proxy's
/// `provider#signature` cache envelope.
pub fn split_provider_prefix(raw: &str) -> Option<(Provider, &str)> {
    let (prefix, rest) = raw.trim().split_once('#')?;
    match provider_from_cache_prefix(prefix) {
        Provider::Unknown => None,
        provider => Some((provider, rest.trim())),
    }
}

/// `SignatureProviderFromCachePrefix`.
pub fn provider_from_cache_prefix(prefix: &str) -> Provider {
    match prefix.trim().to_lowercase().as_str() {
        "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax" | "claude-code-max"
        | "claude_code_max" => Provider::Claude,
        "gemini" | "google" => Provider::Gemini,
        "openai" | "gpt" | "codex" => Provider::Gpt,
        "swe" | "sealed" => Provider::Swe,
        _ => Provider::Unknown,
    }
}

/// `SignaturePayloadWithoutProviderPrefix`: the value to replay upstream.
pub fn payload_without_provider_prefix(raw: &str) -> &str {
    match split_provider_prefix(raw) {
        Some((_, rest)) => rest,
        None => raw.trim(),
    }
}

/// `CompatibleSignatureForProvider`.
pub fn compatible_signature_for_provider(target: Provider, raw: &str) -> Option<String> {
    compatible_signature_for_provider_block(target, raw, BlockKind::Unknown)
}

/// `CompatibleSignatureForProviderBlock`.
pub fn compatible_signature_for_provider_block(target: Provider, raw: &str, block_kind: BlockKind) -> Option<String> {
    let decision = decide_compatibility(target, raw, block_kind);
    (decision.compatible && !decision.normalized_signature.is_empty()).then_some(decision.normalized_signature)
}

/// `CompatibleAntigravityClaudeThinkingSignature`: the double-layer `R` form, only for
/// signatures strictly identifiable as Claude.
pub fn compatible_antigravity_claude_thinking_signature(raw: &str) -> Option<String> {
    if detect_provider_for_block(raw, BlockKind::ClaudeThinking) != Provider::Claude {
        return None;
    }
    normalize_claude_thinking_signature(payload_without_provider_prefix(raw), ClaudeValidation::STRICT).ok()
}

fn claude_compatible_reason(target: Provider, raw: &str, target_model: &str) -> String {
    const GENERIC: &str = "signature provider matches target provider";
    if target != Provider::Claude {
        return GENERIC.into();
    }
    let Ok(info) = inspect_claude_cais_signature(payload_without_provider_prefix(raw)) else {
        return GENERIC.into();
    };
    let mut reason = if !info.model_text.is_empty() {
        format!(
            "valid Claude CAIS signature with embedded model {} is compatible with any Claude target",
            info.model_text
        )
    } else if info.envelope_version >= 4 {
        "valid Claude CAQS signature is compatible with any Claude target".into()
    } else {
        "valid Claude CAIS signature is compatible with any Claude target".into()
    };
    let model = target_model.trim();
    if !model.is_empty() {
        reason.push_str(", including target model ");
        reason.push_str(model);
    }
    reason
}

pub(crate) fn normalize_target(provider: Provider) -> Provider {
    if provider == Provider::GeminiBypass {
        Provider::Gemini
    } else {
        provider
    }
}

fn provider_matches_target(target: Provider, detected: Provider) -> bool {
    match target {
        Provider::Gemini => matches!(detected, Provider::Gemini | Provider::GeminiBypass),
        Provider::Claude | Provider::Gpt | Provider::Swe | Provider::Kimi => detected == target,
        _ => false,
    }
}

fn normalize_compatible_signature(target: Provider, raw: &str, block_kind: BlockKind) -> String {
    let payload = payload_without_provider_prefix(raw);
    let ok = |valid: bool| if valid { payload.to_owned() } else { String::new() };
    match normalize_target(target) {
        Provider::Claude => {
            if is_valid_claude_cais_signature(payload) {
                return payload.to_owned();
            }
            normalize_claude_provider_native_thinking_signature(payload, ClaudeValidation::default())
                .unwrap_or_default()
        }
        Provider::Gemini => ok(is_gemini_bypass(payload) || is_recognized_gemini_signature(payload, block_kind)),
        Provider::Gpt => ok(is_valid_gpt_reasoning_signature(payload)),
        Provider::Swe => ok(payload.starts_with("sealed.v1.")),
        Provider::Kimi => ok(is_valid_kimi_thinking_signature(payload)),
        _ => String::new(),
    }
}

pub(crate) fn is_recognized_gemini_signature(raw: &str, _block_kind: BlockKind) -> bool {
    if is_valid_claude_cais_signature(raw) {
        return false;
    }
    is_valid_gemini_thought_signature(
        raw,
        GeminiValidation {
            require_known_envelope: true,
            ..Default::default()
        },
    )
}

/// `IsRecognizedReasoningSignature`: structurally valid for any known provider.
pub fn is_recognized_reasoning_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() {
        return false;
    }
    detect_provider(sig) != Provider::Unknown || is_valid_grok_encrypted_content(sig)
}

#[cfg(test)]
mod tests;
