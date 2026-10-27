//! Distro profiles — the knowledge layer of `mcp-qemu-ephemeral`.
//!
//! This crate answers the question *an agent should never have to guess*:
//! "how do I boot and log into distro X on architecture Y?" Each
//! [`DistroProfile`] carries everything needed to turn a distro name into a
//! bootable VM:
//!
//! - the ISO download URL (per architecture),
//! - the login credentials used at the serial console,
//! - the guest shell to use for script execution,
//! - architecture-specific QEMU defaults (binary, machine model).
//!
//! The registry is a small built-in table; adding a distro means adding one
//! entry (plus, optionally, variant/arch rows). Everything else in the system
//! (image fetch, the runner, the MCP tools) reads from here.

use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Guest CPU architecture supported by the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    /// x86-64 (AMD64 / Intel 64).
    #[serde(rename = "x86_64")]
    X86_64,
    /// ARM 64-bit (AArch64).
    #[serde(rename = "aarch64")]
    Aarch64,
}

impl Arch {
    /// Architecture name as used by distro mirrors and `uname -m`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// The QEMU binary that emulates this architecture.
    pub fn qemu_binary(&self) -> &'static str {
        match self {
            Arch::X86_64 => "qemu-system-x86_64",
            Arch::Aarch64 => "qemu-system-aarch64",
        }
    }

    /// Machine-model arguments QEMU needs for this architecture.
    ///
    /// x86-64 needs none (QEMU defaults to a sane PC machine). AArch64
    /// requires an explicit `-M virt` machine plus a CPU model.
    pub fn qemu_machine_args(&self) -> Vec<&'static str> {
        match self {
            Arch::X86_64 => Vec::new(),
            Arch::Aarch64 => vec!["-M", "virt", "-cpu", "cortex-a57"],
        }
    }

    /// All architectures, for listing tools.
    pub fn all() -> [Arch; 2] {
        [Arch::X86_64, Arch::Aarch64]
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Arch {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "x86_64" | "amd64" | "x64" => Ok(Arch::X86_64),
            "aarch64" | "arm64" | "arm" => Ok(Arch::Aarch64),
            other => Err(format!(
                "unknown architecture \"{other}\" (expected x86_64/amd64 or aarch64/arm64)"
            )),
        }
    }
}

/// Serial-console login credentials for a distro profile.
///
/// An empty `password` means the guest accepts an enter-key login (no
/// password), which is how Alpine live ISOs authenticate `root`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    /// Username sent when the console shows a `login:` prompt.
    pub username: &'static str,
    /// Password sent when the console shows a `Password:` prompt.
    /// Empty string = press Enter (no password).
    pub password: &'static str,
}

/// A distro profile: everything needed to boot, log into, and run scripts
/// on one distro/variant/architecture combination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistroProfile {
    /// Distro short name (e.g. `alpine`).
    pub distro: &'static str,
    /// ISO variant (e.g. `virt` — tuned for VMs, serial-console friendly).
    pub variant: &'static str,
    /// Distro release version.
    pub version: &'static str,
    /// Guest architecture.
    pub arch: Arch,
    /// Full URL of the bootable ISO on the distro's mirror.
    pub iso_url: &'static str,
    /// Serial-console login credentials.
    pub login: Login,
    /// Shell to use when running scripts inside the guest.
    pub shell: &'static str,
    /// Package manager inside the guest (if one is reachable on the ISO).
    pub package_manager: Option<&'static str>,
    /// Boot-loader prompt marker to watch for on the serial console
    /// (e.g. ISOLINUX `boot:`) — when seen, `boot_response` is sent.
    pub boot_prompt: Option<&'static str>,
    /// Line to send at the boot-loader prompt (kernel + initrd + serial
    /// console kernel argument). Only used together with `boot_prompt`.
    pub boot_response: Option<&'static str>,
    /// Free-form notes for agents: quirks, network setup, prompt format.
    pub notes: &'static str,
}

impl DistroProfile {
    /// File name of the ISO (last URL segment).
    pub fn iso_filename(&self) -> &str {
        self.iso_url.rsplit('/').next().unwrap_or(self.iso_url)
    }
}

/// The built-in profile registry.
///
/// Organized per distro/variant/arch so each combination can evolve
/// independently (a future aarch64 firmware-aware flow only touches its own
/// rows).
pub fn all() -> &'static [DistroProfile] {
    &ALPINE_PROFILES
}

const ALPINE_NOTES: &str = "Live ISO: boots by default to 'localhost login:' on the \
serial console (getty listens on ttyS0; no boot-prompt answer needed, but TCG \
boot without KVM can take 1-4 minutes — keep the boot timeout generous). root \
has NO password (Enter-key login). Scripts run under /bin/sh (busybox ash, not \
bash). Networking is DOWN by default: run `setup-interfaces -a -r` (or \
`udhcpc -i eth0`) before anything network-related.";

/// Alpine Linux 3.22 (standard + virt variants, x86_64 + aarch64).
static ALPINE_PROFILES: [DistroProfile; 4] = [
    DistroProfile {
        distro: "alpine",
        variant: "virt",
        version: "3.22.7",
        arch: Arch::X86_64,
        iso_url: "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-virt-3.22.7-x86_64.iso",
        login: Login { username: "root", password: "" },
        shell: "/bin/sh",
        package_manager: Some("apk"),
        boot_prompt: None,
        boot_response: None,
        notes: ALPINE_NOTES,
    },
    DistroProfile {
        distro: "alpine",
        variant: "standard",
        version: "3.22.7",
        arch: Arch::X86_64,
        iso_url: "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-standard-3.22.7-x86_64.iso",
        login: Login { username: "root", password: "" },
        shell: "/bin/sh",
        package_manager: Some("apk"),
        boot_prompt: None,
        boot_response: None,
        notes: ALPINE_NOTES,
    },
    DistroProfile {
        distro: "alpine",
        variant: "virt",
        version: "3.22.7",
        arch: Arch::Aarch64,
        iso_url: "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/aarch64/alpine-virt-3.22.7-aarch64.iso",
        login: Login { username: "root", password: "" },
        shell: "/bin/sh",
        package_manager: Some("apk"),
        boot_prompt: None,
        boot_response: None,
        notes: "aarch64 ISOs boot via UEFI only: QEMU needs aarch64 UEFI \
firmware (e.g. QEMU_EFI.fd/edk2-aarch64-code.fd); the runner probes standard \
firmware paths and reports clearly when none is installed. Boot-loader \
serial-console automation is not yet wired for aarch64 (grub-over-UEFI).",
    },
    DistroProfile {
        distro: "alpine",
        variant: "standard",
        version: "3.22.7",
        arch: Arch::Aarch64,
        iso_url: "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/aarch64/alpine-standard-3.22.7-aarch64.iso",
        login: Login { username: "root", password: "" },
        shell: "/bin/sh",
        package_manager: Some("apk"),
        boot_prompt: None,
        boot_response: None,
        notes: "aarch64 ISOs boot via UEFI only: QEMU needs aarch64 UEFI \
firmware (e.g. QEMU_EFI.fd/edk2-aarch64-code.fd); the runner probes standard \
firmware paths and reports clearly when none is installed. Boot-loader \
serial-console automation is not yet wired for aarch64 (grub-over-UEFI).",
    },
];

/// Look up a profile by distro, optional variant, and architecture.
///
/// When `variant` is `None`, the first matching profile for the distro/arch
/// wins (the `virt` variant is listed first: it is the recommended one for
/// headless serial-console work).
pub fn lookup(distro: &str, variant: Option<&str>, arch: Arch) -> Option<&'static DistroProfile> {
    let distro_lower = distro.to_lowercase();
    all().iter().find(|p| {
        p.distro == distro_lower
            && p.arch == arch
            && variant.map_or(true, |v| p.variant == v.to_lowercase())
    })
}

/// Unique distro names in the registry.
pub fn list_distros() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for p in all() {
        if !names.contains(&p.distro) {
            names.push(p.distro);
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arch_from_str() {
        assert_eq!(Arch::from_str("x86_64"), Ok(Arch::X86_64));
        assert_eq!(Arch::from_str("amd64"), Ok(Arch::X86_64));
        assert_eq!(Arch::from_str("aarch64"), Ok(Arch::Aarch64));
        assert_eq!(Arch::from_str("arm64"), Ok(Arch::Aarch64));
        assert_eq!(Arch::from_str("ARM"), Ok(Arch::Aarch64));
        assert!(Arch::from_str("mips").is_err());
    }

    #[test]
    fn test_lookup_alpine() {
        let p = lookup("alpine", None, Arch::X86_64).expect("alpine x86_64");
        assert_eq!(p.variant, "virt");
        assert_eq!(p.login.username, "root");
        assert_eq!(p.login.password, "");

        let p = lookup("ALPINE", Some("standard"), Arch::Aarch64).expect("alpine aarch64");
        assert_eq!(p.arch, Arch::Aarch64);
        assert!(p.iso_url.contains("/aarch64/"));

        assert!(lookup("alpine", Some("nope"), Arch::X86_64).is_none());
        assert!(lookup("ubuntu", None, Arch::X86_64).is_none());
    }

    #[test]
    fn test_lookup_all_variants_exist() {
        for arch in Arch::all() {
            for variant in ["virt", "standard"] {
                assert!(
                    lookup("alpine", Some(variant), arch).is_some(),
                    "missing alpine {variant} on {arch}"
                );
            }
        }
    }

    #[test]
    fn test_iso_filename() {
        let p = lookup("alpine", None, Arch::X86_64).unwrap();
        assert_eq!(p.iso_filename(), "alpine-virt-3.22.7-x86_64.iso");
    }

    #[test]
    fn test_qemu_binaries() {
        assert_eq!(Arch::X86_64.qemu_binary(), "qemu-system-x86_64");
        assert_eq!(Arch::Aarch64.qemu_binary(), "qemu-system-aarch64");
        assert!(Arch::X86_64.qemu_machine_args().is_empty());
        assert_eq!(
            Arch::Aarch64.qemu_machine_args(),
            vec!["-M", "virt", "-cpu", "cortex-a57"]
        );
    }

    #[test]
    fn test_list_distros() {
        let distros = list_distros();
        assert!(distros.contains(&"alpine"));
        assert_eq!(distros.len(), 1);
    }
}
