//! Claude Code software identity (helps/claude_device_profile.go).
//!
//! The measured CLI release lives in exactly one place: [`BASELINE`]. Anthropic
//! rejected the older 2.1.220 identity on 2026-09-12 and required 2.1.251 or newer;
//! bumping the CLI version is a one-line change to `BASELINE.user_agent` plus the
//! matching Stainless/runtime values when a fresh native capture shows they moved.
//! The billing `cc_version` and the native-client version floor derive from it.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use http::HeaderMap;
use sha2::{Digest, Sha256};

use super::detect::{NATIVE_ENTRYPOINTS, header, native_user_agent, user_agent_details};
use super::settings::Settings;

/// Claude Code 2.1.280 / @anthropic-ai/sdk 0.112.1 on Node v26.3.0, macOS arm64.
pub(crate) const BASELINE: Baseline = Baseline {
    user_agent: "claude-cli/2.1.280 (external, cli)",
    package_version: "0.112.1",
    runtime_version: "v26.3.0",
    os: "MacOS",
    arch: "arm64",
};

/// Stabilized profiles live for seven days after last use.
// ponytail: Home KV mode (shared profiles with a 5 s write lock) is not ported. The
// seam is [`resolve`]: Go's resolveClaudeDeviceProfile calls
// resolveClaudeDeviceProfileHome instead when a Home KV client is current
// (cpa_home::kv::current_client), and this local resolver otherwise.
pub(crate) const PROFILE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

pub(crate) struct Baseline {
    pub user_agent: &'static str,
    pub package_version: &'static str,
    pub runtime_version: &'static str,
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Profile {
    pub user_agent: String,
    pub package_version: String,
    pub runtime_version: String,
    pub os: String,
    pub arch: String,
}

pub(crate) type Version = (u64, u64, u64);

/// `^claude-cli/(\d+)\.(\d+)\.(\d+)` on the trimmed User-Agent.
pub(crate) fn version(user_agent: &str) -> Option<Version> {
    let rest = user_agent.trim().strip_prefix("claude-cli/")?;
    let mut parts = rest.splitn(3, '.');
    let major = digits(parts.next()?)?;
    let minor = digits(parts.next()?)?;
    let tail = parts.next()?;
    let end = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
    Some((major, minor, digits(&tail[..end])?))
}

fn digits(s: &str) -> Option<u64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok())?
}

impl Profile {
    /// `defaultClaudeDeviceProfile`: configured header defaults over the baseline.
    pub fn default_for(settings: &Settings) -> Self {
        let pick = |configured: &str, fallback: &str| {
            let configured = configured.trim();
            if configured.is_empty() { fallback } else { configured }.to_owned()
        };
        let h = &settings.header_defaults;
        Self {
            user_agent: pick(&h.user_agent, BASELINE.user_agent),
            package_version: pick(&h.package_version, BASELINE.package_version),
            runtime_version: pick(&h.runtime_version, BASELINE.runtime_version),
            os: pick(&h.os, BASELINE.os),
            arch: pick(&h.arch, BASELINE.arch),
        }
    }

    fn version(&self) -> Option<Version> {
        version(&self.user_agent)
    }

    /// Exact software tuple of the measured baseline.
    fn meets(&self, baseline: &Profile) -> bool {
        matches!((self.version(), baseline.version()), (Some(a), Some(b)) if a == b)
            && self.package_version == baseline.package_version
            && self.runtime_version == baseline.runtime_version
    }
}

/// `DefaultClaudeVersion`: the billing `cc_version` prefix.
pub(crate) fn default_version(settings: &Settings) -> String {
    match Profile::default_for(settings).version() {
        Some((a, b, c)) => format!("{a}.{b}.{c}"),
        None => "2.1.280".into(),
    }
}

/// Patch releases at or above the baseline in the same major.minor line keep native
/// passthrough; Claude Code auto-updates patch releases in the background.
pub(crate) fn plausible_user_agent(user_agent: &str, settings: &Settings) -> bool {
    let user_agent = user_agent.trim();
    if !native_user_agent(user_agent) {
        return false;
    }
    match (version(user_agent), Profile::default_for(settings).version()) {
        (Some(c), Some(b)) => c.0 == b.0 && c.1 == b.1 && c.2 >= b.2,
        _ => false,
    }
}

/// The host the Stainless headers describe. Unit tests pin Linux x64, the host the
/// Go fixtures were recorded on, so they compare the same on every machine.
#[cfg(not(test))]
const HOST: (&str, &str) = (std::env::consts::OS, std::env::consts::ARCH);
#[cfg(test)]
const HOST: (&str, &str) = ("linux", "x86_64");

/// Stainless OS/arch names for this host (`mapStainlessOS`, `mapStainlessArch`).
pub(crate) fn host_os() -> String {
    stainless_os(HOST.0)
}
pub(crate) fn host_arch() -> String {
    stainless_arch(HOST.1)
}

fn stainless_os(os: &str) -> String {
    match os {
        "macos" => "MacOS".into(),
        "windows" => "Windows".into(),
        "linux" => "Linux".into(),
        "freebsd" => "FreeBSD".into(),
        other => format!("Other::{other}"),
    }
}

fn stainless_arch(arch: &str) -> String {
    match arch {
        "x86_64" => "x64".into(),
        "aarch64" => "arm64".into(),
        "x86" => "x86".into(),
        other => format!("other::{other}"),
    }
}

fn package_version_ok(s: &str) -> bool {
    let parts: Vec<_> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}
fn runtime_version_ok(s: &str) -> bool {
    s.strip_prefix('v').is_some_and(package_version_ok)
}

/// `extractClaudeDeviceProfile`: a native-looking caller's own software tuple.
fn candidate(headers: &HeaderMap, settings: &Settings) -> Option<Profile> {
    let user_agent = header(headers, "user-agent").trim().to_owned();
    version(&user_agent)?;
    if !native_user_agent(&user_agent) {
        return None;
    }
    let baseline = Profile::default_for(settings);
    let or = |name: &str, fallback: &str| {
        let value = header(headers, name).trim();
        if value.is_empty() {
            fallback.to_owned()
        } else {
            value.to_owned()
        }
    };
    let mut package_version = or("x-stainless-package-version", &baseline.package_version);
    if !package_version_ok(&package_version) {
        package_version = baseline.package_version.clone();
    }
    let mut runtime_version = or("x-stainless-runtime-version", &baseline.runtime_version);
    if !runtime_version_ok(&runtime_version) {
        runtime_version = baseline.runtime_version.clone();
    }
    Some(Profile {
        user_agent,
        package_version,
        runtime_version,
        os: or("x-stainless-os", &baseline.os),
        arch: or("x-stainless-arch", &baseline.arch),
    })
}

struct Entry {
    profile: Profile,
    expires: Instant,
}

fn cache() -> &'static Mutex<HashMap<[u8; 32], Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 32], Entry>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn scope_key(credential_id: &str, api_key: &str, profile: &Profile) -> [u8; 32] {
    let mut key = if !credential_id.trim().is_empty() {
        format!("auth:{}", credential_id.trim())
    } else if !api_key.trim().is_empty() {
        format!("api_key:{}", api_key.trim())
    } else {
        "global".into()
    };
    // Distinct first-party subclients keep separate stabilized profiles.
    let (entrypoint, _) = user_agent_details(&profile.user_agent);
    if !entrypoint.is_empty() && entrypoint != "cli" {
        key.push_str("|subclient:");
        key.push_str(if NATIVE_ENTRYPOINTS.contains(&entrypoint.as_str()) {
            &entrypoint
        } else {
            "other"
        });
    }
    Sha256::digest(key.as_bytes()).into()
}

/// `resolveClaudeDeviceProfileLocal`: per-credential stabilized profile, 7-day TTL.
/// Only an exact baseline tuple from a confirmed caller may populate or refresh it.
pub(crate) fn resolve(credential_id: &str, api_key: &str, headers: &HeaderMap, settings: &Settings) -> Profile {
    let now = Instant::now();
    let baseline = Profile::default_for(settings);
    let pin = |mut p: Profile| {
        p.os = baseline.os.clone();
        p.arch = baseline.arch.clone();
        p
    };
    let normalize = |p: Profile| {
        let mut p = pin(p);
        if !p.meets(&baseline) {
            p.user_agent = baseline.user_agent.clone();
            p.package_version = baseline.package_version.clone();
            p.runtime_version = baseline.runtime_version.clone();
        }
        p
    };
    let candidate = candidate(headers, settings).map(pin).filter(|c| c.meets(&baseline));
    let key = scope_key(
        credential_id,
        api_key,
        candidate.as_ref().unwrap_or(&Profile::default()),
    );
    let mut cache = cache().lock().expect("device profile cache");
    cache.retain(|_, e| e.expires > now);
    let cached = cache.get_mut(&key).filter(|e| !e.profile.user_agent.is_empty());
    match (candidate, cached) {
        (Some(candidate), Some(entry)) => {
            entry.profile = normalize(entry.profile.clone());
            let upgrade = match (candidate.version(), entry.profile.version()) {
                (Some(c), Some(e)) => c > e,
                (Some(_), None) => true,
                _ => false,
            };
            entry.expires = now + PROFILE_TTL;
            if !upgrade {
                return entry.profile.clone();
            }
            entry.profile = candidate.clone();
            candidate
        }
        (Some(candidate), None) => {
            cache.insert(
                key,
                Entry {
                    profile: candidate.clone(),
                    expires: now + PROFILE_TTL,
                },
            );
            candidate
        }
        (None, Some(entry)) => {
            entry.profile = normalize(entry.profile.clone());
            entry.expires = now + PROFILE_TTL;
            entry.profile.clone()
        }
        (None, None) => baseline,
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn stainless_host_names() {
        assert_eq!(stainless_os("macos"), "MacOS");
        assert_eq!(stainless_os("plan9"), "Other::plan9");
        assert_eq!(stainless_arch("aarch64"), "arm64");
        assert_eq!(stainless_arch("riscv64"), "other::riscv64");
    }
    use super::*;

    #[test]
    fn version_parsing_and_patch_floor() {
        assert_eq!(version("claude-cli/2.1.280 (external, cli)"), Some((2, 1, 280)));
        assert_eq!(version(" claude-cli/10.0.3x"), Some((10, 0, 3)));
        assert_eq!(version("claude-cli/2.1"), None);
        let s = Settings::default();
        assert!(plausible_user_agent("claude-cli/2.1.280 (external, cli)", &s));
        assert!(plausible_user_agent("claude-cli/2.1.299 (external, sdk-cli)", &s));
        // The rejected 2.1.220 identity and other release lines are not native.
        assert!(!plausible_user_agent("claude-cli/2.1.220 (external, cli)", &s));
        assert!(!plausible_user_agent("claude-cli/2.2.280 (external, cli)", &s));
        assert!(!plausible_user_agent("claude-cli/2.1.280", &s));
        assert_eq!(default_version(&s), "2.1.280");
    }
}
