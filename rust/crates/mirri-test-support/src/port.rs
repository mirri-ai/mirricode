//! Ephemeral TCP listeners on OS-assigned ports.
//!
//! Per the isolation contract (16-test-execution-strategy.md §4.2), every
//! listener must bind `127.0.0.1:0` and let the OS pick the port; hardcoded
//! ports are forbidden.

use std::io;
use std::net::TcpListener;

/// Bind a listener on `127.0.0.1:0` and let the OS assign a free port.
///
/// Async consumers can convert via `tokio::net::TcpListener::from_std`.
pub fn ephemeral_listener() -> io::Result<TcpListener> {
    TcpListener::bind("127.0.0.1:0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_bind_to_an_os_assigned_port() {
        let listener = ephemeral_listener().expect("ephemeral bind should succeed");
        let addr = listener
            .local_addr()
            .expect("local_addr should be available");
        assert_eq!(addr.ip().to_string(), "127.0.0.1");
        assert_ne!(addr.port(), 0, "OS must assign a concrete port");
    }
}
