//! Host resource detection (macOS `sysctl`) and capacity validation.
//!
//! `validate_capacity` is a pure function over `HostResources` so it is unit
//! tested without touching the machine; `detect_host` is the side-effecting edge.

use std::net::{ToSocketAddrs, UdpSocket};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::memory::Memory;

/// The physical resources of the machine the worker runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostResources {
    pub cpu: u32,
    pub memory_bytes: u64,
}

/// Read host capacity from macOS `sysctl` (`hw.logicalcpu`, `hw.memsize`).
pub fn detect_host() -> Result<HostResources> {
    let cpu = sysctl_u64("hw.logicalcpu")?;
    let memory_bytes = sysctl_u64("hw.memsize")?;
    Ok(HostResources {
        cpu: u32::try_from(cpu).unwrap_or(u32::MAX),
        memory_bytes,
    })
}

fn sysctl_u64(key: &str) -> Result<u64> {
    let text = sysctl_string(key)?;
    text.parse::<u64>()
        .with_context(|| format!("parsing sysctl {key} output {text:?}"))
}

fn sysctl_string(key: &str) -> Result<String> {
    let out = Command::new("sysctl")
        .args(["-n", key])
        .output()
        .with_context(|| format!("running sysctl -n {key}"))?;
    if !out.status.success() {
        bail!("sysctl -n {key} failed");
    }
    let text =
        String::from_utf8(out.stdout).with_context(|| format!("sysctl {key} output not UTF-8"))?;
    Ok(text.trim().to_string())
}

/// Identifying facts about the worker's OS and agent build, reported at
/// registration for fleet visibility (agent version, OS, arch, hostname).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemInfo {
    pub agent_version: String,
    pub os: String,
    pub arch: String,
    pub hostname: String,
}

/// Collect host system info for registration. Best-effort (Principle #6 applies
/// to *auth*, not cosmetics): a field that cannot be read falls back to a
/// placeholder rather than aborting a worker's registration.
pub fn detect_system_info() -> SystemInfo {
    let os = match sysctl_string("kern.osproductversion") {
        Ok(v) => format!("macOS {v}"),
        Err(_) => "macOS".to_string(),
    };
    SystemInfo {
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        os,
        arch: sysctl_string("hw.machine").unwrap_or_else(|_| "unknown".to_string()),
        hostname: sysctl_string("kern.hostname").unwrap_or_else(|_| "unknown".to_string()),
    }
}

/// Reject capacity that exceeds the physical host or is degenerate. Fail closed.
pub fn validate_capacity(cpu: u32, memory: Memory, host: HostResources) -> Result<()> {
    if cpu == 0 {
        bail!("cpu must be at least 1");
    }
    if cpu > host.cpu {
        bail!("requested {cpu} cores but machine has {}", host.cpu);
    }
    let want = memory.bytes();
    if want == 0 {
        bail!("memory must be greater than 0");
    }
    if want > host.memory_bytes {
        bail!(
            "requested {} memory but machine has {}",
            memory,
            Memory::from_bytes(host.memory_bytes)
        );
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(test, allow(clippy::unwrap_used))]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn host() -> HostResources {
        HostResources {
            cpu: 8,
            memory_bytes: 16 * GB,
        }
    }

    #[test]
    fn accepts_capacity_within_host() {
        assert!(validate_capacity(8, Memory::from_bytes(16 * GB), host()).is_ok());
        assert!(validate_capacity(1, Memory::from_bytes(GB), host()).is_ok());
    }

    #[test]
    fn rejects_too_many_cores() {
        assert!(validate_capacity(9, Memory::from_bytes(GB), host()).is_err());
    }

    #[test]
    fn rejects_too_much_memory() {
        assert!(validate_capacity(1, Memory::from_bytes(32 * GB), host()).is_err());
    }

    #[test]
    fn rejects_zero() {
        assert!(validate_capacity(0, Memory::from_bytes(GB), host()).is_err());
        assert!(validate_capacity(1, Memory::from_bytes(0), host()).is_err());
    }
}

/// The address other machines should use to reach this worker.
///
/// Found by asking the kernel which local address it would use to reach the
/// control plane, via a UDP socket that is connected but never sent on. That is
/// the right answer rather than a convenient one: a Mac routinely has several
/// addresses (Wi-Fi, Ethernet, VPN, Thunderbolt bridges), and the one that can
/// reach the server is the one an ingress on the server's network can reach
/// back. Picking the "first" interface instead would publish an endpoint that
/// happens to work on one worker and black-holes on the next.
///
/// A loopback answer is published as-is rather than suppressed: it means the
/// control plane is on this same machine, and for an all-in-one-box install
/// `127.0.0.1` is genuinely where this worker's services answer. Suppressing it
/// would leave that install with no endpoints and no explanation.
///
/// `None` when the server URL cannot be parsed or no route exists. The worker
/// then registers with no address and the endpoints controller says so, which
/// beats advertising a guess.
pub fn detect_address(server: &str) -> Option<String> {
    let hostport = server_host_port(server)?;
    let target = hostport.to_socket_addrs().ok()?.next()?;
    let sock = UdpSocket::bind(if target.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    })
    .ok()?;
    // UDP connect only fixes the peer and picks a route; no packet is sent, so
    // this neither reaches the server nor needs it to be up.
    sock.connect(target).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    // `0.0.0.0` is a bind wildcard, never an address anything can reach.
    if ip.is_unspecified() {
        return None;
    }
    Some(ip.to_string())
}

/// Split `http://host:port/path` into `host:port`, defaulting the port by
/// scheme. Deliberately tiny: the value comes from this worker's own config,
/// which `veloslet setup` wrote, not from the network.
fn server_host_port(server: &str) -> Option<String> {
    let (scheme, rest) = server.split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    if authority.is_empty() {
        return None;
    }
    if authority.contains(':') {
        return Some(authority.to_string());
    }
    let port = match scheme {
        "https" => 443,
        _ => 80,
    };
    Some(format!("{authority}:{port}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod address_tests {
    use super::*;

    #[test]
    fn server_url_splits_into_host_and_port() {
        assert_eq!(
            server_host_port("http://192.168.68.60:8088"),
            Some("192.168.68.60:8088".to_string())
        );
        assert_eq!(
            server_host_port("http://velos.lan/api"),
            Some("velos.lan:80".to_string())
        );
        assert_eq!(
            server_host_port("https://velos.example.com"),
            Some("velos.example.com:443".to_string())
        );
        assert_eq!(server_host_port("velos.lan:8088"), None, "scheme required");
    }

    #[test]
    fn the_address_is_the_one_that_reaches_that_server() {
        // A server on loopback is reached over loopback, and that is a real
        // answer for an all-in-one-box install, not a value to suppress.
        assert_eq!(
            detect_address("http://127.0.0.1:8088"),
            Some("127.0.0.1".to_string())
        );
        // A server off-box is reached over the default route instead.
        // TEST-NET-1 is never routed on a real network, but the kernel still
        // picks the interface it would leave by, which is the point. Skipped on
        // a machine with no route at all (an offline CI runner).
        if let Some(addr) = detect_address("http://192.0.2.1:9") {
            assert!(
                !addr.starts_with("127."),
                "published a loopback address for an off-box server"
            );
        }
    }
}
