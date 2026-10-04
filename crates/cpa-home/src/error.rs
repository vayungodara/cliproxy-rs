//! Home client errors. `Display` reproduces Go's error text: several of these reach
//! clients verbatim inside auth error messages (`home auth not found`).

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Disabled,
    NotConnected,
    EmptyResponse,
    AuthNotFound,
    ConfigNotFound,
    ModelsNotFound,
    PluginSyncUnsupported(String),
    /// This client lifetime ended (closed or an ambiguous dispatch fenced it).
    DispatchFenced,
    /// This Home predates the `CAS` command.
    CompareAndSwapUnsupported,
    /// Home answered with a RESP error reply (go-redis `redis.Error`).
    Server(String),
    /// The connection failed: dial, TLS, I/O, protocol or a closed connection.
    Transport(String),
    /// No reply within the operation deadline.
    Timeout,
    /// An issued auth dispatch failed after the request was sent: Home may have
    /// granted a lease, so the client lifetime must be fenced, never retried.
    Ambiguous(Box<Error>),
    /// Local validation, decoding or configuration failure.
    Other(String),
    /// The caller's lifetime ended (Go `context.Canceled`).
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Disabled => f.write_str("home client disabled"),
            Error::NotConnected => f.write_str("home not connected"),
            Error::EmptyResponse => f.write_str("home returned empty response"),
            Error::AuthNotFound => f.write_str("home auth not found"),
            Error::ConfigNotFound => f.write_str("home config not found"),
            Error::ModelsNotFound => f.write_str("home models not found"),
            Error::PluginSyncUnsupported(message) => write!(f, "home plugin sync is unsupported: {message}"),
            Error::DispatchFenced => f.write_str("home auth dispatch is fenced"),
            Error::CompareAndSwapUnsupported => f.write_str("home compare-and-swap is unsupported"),
            Error::Server(message) | Error::Transport(message) | Error::Other(message) => f.write_str(message),
            Error::Timeout => f.write_str("i/o timeout"),
            Error::Cancelled => f.write_str("context canceled"),
            Error::Ambiguous(inner) => inner.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::TimedOut {
            return Error::Timeout;
        }
        Error::Transport(error.to_string())
    }
}

impl Error {
    /// Go `IsAmbiguousDispatchError`.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Error::Ambiguous(_))
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Timeout)
    }

    /// Go `isHomeCommandUnsupported`: Home rejected a command it does not implement.
    pub fn is_command_unsupported(&self) -> bool {
        let message = self.to_string().trim().to_lowercase();
        message.contains("unknown command") || message.contains("unsupported command")
    }

    /// Go `IsMembershipTakeoverUnavailableError`.
    pub fn is_membership_takeover_unavailable(&self) -> bool {
        let message = self.to_string().trim().to_lowercase();
        message == "membership_takeover_unavailable" || message == "err membership_takeover_unavailable"
    }

    /// Go `IsLegacyMembershipProtocolError`: an old Home rejected the membership
    /// arguments of `SUBSCRIBE`.
    pub fn is_legacy_membership_protocol(&self) -> bool {
        let message = self.to_string().trim().to_lowercase();
        message == "wrong number of arguments for 'subscribe' command"
            || message == "err wrong number of arguments for 'subscribe' command"
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A decode failure without the value text serde embeds in its messages
/// (`invalid type: string "..."`): KV entries and enrollment JWTs carry secrets.
pub(crate) fn redacted_decode_error(context: &str, error: &serde_json::Error) -> Error {
    Error::Other(format!("{context}: {}", redacted_decode_text(error)))
}

/// The kind and position of a decode failure, never the value: for callers outside
/// this crate that decode Home KV values themselves.
pub fn redacted_decode_text(error: &serde_json::Error) -> String {
    let kind = match error.classify() {
        serde_json::error::Category::Io => "read",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "type",
        serde_json::error::Category::Eof => "truncated input",
    };
    format!("{kind} error at line {} column {}", error.line(), error.column())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_errors_never_echo_values() {
        let error = serde_json::from_str::<u64>(r#""sensitive-token""#).unwrap_err();
        assert!(error.to_string().contains("sensitive-token"));
        let clean = redacted_decode_error("home kv", &error).to_string();
        assert_eq!(clean, "home kv: type error at line 1 column 17");
    }

    #[test]
    fn classifiers_match_go_message_checks() {
        assert!(Error::Server("ERR unknown command 'CAS'".into()).is_command_unsupported());
        assert!(Error::Server("ERR Unsupported Command".into()).is_command_unsupported());
        assert!(!Error::Server("ERR denied".into()).is_command_unsupported());
        assert!(Error::Server("ERR membership_takeover_unavailable".into()).is_membership_takeover_unavailable());
        assert!(Error::Server(" membership_takeover_unavailable ".into()).is_membership_takeover_unavailable());
        assert!(!Error::Server("ERR membership_takeover_unavailable now".into()).is_membership_takeover_unavailable());
        assert!(
            Error::Server("ERR wrong number of arguments for 'subscribe' command".into())
                .is_legacy_membership_protocol()
        );
        assert!(Error::Ambiguous(Box::new(Error::Timeout)).is_ambiguous());
        assert_eq!(Error::Ambiguous(Box::new(Error::Timeout)).to_string(), "i/o timeout");
        assert_eq!(Error::AuthNotFound.to_string(), "home auth not found");
    }
}
