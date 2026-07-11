//! Host resource detection (macOS `sysctl`) and capacity validation.
//!
//! `validate_capacity` is a pure function over `HostResources` so it is unit
//! tested without touching the machine; `detect_host` is the side-effecting edge.

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
