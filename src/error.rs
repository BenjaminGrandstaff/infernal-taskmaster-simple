//! Goal: give every failure mode a specific, typed variant -- matching
//! infernal-law's own error style -- rather than collapsing configuration,
//! transport, and protocol failures into one opaque string.

use std::fmt::{self, Display, Formatter};

use infernal_client::ClientError;

#[derive(Debug)]
pub enum TaskmasterError {
    MissingEnv(&'static str),
    InvalidServiceId,
    Client(ClientError),
    UnexpectedStatus(u16),
    MalformedResponse(String),
    /// `KERNEL_CA_CERT_PATH` was set but the file could not be read.
    CaCertificateUnreadable(std::io::Error),
}

impl Display for TaskmasterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv(name) => {
                write!(formatter, "missing required environment variable {name}")
            }
            Self::InvalidServiceId => formatter.write_str("service ID must be a UUID"),
            Self::Client(error) => write!(formatter, "kernel client error: {error}"),
            Self::UnexpectedStatus(status) => {
                write!(formatter, "kernel returned unexpected status {status}")
            }
            Self::MalformedResponse(message) => {
                write!(formatter, "malformed kernel response: {message}")
            }
            Self::CaCertificateUnreadable(error) => {
                write!(formatter, "could not read KERNEL_CA_CERT_PATH: {error}")
            }
        }
    }
}

impl std::error::Error for TaskmasterError {}

impl From<ClientError> for TaskmasterError {
    fn from(error: ClientError) -> Self {
        Self::Client(error)
    }
}
