/// Errors the user interface can show; [`Error::code`] is the stable key for its texts.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Launchpad address: {0}")]
    InvalidLaunchpad(String),
    #[error("pairing was cancelled")]
    PairingCancelled,
    #[error("pairing timed out")]
    PairingTimedOut,
    #[error("pairing was declined in Lazurio")]
    PairingDeclined,
    #[error("invalid pairing result: {0}")]
    InvalidPairingResult(String),
    #[error("key storage: {0}")]
    KeyStore(String),
    #[error("key: {0}")]
    Key(String),
    #[error("unknown environment {0}")]
    UnknownEnvironment(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("data: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidLaunchpad(_) => "invalid-launchpad",
            Error::PairingCancelled => "pairing-cancelled",
            Error::PairingTimedOut => "pairing-timeout",
            Error::PairingDeclined => "pairing-declined",
            Error::InvalidPairingResult(_) => "pairing-invalid",
            Error::KeyStore(_) => "keystore",
            Error::Key(_) => "key",
            Error::UnknownEnvironment(_) => "unknown-environment",
            Error::Io(_) => "io",
            Error::Json(_) => "data",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
