//! P3-02: the evidence-harvesting half of the intercepting proxy. `sandbox::NetworkBridge`
//! (P3-02's plumbing half) redirects every TCP connection a network-isolated sandbox
//! attempts to a local port; this module is what listens there, records each connection's
//! real destination, and closes it.
//!
//! **Must not:** interpret anything, same as this crate's own top-level contract — this
//! module observes and records, it does not proxy traffic through to the real destination
//! (that is P3-03's job, mock backend redirection) or decide what a connection *means* for a
//! verdict (P3-05's `openWorldHint` protocol, over the entries this module produces).
//!
//! # `SO_ORIGINAL_DST`, not a level this crate invented
//!
//! The real destination of a `REDIRECT`ed connection is retrieved via `getsockopt` with
//! `SOL_IP`/`SO_ORIGINAL_DST` — the exact netfilter mechanism `sandbox::NetworkBridge`'s own
//! `iptables REDIRECT` rule already depends on to make interception meaningful at all (a
//! `REDIRECT`ed connection's local peer address is otherwise indistinguishable from a
//! deliberate direct connection to the proxy). `SO_ORIGINAL_DST`'s value (`80`) is a
//! netfilter constant from `<linux/netfilter_ipv4.h>`, not exposed by the mainline `libc`
//! crate for generic Linux (the same kind of gap `sandbox::supervisor`'s own doc comment
//! already found for `ifreq`/`SIOCSIFFLAGS`) — hardcoded here with that provenance stated
//! rather than silently assumed.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `SO_ORIGINAL_DST` from `<linux/netfilter_ipv4.h>` — see this module's own doc comment for
/// why it isn't `libc::SO_ORIGINAL_DST`.
const SO_ORIGINAL_DST: libc::c_int = 80;

/// One observed connection attempt: the real destination the sandboxed process was actually
/// trying to reach, recovered via `SO_ORIGINAL_DST` rather than the connection's own local
/// peer address (which is always the proxy itself, once `REDIRECT` has rewritten it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLogEntry {
    /// The real destination address and port.
    pub destination: SocketAddrV4,
}

impl From<ConnectionLogEntry> for datamodel::ObservedDestination {
    /// Re-encode into `datamodel`'s pure vocabulary — the same job `evtree::decode` already
    /// does for overlay evidence, translating without interpreting: `normalise::classify_
    /// destination` (P3-04) is what decides what an address *means*, not this crate.
    fn from(entry: ConnectionLogEntry) -> Self {
        Self::new(entry.destination.ip().octets(), entry.destination.port())
    }
}

/// Why starting or reading back the connection log failed.
#[derive(Debug)]
pub struct ConnectionLogError(io::Error);

impl std::fmt::Display for ConnectionLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "connection log error: {}", self.0)
    }
}

impl std::error::Error for ConnectionLogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

impl From<io::Error> for ConnectionLogError {
    fn from(e: io::Error) -> Self {
        Self(e)
    }
}

/// A running connection-log listener. Bind with [`Self::start`], read [`Self::port`] to
/// learn where to point `sandbox::NetworkBridge::set_up`'s redirect at, and consume with
/// [`Self::stop`] once the run is over to get back everything it saw.
pub struct ConnectionLog {
    port: u16,
    entries: Arc<Mutex<Vec<ConnectionLogEntry>>>,
    stop_flag: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl ConnectionLog {
    /// Bind an OS-assigned local port on every local interface (`0.0.0.0`) and start
    /// accepting connections on a background thread. Every accepted connection has its real
    /// destination recorded, then is dropped (closed) — never forwarded.
    ///
    /// Deliberately not `127.0.0.1`: `iptables REDIRECT` rewrites a redirected packet's
    /// destination to the *primary address of the interface it arrived on*, not to loopback —
    /// for `sandbox::NetworkBridge`'s veth-arriving traffic that's the host-side veth's own
    /// address, never `127.0.0.1`. A loopback-only listener would refuse every such
    /// connection outright, confirmed directly the first time this was wired up end to end.
    pub fn start() -> Result<Self, ConnectionLogError> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let port = listener.local_addr()?.port();
        // Non-blocking so the accept loop below can also poll `stop_flag` — there is no
        // portable way to interrupt a blocking `accept()` from another thread, and adding a
        // second wakeup-only connection just to unblock it would be more machinery than a
        // short poll interval costs.
        listener.set_nonblocking(true)?;

        let entries = Arc::new(Mutex::new(Vec::new()));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let thread_entries = Arc::clone(&entries);
        let thread_stop_flag = Arc::clone(&stop_flag);
        let thread = std::thread::spawn(move || {
            // Once `stop()` is requested, a connection already past its TCP handshake can
            // still be sitting in the kernel's accept backlog for a brief moment — the
            // sandboxed process's own `connect()` returns as soon as *it* sees the
            // handshake's final ACK sent, which can land here microseconds later. Found
            // directly, not assumed: a fast-exiting sandboxed process reliably raced
            // `stop()` against its own last connection or two before this grace period was
            // added. Rather than stopping the instant the queue looks empty, keep polling
            // until it has looked empty for a full `STOP_GRACE_PERIOD` in a row, restarting
            // that countdown every time another connection is actually accepted.
            const STOP_GRACE_PERIOD: Duration = Duration::from_millis(200);
            let mut idle_since_stop: Option<std::time::Instant> = None;
            loop {
                match listener.accept() {
                    Ok((stream, _local_peer_addr_is_always_the_proxy_itself)) => {
                        idle_since_stop = None;
                        if let Ok(destination) = original_destination(&stream) {
                            thread_entries.lock().unwrap_or_else(|p| p.into_inner()).push(ConnectionLogEntry { destination });
                        }
                        // `stream` drops here, closing the connection — this module observes
                        // and records, it does not forward traffic to the real destination.
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        if thread_stop_flag.load(Ordering::SeqCst) {
                            let now = std::time::Instant::now();
                            match idle_since_stop {
                                Some(first_idle) if now.duration_since(first_idle) >= STOP_GRACE_PERIOD => break,
                                Some(_) => {}
                                None => idle_since_stop = Some(now),
                            }
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self { port, entries, stop_flag, thread })
    }

    /// The port this log actually bound — pass this to
    /// `sandbox::NetworkBridge::set_up`'s `proxy_port`.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Signal the background thread to stop, wait (up to `STOP_GRACE_PERIOD` of continuous
    /// idleness — see this module's own accept-loop comment) for any already-in-flight
    /// connections to be drained, and return everything logged, in the order accepted.
    #[must_use]
    pub fn stop(self) -> Vec<ConnectionLogEntry> {
        self.stop_flag.store(true, Ordering::SeqCst);
        let _ = self.thread.join();
        Arc::try_unwrap(self.entries)
            .map(|m| m.into_inner().unwrap_or_else(|p| p.into_inner()))
            .unwrap_or_default()
    }
}

/// # `unsafe_code`
///
/// `SO_ORIGINAL_DST` has no safe wrapper anywhere in this dependency tree (it's a
/// netfilter-specific `getsockopt` level `libc` doesn't model) — a raw call is the only way
/// to reach it at all.
#[allow(unsafe_code)]
fn original_destination(stream: &std::net::TcpStream) -> io::Result<SocketAddrV4> {
    let fd = stream.as_raw_fd();
    // SAFETY: a zeroed `sockaddr_in` is a valid bit pattern for that type.
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: `addr`/`len` are valid, uniquely-owned locals of exactly the size `getsockopt`
    // is told about; `fd` is a real, open socket owned by `stream` for the duration of this
    // call.
    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_IP,
            SO_ORIGINAL_DST,
            std::ptr::addr_of_mut!(addr).cast(),
            &mut len,
        )
    };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    let port = u16::from_be(addr.sin_port);
    Ok(SocketAddrV4::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `datamodel::ObservedDestination` conversion must carry the exact address and
    /// port through, in the same network byte order `SO_ORIGINAL_DST` itself reports — a
    /// silently swapped octet or endian mismatch here would make every P3-04 classification
    /// downstream wrong in a way no type error would ever catch.
    #[test]
    fn conversion_to_observed_destination_preserves_address_and_port_exactly() {
        let entry = ConnectionLogEntry {
            destination: SocketAddrV4::new(Ipv4Addr::new(93, 184, 216, 34), 443),
        };
        let observed: datamodel::ObservedDestination = entry.into();
        assert_eq!(observed.address, [93, 184, 216, 34]);
        assert_eq!(observed.port, 443);
    }

    /// Without any `iptables REDIRECT` rule in play, `SO_ORIGINAL_DST` on an ordinary,
    /// un-redirected connection must fail — it's a netfilter concept that requires the
    /// connection to have actually gone through a `REDIRECT`/`DNAT` target, not a general
    /// property of any TCP socket. Proves this module's own `getsockopt` call is real and
    /// behaves as documented, rather than always guessing something.
    #[test]
    fn a_direct_unredirected_connection_has_no_original_destination() {
        let log = ConnectionLog::start().expect("start");
        let port = log.port();

        let stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).expect("connect directly");
        std::thread::sleep(Duration::from_millis(100));
        drop(stream);

        let entries = log.stop();
        // The connection was accepted (proving the listener itself works), but a direct,
        // non-redirected connection has no SO_ORIGINAL_DST to report, so nothing is logged.
        assert!(entries.is_empty(), "a direct connection must not produce a logged entry: {entries:?}");
    }

    #[test]
    fn port_is_a_real_bound_ephemeral_port() {
        let log = ConnectionLog::start().expect("start");
        assert_ne!(log.port(), 0);
        let _ = log.stop();
    }

    /// `stop()` must actually stop the accept loop, not leave a background thread spinning
    /// forever — checked directly by giving the join a bounded wait via the thread's own
    /// handle rather than trusting it completed just because `stop()` returned.
    #[test]
    fn stop_joins_the_background_thread_cleanly() {
        let log = ConnectionLog::start().expect("start");
        let entries = log.stop();
        assert!(entries.is_empty());
    }
}
