//! Errors the SDK reports.
//!
//! Mirrors the TypeScript SDK's `HarnessError`: one error type with a machine
//! readable code, so a caller can branch on the cause instead of matching on
//! prose that is free to change.

use std::fmt;

/// What went wrong, in a form a caller can branch on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// The socket could not be dialed. Usually: the harness is not running.
    ConnectFailed,
    /// The version handshake was refused or answered with the wrong frame.
    HandshakeFailed,
    /// No reply arrived within the request timeout.
    Timeout,
    /// The connection is closed; nothing further can be sent on it.
    Disconnected,
    /// The reply was a different event kind than the request implies.
    UnexpectedReply,
    /// The harness answered with an error frame. Carries the wire code.
    Harness(jcode_harness_api::ErrorCode),
    /// The specific Ask request no longer accepts answers on this attachment.
    AskNotPending,
    /// I/O trouble on the transport.
    Transport,
    /// The peer sent a malformed frame. Reconnecting may replay the same corrupt data.
    Protocol,
    /// Starting a runtime (daemon or bridge) failed.
    LaunchFailed,
    /// The configured jcode executable could not be started.
    JcodeNotFound,
    /// The private bridge exited before its socket became ready.
    StartupFailed,
    /// The private bridge or SSH startup/handshake missed its deadline.
    StartupTimeout,
    /// An instance home was unsafe or aliased the user's own home.
    InvalidInstanceHome,
    /// An option was outside its documented range.
    InvalidOption,
    /// The connected runtime does not advertise a required capability.
    UnsupportedCapability,
    /// An operation requires a reconnectable native socket or SSH transport.
    UnsupportedTransport,
    /// A global event consumer fell behind its bounded queue.
    EventBufferOverflow,
    /// The caller cancelled a long-running operation (sign-in, host wake).
    Cancelled,
    /// Jcode Cloud rejected the account credentials. Sign in again.
    CloudAuth,
    /// The account has no Jcode Cloud entitlement or allowance left.
    CloudNotEntitled,
    /// The server's SSH host key did not match any pinned key.
    HostKeyMismatch,
    /// A paired Harness gateway could not be reached or refused this device.
    Paired(PairedFailure),
}

/// Why a paired Harness gateway connection failed, classified at the source so
/// a client can say what happened and offer the right next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairedFailure {
    /// The gateway's host name did not resolve. Usually: no network, VPN or
    /// tailnet is down.
    Unresolved,
    /// The host answered and refused the TCP connection. Nothing is listening.
    Refused,
    /// The connection or handshake did not finish before its deadline.
    TimedOut,
    /// The network path to the host is unavailable.
    Unreachable,
    /// The TLS handshake failed, for example an untrusted or mismatched certificate.
    Tls,
    /// The gateway rejected this device's token (HTTP 401 or 403): revoked or unknown.
    Revoked,
    /// The origin answered but does not serve a Harness gateway (HTTP 404).
    WrongOrigin,
    /// The gateway answered the websocket upgrade with something unusable.
    UpgradeRejected,
    /// The gateway is reachable but too old to serve Harness (`harness_v1` missing).
    TooOld,
    /// This device holds no saved token for the gateway, so there is nothing to present.
    NoToken,
}

impl PairedFailure {
    /// Stable lowercase identifier, usable as a wire or log code.
    pub fn code(self) -> &'static str {
        match self {
            Self::Unresolved => "paired_unresolved",
            Self::Refused => "paired_refused",
            Self::TimedOut => "paired_timeout",
            Self::Unreachable => "paired_unreachable",
            Self::Tls => "paired_tls",
            Self::Revoked => "paired_revoked",
            Self::WrongOrigin => "paired_wrong_origin",
            Self::UpgradeRejected => "paired_upgrade_rejected",
            Self::TooOld => "paired_too_old",
            Self::NoToken => "paired_no_token",
        }
    }
}

impl ErrorKind {
    /// Stable, lowercase identifier. Matches the TS SDK's code strings so the
    /// two SDKs describe the same failure by the same name.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConnectFailed => "connect_failed",
            Self::HandshakeFailed => "handshake_failed",
            Self::Timeout => "timeout",
            Self::Disconnected => "disconnected",
            Self::UnexpectedReply => "unexpected_reply",
            Self::Transport => "transport",
            Self::Protocol => "protocol",
            Self::LaunchFailed => "launch_failed",
            Self::JcodeNotFound => "jcode_not_found",
            Self::StartupFailed => "startup_failed",
            Self::StartupTimeout => "startup_timeout",
            Self::InvalidInstanceHome => "invalid_instance_home",
            Self::InvalidOption => "invalid_option",
            Self::UnsupportedCapability => "unsupported",
            Self::UnsupportedTransport => "unsupported_transport",
            Self::EventBufferOverflow => "event_buffer_overflow",
            Self::Cancelled => "cancelled",
            Self::CloudAuth => "cloud_auth",
            Self::CloudNotEntitled => "cloud_not_entitled",
            Self::HostKeyMismatch => "host_key_mismatch",
            Self::AskNotPending => "ask_not_pending",
            Self::Paired(failure) => failure.code(),
            Self::Harness(code) => match code {
                jcode_harness_api::ErrorCode::UnsupportedVersion => "unsupported_version",
                jcode_harness_api::ErrorCode::UnknownRequest => "unknown_request",
                jcode_harness_api::ErrorCode::UnknownSession => "unknown_session",
                jcode_harness_api::ErrorCode::UnknownSkill => "unknown_skill",
                jcode_harness_api::ErrorCode::InvalidRequest => "invalid_request",
                jcode_harness_api::ErrorCode::Internal => "internal",
                jcode_harness_api::ErrorCode::TranscriptionNotConfigured => {
                    "transcription_not_configured"
                }
                jcode_harness_api::ErrorCode::TranscriptionUnavailable => {
                    "transcription_unavailable"
                }
                jcode_harness_api::ErrorCode::TranscriptionTimeout => "transcription_timeout",
                jcode_harness_api::ErrorCode::AudioTooLarge => "audio_too_large",
            },
        }
    }
}

/// An SDK failure: a code plus a sentence that says what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// The stable code for this failure.
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for Error {}

impl From<jcode_harness_api::FrameError> for Error {
    fn from(error: jcode_harness_api::FrameError) -> Self {
        let kind = match error {
            jcode_harness_api::FrameError::Eof => ErrorKind::Disconnected,
            jcode_harness_api::FrameError::Json(_) => ErrorKind::Protocol,
            jcode_harness_api::FrameError::Io(ref error)
                if error.kind() == std::io::ErrorKind::InvalidData =>
            {
                ErrorKind::Protocol
            }
            _ => ErrorKind::Transport,
        };
        Self::new(kind, error.to_string())
    }
}

/// Result alias used throughout the SDK.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn classify_ask_response_error(mut error: Error) -> Error {
    if error.kind == ErrorKind::Harness(jcode_harness_api::ErrorCode::Internal)
        && error.message
            == "The input request is no longer pending on this connection; your answer was not accepted"
    {
        error.kind = ErrorKind::AskNotPending;
    }
    error
}
