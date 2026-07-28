//! P3-02: a veth pair bridging a network-isolated sandbox (P3-01) back to the host through
//! exactly one, mediated path — plus the `iptables` rule that redirects every outbound TCP
//! connection the sandboxed side ever attempts to a local, host-side listener
//! (`observe::connection_log`, P3-02's own evidence-harvesting half) instead of nowhere.
//!
//! # Why a veth pair, and why this and not a heavier crate
//!
//! `neli` (sync feature only — its `async` feature pulls in `tokio`, which nothing else in
//! this codebase uses, so it's deliberately left off) speaks raw `rtnetlink` — the same
//! protocol `ip link`/`ip addr`/`ip route` themselves use, verified directly against this
//! project's own dev container by hand before writing this module: `neli` doesn't have a
//! typed `VETH_INFO_PEER` constant (only vlan/bridge-style link kinds are covered), so this
//! module encodes it as a raw `Rtattr<u16, _>` with the literal kernel value (`1`, per
//! `<linux/if_link.h>`'s `veth_info` enum) — `neli`'s own generic `Rtattr<T, P>` allows any
//! `T`, so this is a supported escape hatch, not a hack around the crate's own API.
//!
//! # Verified empirically end to end before any production code was written
//!
//! Confirmed by hand, entirely outside this crate, before writing a line of the code below:
//! creating a veth pair, moving one end into another process's network namespace via
//! `IFLA_NET_NS_PID`, addressing and bringing up both ends, adding a default route on the
//! namespaced side, then an `iptables -t nat -A PREROUTING -j REDIRECT` rule on the host
//! side — a real connection attempt from inside the namespace to an arbitrary external
//! address (`8.8.8.8:53`) actually lands on a local listener, whose `SO_ORIGINAL_DST`
//! getsockopt correctly reports the real destination the sandboxed side tried to reach.
//! `REDIRECT` needs no `net.ipv4.ip_forward` at all (confirmed directly) — it makes the
//! packet locally destined rather than routing it further, which is exactly the shape this
//! module needs and nothing more.
//!
//! Also confirmed directly: destroying the process that owns the sandboxed network
//! namespace destroys *both* veth ends automatically, including the host-side one living in
//! the host's own, persistent namespace — the same "kernel tears it down, not this module"
//! guarantee P2-01 already established for PID namespaces. Only the `iptables` rule (which
//! doesn't care whether the interface it names still exists) needs explicit teardown here.
//!
//! # Why this targets `init_pid`, not the real sandboxed target's own PID
//!
//! [`NetworkBridge::set_up`] takes the namespace to bridge into by referencing
//! `/proc/<pid>/ns/net` for a caller-supplied `pid` — callers should pass `spawn`'s own
//! `init_pid` (fork-1, the namespace's long-lived "init"), never the real target's PID
//! (fork-2). Both processes share the identical namespace (fork-2 inherits it unchanged
//! from fork-1's own `unshare`), so either reference resolves to the same kernel namespace
//! object — but fork-2 is not guaranteed to still exist by the time this setup runs (a
//! trivially fast target could already have exited), while fork-1 is guaranteed to survive
//! for the run's entire duration by construction (`supervisor`'s own "init" role). Using the
//! long-lived PID removes the race entirely rather than accepting and documenting it.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::AsFd;
use std::process::Command;

use neli::consts::nl::NlmF;
use neli::consts::rtnl::{Ifa, Ifla, IflaInfo, RtAddrFamily, RtScope, Rta, Rtm, RtTable, Rtn, Rtprot};
use neli::consts::socket::NlFamily;
use neli::err::RouterError;
use neli::nl::NlPayload;
use neli::router::synchronous::NlRouter;
use neli::rtnl::{Ifaddrmsg, IfaddrmsgBuilder, Ifinfomsg, IfinfomsgBuilder, RtattrBuilder, Rtmsg, RtmsgBuilder};
use neli::types::{Buffer, RtBuffer};
use neli::utils::Groups;
use nix::sched::{setns, CloneFlags};
use nix::unistd::Pid;

/// The raw `VETH_INFO_PEER` value from `<linux/if_link.h>`'s `veth_info` enum — `neli` has
/// no typed constant for it (see this module's own doc comment).
const VETH_INFO_PEER: u16 = 1;

/// The host side of the bridge, on the host's own (persistent) network namespace.
const HOST_IP: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 1);
/// The sandboxed side, inside the process-isolated network namespace.
const SANDBOX_IP: Ipv4Addr = Ipv4Addr::new(10, 200, 0, 2);
/// A `/30` — exactly the two addresses above, nothing else routable on this link.
const PREFIX_LEN: u8 = 30;

/// Why setting up (or tearing down) the network bridge failed.
#[derive(Debug)]
pub enum NetnsError {
    /// A netlink request failed.
    Netlink(String),
    /// An underlying I/O operation (opening a `/proc` namespace handle, running `iptables`)
    /// failed.
    Io(io::Error),
    /// `iptables` itself ran but reported a non-zero exit.
    IptablesFailed {
        /// The exact arguments that were passed to `iptables`.
        args: Vec<String>,
        /// The exit status `iptables` reported.
        status: std::process::ExitStatus,
    },
    /// A named interface never showed up in a `Getlink` dump.
    InterfaceNotFound(String),
}

impl std::fmt::Display for NetnsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Netlink(msg) => write!(f, "netlink error: {msg}"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::IptablesFailed { args, status } => {
                write!(f, "iptables {args:?} exited with {status}")
            }
            Self::InterfaceNotFound(name) => write!(f, "interface `{name}` not found"),
        }
    }
}

impl std::error::Error for NetnsError {}

impl From<io::Error> for NetnsError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl<T: std::fmt::Debug, P: std::fmt::Debug> From<RouterError<T, P>> for NetnsError {
    fn from(e: RouterError<T, P>) -> Self {
        Self::Netlink(format!("{e:?}"))
    }
}

/// A veth pair bridging one sandboxed network namespace to the host, plus the `iptables`
/// redirect rule that routes every connection attempt on it to a local proxy port.
///
/// The veth pair itself needs no explicit teardown (see this module's own doc comment); only
/// [`Self::teardown`]'s `iptables` rule removal is real cleanup work.
pub struct NetworkBridge {
    host_ifname: String,
    proxy_port: u16,
}

/// Set up a veth pair into the network namespace `init_pid` belongs to, address and route
/// both ends, and redirect every TCP connection the sandboxed side attempts to
/// `<host_ifname's own address>:proxy_port` on the host (that is what `iptables REDIRECT`
/// rewrites the destination to for traffic arriving on `host_ifname` — never `127.0.0.1`,
/// which only applies to locally-generated packets) — a local listener
/// (`observe::connection_log`) is expected to already be bound on every interface (`0.0.0.0`)
/// before this is called, so no connection attempt is ever refused for lack of a listener.
impl NetworkBridge {
    /// Interface name length budget: `IFNAMSIZ` is 16 bytes including the trailing NUL, so
    /// 15 usable bytes. `"vh"`/`"vs"` (2 bytes) plus a PID (Linux's own default
    /// `pid_max` is 4194304, 7 digits) comfortably fits without needing to truncate.
    fn ifnames(pid: Pid) -> (String, String) {
        (format!("vh{}", pid.as_raw()), format!("vs{}", pid.as_raw()))
    }

    /// # Errors
    /// Any netlink or `iptables` step failing. Nothing here is retried — a caller that gets
    /// an error should treat the whole bridge as not set up at all.
    pub fn set_up(init_pid: Pid, proxy_port: u16) -> Result<Self, NetnsError> {
        let (host_ifname, sandbox_ifname) = Self::ifnames(init_pid);

        create_veth_pair(&host_ifname, &sandbox_ifname, init_pid)?;
        configure_host_side(&host_ifname)?;
        configure_sandbox_side(init_pid, &sandbox_ifname)?;
        add_redirect_rule(&host_ifname, proxy_port)?;

        Ok(Self { host_ifname, proxy_port })
    }

    /// Remove the `iptables` redirect rule. The veth pair itself is expected to already be
    /// gone by the time this runs (torn down automatically when the sandboxed namespace
    /// was destroyed) — this only cleans up the one thing that genuinely outlives it.
    pub fn teardown(self) -> Result<(), NetnsError> {
        remove_redirect_rule(&self.host_ifname, self.proxy_port)
    }
}

fn connect_route_socket() -> Result<NlRouter, NetnsError> {
    let (router, _) = NlRouter::connect(NlFamily::Route, None, Groups::empty())
        .map_err(|e| NetnsError::Netlink(format!("connect: {e}")))?;
    router.enable_ext_ack(true).map_err(|e| NetnsError::Netlink(format!("enable_ext_ack: {e}")))?;
    Ok(router)
}

fn get_link_index(router: &NlRouter, name: &str) -> Result<libc::c_int, NetnsError> {
    let recv = router
        .send::<Rtm, Ifinfomsg, Rtm, Ifinfomsg>(
            Rtm::Getlink,
            NlmF::ROOT,
            NlPayload::<Rtm, Ifinfomsg>::Payload(
                IfinfomsgBuilder::default()
                    .ifi_family(RtAddrFamily::Netlink)
                    .build()
                    .map_err(|e| NetnsError::Netlink(e.to_string()))?,
            ),
        )
        .map_err(|e| NetnsError::Netlink(format!("getlink: {e}")))?;

    // Drain the *entire* dump rather than returning as soon as a match is found: dropping the
    // receiver handle mid-dump deregisters its sequence number immediately
    // (`NlRouterReceiverHandle::drop`), but the kernel may still have further messages for that
    // same dump (other interfaces, the trailing `NLMSG_DONE`) in flight. Those stragglers then
    // arrive with no sender registered for their sequence number, and neli's router broadcasts
    // that as a `BadSeqOrPid` error to *every* currently-pending request on this router —
    // observed directly as a `Newaddr` request failing with a corrupted `NLMSG_DONE` (`nl_type
    // = 3`) left over from an earlier, early-exited `Getlink` dump on the same connection.
    let mut found = None;
    for response in recv {
        let header = response.map_err(|e| NetnsError::Netlink(format!("getlink response: {e}")))?;
        if let NlPayload::Payload(if_info) = header.nl_payload() {
            let matches = if_info
                .rtattrs()
                .get_attr_handle()
                .get_attr_payload_as_with_len_borrowed::<&str>(Ifla::Ifname)
                .map(|n| n.trim_end_matches('\0') == name)
                .unwrap_or_default();
            if matches {
                found = Some(*if_info.ifi_index());
            }
        }
    }
    found.ok_or_else(|| NetnsError::InterfaceNotFound(name.to_string()))
}

fn create_veth_pair(host_ifname: &str, sandbox_ifname: &str, target_pid: Pid) -> Result<(), NetnsError> {
    let router = connect_route_socket()?;

    let mut peer_attrs = RtBuffer::<Ifla, Buffer>::new();
    peer_attrs.push(
        RtattrBuilder::default()
            .rta_type(Ifla::Ifname)
            .rta_payload(format!("{sandbox_ifname}\0"))
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );
    peer_attrs.push(
        RtattrBuilder::default()
            .rta_type(Ifla::NetNsPid)
            .rta_payload(target_pid.as_raw() as u32)
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );
    let peer_ifinfomsg = IfinfomsgBuilder::default()
        .ifi_family(RtAddrFamily::Netlink)
        .rtattrs(peer_attrs)
        .build()
        .map_err(|e| NetnsError::Netlink(e.to_string()))?;

    let mut veth_data = RtBuffer::<u16, Buffer>::new();
    veth_data.push(
        RtattrBuilder::default()
            .rta_type(VETH_INFO_PEER)
            .rta_payload(peer_ifinfomsg)
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );

    let mut info_attrs = RtBuffer::<IflaInfo, Buffer>::new();
    info_attrs.push(
        RtattrBuilder::default()
            .rta_type(IflaInfo::Kind)
            .rta_payload("veth\0")
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );
    info_attrs.push(
        RtattrBuilder::default()
            .rta_type(IflaInfo::Data)
            .rta_payload(veth_data)
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );

    let mut attrs = RtBuffer::<Ifla, Buffer>::new();
    attrs.push(
        RtattrBuilder::default()
            .rta_type(Ifla::Ifname)
            .rta_payload(format!("{host_ifname}\0"))
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );
    attrs.push(
        RtattrBuilder::default()
            .rta_type(Ifla::Linkinfo)
            .rta_payload(info_attrs)
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );

    let ifinfomsg = IfinfomsgBuilder::default()
        .ifi_family(RtAddrFamily::Netlink)
        .rtattrs(attrs)
        .build()
        .map_err(|e| NetnsError::Netlink(e.to_string()))?;

    let recv = router
        .send::<_, _, Rtm, Ifinfomsg>(Rtm::Newlink, NlmF::CREATE | NlmF::EXCL | NlmF::ACK, NlPayload::Payload(ifinfomsg))
        .map_err(|e| NetnsError::Netlink(format!("newlink: {e}")))?;
    for response in recv {
        let header = response.map_err(|e| NetnsError::Netlink(format!("newlink response: {e}")))?;
        if let NlPayload::Err(e) = header.nl_payload() {
            return Err(NetnsError::Netlink(format!("newlink: {e:?}")));
        }
    }
    Ok(())
}

fn assign_address_and_bring_up(router: &NlRouter, ifname: &str, ip: Ipv4Addr) -> Result<(), NetnsError> {
    let index = get_link_index(router, ifname)?;

    let mut addr_attrs = RtBuffer::<Ifa, Buffer>::new();
    let ip_be = u32::from(ip).to_be();
    addr_attrs.push(
        RtattrBuilder::default().rta_type(Ifa::Local).rta_payload(ip_be).build().map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );
    addr_attrs.push(
        RtattrBuilder::default().rta_type(Ifa::Address).rta_payload(ip_be).build().map_err(|e| NetnsError::Netlink(e.to_string()))?,
    );
    let ifaddrmsg: Ifaddrmsg = IfaddrmsgBuilder::default()
        .ifa_family(RtAddrFamily::Inet)
        .ifa_prefixlen(PREFIX_LEN)
        .ifa_scope(RtScope::Universe)
        .ifa_index(index as u32)
        .rtattrs(addr_attrs)
        .build()
        .map_err(|e| NetnsError::Netlink(e.to_string()))?;
    let recv = router
        .send::<_, _, Rtm, Ifaddrmsg>(Rtm::Newaddr, NlmF::CREATE | NlmF::ACK, NlPayload::Payload(ifaddrmsg))
        .map_err(|e| NetnsError::Netlink(format!("newaddr: {e}")))?;
    for response in recv {
        let header = response.map_err(|e| NetnsError::Netlink(format!("newaddr response: {e}")))?;
        if let NlPayload::Err(e) = header.nl_payload() {
            return Err(NetnsError::Netlink(format!("newaddr: {e:?}")));
        }
    }

    let up_msg = IfinfomsgBuilder::default()
        .ifi_family(RtAddrFamily::Netlink)
        .ifi_index(index)
        .up()
        .build()
        .map_err(|e| NetnsError::Netlink(e.to_string()))?;
    let recv = router
        .send::<_, _, Rtm, Ifinfomsg>(Rtm::Setlink, NlmF::ACK, NlPayload::Payload(up_msg))
        .map_err(|e| NetnsError::Netlink(format!("setlink up: {e}")))?;
    for response in recv {
        let header = response.map_err(|e| NetnsError::Netlink(format!("setlink up response: {e}")))?;
        if let NlPayload::Err(e) = header.nl_payload() {
            return Err(NetnsError::Netlink(format!("setlink up: {e:?}")));
        }
    }
    Ok(())
}

fn configure_host_side(host_ifname: &str) -> Result<(), NetnsError> {
    let router = connect_route_socket()?;
    assign_address_and_bring_up(&router, host_ifname, HOST_IP)
}

/// Runs entirely on a dedicated OS thread: `setns` changes only the *calling thread's* own
/// namespace membership, never the whole process, so a short-lived thread that joins the
/// target namespace, does its setup, and exits is the correct and only safe way to touch an
/// interface that already lives inside another network namespace from a process that must
/// otherwise stay in its own.
fn configure_sandbox_side(init_pid: Pid, sandbox_ifname: &str) -> Result<(), NetnsError> {
    let sandbox_ifname = sandbox_ifname.to_string();
    std::thread::spawn(move || -> Result<(), NetnsError> {
        let ns_path = format!("/proc/{}/ns/net", init_pid.as_raw());
        let ns_file = std::fs::File::open(&ns_path)?;
        setns(ns_file.as_fd(), CloneFlags::CLONE_NEWNET)
            .map_err(|e| NetnsError::Io(io::Error::from(e)))?;

        // A fresh connection, opened only *after* `setns` — it must be scoped to the target
        // namespace, not whatever namespace this thread happened to start in.
        let router = connect_route_socket()?;
        assign_address_and_bring_up(&router, &sandbox_ifname, SANDBOX_IP)?;
        assign_address_and_bring_up(&router, "lo", Ipv4Addr::LOCALHOST)?;

        let mut rt_attrs = RtBuffer::<Rta, Buffer>::new();
        let gw_be = u32::from(HOST_IP).to_be();
        rt_attrs.push(
            RtattrBuilder::default().rta_type(Rta::Gateway).rta_payload(gw_be).build().map_err(|e| NetnsError::Netlink(e.to_string()))?,
        );
        let oif = get_link_index(&router, &sandbox_ifname)?;
        rt_attrs.push(
            RtattrBuilder::default().rta_type(Rta::Oif).rta_payload(oif as u32).build().map_err(|e| NetnsError::Netlink(e.to_string()))?,
        );
        let rtmsg: Rtmsg = RtmsgBuilder::default()
            .rtm_family(RtAddrFamily::Inet)
            .rtm_dst_len(0)
            .rtm_src_len(0)
            .rtm_tos(0)
            .rtm_table(RtTable::Main)
            .rtm_protocol(Rtprot::Boot)
            .rtm_scope(RtScope::Universe)
            .rtm_type(Rtn::Unicast)
            .rtattrs(rt_attrs)
            .build()
            .map_err(|e| NetnsError::Netlink(e.to_string()))?;
        let recv = router
            .send::<_, _, Rtm, Rtmsg>(Rtm::Newroute, NlmF::CREATE | NlmF::ACK, NlPayload::Payload(rtmsg))
            .map_err(|e| NetnsError::Netlink(format!("newroute: {e}")))?;
        for response in recv {
            let header = response.map_err(|e| NetnsError::Netlink(format!("newroute response: {e}")))?;
            if let NlPayload::Err(e) = header.nl_payload() {
                return Err(NetnsError::Netlink(format!("newroute: {e:?}")));
            }
        }
        Ok(())
    })
    .join()
    .unwrap_or_else(|panic| {
        Err(NetnsError::Netlink(format!("sandbox-side setup thread panicked: {panic:?}")))
    })
}

fn add_redirect_rule(host_ifname: &str, proxy_port: u16) -> Result<(), NetnsError> {
    run_iptables(&[
        "-t", "nat", "-A", "PREROUTING", "-i", host_ifname, "-p", "tcp", "-j", "REDIRECT",
        "--to-port", &proxy_port.to_string(),
    ])
}

fn remove_redirect_rule(host_ifname: &str, proxy_port: u16) -> Result<(), NetnsError> {
    run_iptables(&[
        "-t", "nat", "-D", "PREROUTING", "-i", host_ifname, "-p", "tcp", "-j", "REDIRECT",
        "--to-port", &proxy_port.to_string(),
    ])
}

fn run_iptables(args: &[&str]) -> Result<(), NetnsError> {
    let status = Command::new("iptables").args(args).status()?;
    if !status.success() {
        return Err(NetnsError::IptablesFailed {
            args: args.iter().map(|s| s.to_string()).collect(),
            status,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{spawn, OverlaySpec, SandboxSpec};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::time::Duration;

    fn build_trivial_lower(root: &std::path::Path) {
        crate::build(root, &[]).expect("build base layer");
    }

    /// P3-02's own literal exit criterion, at the plumbing layer this crate owns: a real
    /// connection attempt from inside a network-isolated sandbox, once bridged, actually
    /// reaches a listener on the host — proving the veth pair, addressing, routing, and
    /// `iptables REDIRECT` rule all work together, not just individually. Retrieving the
    /// *real* destination via `SO_ORIGINAL_DST` is `observe::connection_log`'s own job and
    /// its own test (this crate has no reason to depend on `observe`); this test only proves
    /// this crate's own half — that the connection lands here at all.
    #[test]
    fn a_bridged_sandboxed_connection_reaches_the_host_side_listener() {
        let lower_dir = tempfile::tempdir().expect("tempdir");
        build_trivial_lower(lower_dir.path());
        let scratch = tempfile::tempdir().expect("tempdir");

        // Stdin-gated: the script waits for a line before attempting its connection, so the
        // bridge is guaranteed fully set up (veth, addresses, route, iptables rule) before
        // the sandboxed side ever tries to use it — the same discipline P2-02's own cgroup
        // tests already established for "don't race the setup you're about to depend on."
        let script = "\
import socket, sys
sys.stdin.readline()
try:
    socket.create_connection(('93.184.216.34', 80), timeout=5)
    print('CONNECT: succeeded')
except OSError as e:
    print('CONNECT:', e)
";
        let spec = SandboxSpec {
            overlay: OverlaySpec {
                lower: lower_dir.path().to_path_buf(),
                upper: scratch.path().join("upper"),
                work: scratch.path().join("work"),
                mountpoint: scratch.path().join("merged"),
            },
            program: PathBuf::from("/usr/local/bin/python3"),
            args: vec!["-c".to_string(), script.to_string()],
            timeout: Duration::from_secs(10),
            network_isolated: true,
        };

        let (handle, mut stdin, mut stdout) = spawn(&spec).expect("spawn");

        let listener = TcpListener::bind(("0.0.0.0", 0)).expect("bind proxy listener");
        let proxy_port = listener.local_addr().expect("local_addr").port();

        let bridge = NetworkBridge::set_up(handle.init_pid(), proxy_port).expect("set up bridge");

        writeln!(stdin, "go").expect("release the sandboxed script");
        drop(stdin);

        listener.set_nonblocking(false).expect("blocking accept");
        listener
            .set_ttl(64)
            .ok(); // no-op; keeps `listener` unambiguously used before the blocking accept below
        let accept_result = {
            // Bound wait: `accept()` itself has no timeout, so give it one via a short-lived
            // thread rather than blocking this test forever if the redirect never arrives.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(listener.accept());
            });
            rx.recv_timeout(Duration::from_secs(5))
        };

        let mut output = String::new();
        stdout.read_to_string(&mut output).expect("read sandboxed stdout");
        let outcome = handle.wait().expect("wait");
        bridge.teardown().expect("teardown bridge");

        assert!(!outcome.timed_out, "sandboxed output: {output}");
        assert!(
            accept_result.is_ok(),
            "the bridge must deliver a real connection attempt to the host listener \
             within 5s; sandboxed output was: {output}"
        );
        assert!(
            accept_result.unwrap().is_ok(),
            "accept() itself must succeed once a connection arrives"
        );
        assert!(
            output.contains("CONNECT: succeeded"),
            "from the sandboxed side, the redirected connection must appear to succeed \
             (the whole point of `REDIRECT` over a hard rejection): {output}"
        );
    }
}
