//! Crate-wide error type for public fallible operations.

use thiserror::Error;

use crate::session::client::ClientError;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("PTY error: {0}")]
    Pty(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("daemon error: {0}")]
    Daemon(String),
    /// A daemon-control call failed. Transparent so `?` converts
    /// [`ClientError`] into this type with its message and source intact.
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::protocol::PROTOCOL_VERSION;

    #[test]
    fn every_client_error_converts_with_display_intact() {
        let incompatible = format!("session daemon protocol 99 != client {PROTOCOL_VERSION}");
        let cases = vec![
            (
                ClientError::NotRunning,
                "session daemon is not running".to_string(),
            ),
            (
                ClientError::Incompatible {
                    daemon_protocol: 99,
                },
                incompatible,
            ),
            (
                ClientError::Daemon("boom".into()),
                "daemon error: boom".to_string(),
            ),
            (
                ClientError::Unexpected("ping"),
                "unexpected reply to ping".to_string(),
            ),
            (
                ClientError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "raw")),
                "raw".to_string(),
            ),
        ];
        for (client, expected) in cases {
            let err: Error = client.into();
            assert!(matches!(err, Error::Client(_)));
            assert_eq!(err.to_string(), expected);
        }
    }

    #[test]
    fn question_mark_converts_across_the_boundary() {
        fn via_question_mark() -> Result<()> {
            Err(ClientError::Daemon("boom".into()))?;
            #[allow(unreachable_code)]
            Ok(())
        }
        assert_eq!(
            via_question_mark().unwrap_err().to_string(),
            "daemon error: boom"
        );
    }

    #[test]
    fn client_io_reports_the_io_error_as_source() {
        let client = ClientError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "x"));
        let src = std::error::Error::source(&client).expect("io source");
        assert_eq!(src.to_string(), "x");
        assert!(src.downcast_ref::<std::io::Error>().is_some());

        // Every other variant is a leaf.
        let leaves = [
            ClientError::NotRunning,
            ClientError::Incompatible { daemon_protocol: 1 },
            ClientError::Daemon("boom".into()),
            ClientError::Unexpected("ping"),
        ];
        for client in leaves {
            assert!(std::error::Error::source(&client).is_none());
        }
    }

    #[test]
    fn bridge_forwards_display_and_source_transparently() {
        let client = ClientError::Io(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "x"));
        let err: Error = client.into();
        assert_eq!(err.to_string(), "x");
        // Transparent: no wrapper level — straight to the io error itself.
        let fwd = std::error::Error::source(&err).expect("forwarded source");
        assert!(fwd.downcast_ref::<std::io::Error>().is_some());
    }
}
