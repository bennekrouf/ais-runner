//! One place to ask "is something listening on this local port?".
//!
//! The question was being asked in a dozen hand-rolled variants — `std` and
//! `tokio`, timeouts of 200 ms, 700 ms and 5 s — so the answer depended on who
//! asked. New code uses this; the older probes are left where their timeout is
//! deliberate (a retry loop waiting for a container to come up is not the same
//! question as a one-shot liveness check).

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

/// The port `func start` binds for the Logic Apps host.
pub const FUNC_PORT: u16 = 7071;

/// A localhost connect either completes or is refused immediately, so this
/// only bounds the pathological case of a filtered port.
const PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// Blocking, one-shot: is anything accepting connections on `127.0.0.1:port`?
///
/// It cannot tell you *whose* process that is. Callers that act on the answer
/// must be safe when the listener belongs to someone else's project.
pub fn is_listening(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}
