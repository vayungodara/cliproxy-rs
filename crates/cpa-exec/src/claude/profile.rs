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

use super::detect::{NATIVE_ENTRYPOINTS, header, native_user_agent, raw_header, user_agent_details};
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
pub(crate) const PROFILE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Go `claudeDeviceProfileLockTTL`: the Home KV write lock.
const LOCK_TTL: Duration = Duration::from_secs(5);

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

/// Native Claude Code at or above the baseline within the baseline's major version
/// keeps native passthrough. Go accepts only patch releases of the baseline's
/// major.minor line, so a newer minor release was rewritten to the older baseline
/// identity and cloaked like a third-party client (docs/DIFFERENCES-FROM-GO.md).
/// With `stabilize-device-profile` Go's rule stays: a stabilized profile forwards only
/// the exact baseline tuple, so a newer release would send its body under baseline
/// headers.
pub(crate) fn plausible_user_agent(user_agent: &str, settings: &Settings) -> bool {
    let user_agent = user_agent.trim();
    if !native_user_agent(user_agent) {
        return false;
    }
    match (version(user_agent), Profile::default_for(settings).version()) {
        (Some(c), Some(b)) if settings.header_defaults.stabilize_device_profile => {
            c.0 == b.0 && c.1 == b.1 && c.2 >= b.2
        }
        (Some(c), Some(b)) => c.0 == b.0 && c >= b,
        _ => false,
    }
}

/// Both Stainless version headers are well formed in their first value, the one
/// forwarding reads. A forwarded release newer than the baseline needs them, so its SDK
/// and runtime versions go upstream with its User-Agent instead of baseline values
/// that release never sends.
pub(crate) fn stainless_versions_ok(headers: &HeaderMap) -> bool {
    stainless_versions(headers) == (true, true)
}

/// Whether the first `X-Stainless-Package-Version` and `X-Stainless-Runtime-Version`
/// values are well formed, in that order.
fn stainless_versions(headers: &HeaderMap) -> (bool, bool) {
    let first = |name: &str| raw_header(headers, name).trim();
    (
        package_version_ok(first("x-stainless-package-version")),
        runtime_version_ok(first("x-stainless-runtime-version")),
    )
}

/// The one rule for treating a caller's User-Agent as Claude Code's own: a plausible
/// native release, and for one newer than the baseline also well-formed Stainless
/// version headers. Detection and the refused-version log both use it.
pub(crate) fn accepted_user_agent(headers: &HeaderMap, user_agent: &str, settings: &Settings) -> bool {
    plausible_user_agent(user_agent, settings)
        && (!newer_than_baseline(user_agent, settings) || stainless_versions_ok(headers))
}

/// The `cc_version` of a billing block cliproxy-rs adds: the forwarded release's own
/// version for a confirmed caller newer than the baseline, else the baseline's.
pub(crate) fn billing_version(user_agent: &str, confirmed: bool, settings: &Settings) -> String {
    match version(user_agent.trim()) {
        Some((a, b, c)) if confirmed && newer_than_baseline(user_agent, settings) => format!("{a}.{b}.{c}"),
        _ => default_version(settings),
    }
}

/// A passed-through caller newer than the baseline: its own Stainless package and
/// runtime versions go upstream with its User-Agent, so the three agree. Go pins both
/// to the baseline values whatever the User-Agent says. Never with
/// `stabilize-device-profile`: a stabilized profile sends only the baseline tuple, so
/// nothing newer is forwarded and Go's rules apply.
pub(crate) fn newer_than_baseline(user_agent: &str, settings: &Settings) -> bool {
    !settings.header_defaults.stabilize_device_profile
        && plausible_user_agent(user_agent, settings)
        && version(user_agent.trim()) > Profile::default_for(settings).version()
}

/// Refused `claude-cli` versions already logged, by a 64-bit hash of the credential
/// ID: one bit per version hash mod 64. Two versions on the same bit share one line,
/// so a collision can drop the line for a different version.
static REFUSED: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();

fn hash64(value: &impl std::hash::Hash) -> u64 {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    BuildHasherDefault::<DefaultHasher>::default().hash_one(value)
}

/// Logs at most once per credential and Claude Code version when a `claude-cli` User-Agent is
/// refused and the baseline identity is sent instead, so the refused client is
/// visible. The caller decides that the identity is replaced. Cost per call: a prefix
/// check on the User-Agent; a refused version also takes one lock and keeps 16 bytes
/// per credential (key hash and seen mask), however many versions it sends. The line
/// names the credential by its auth index, never its file name (which can hold the
/// account email). Returns whether it logged.
pub(crate) fn note_refused(
    credential: &cpa_core::credential::Credential,
    headers: &HeaderMap,
    settings: &Settings,
) -> bool {
    // The value detection judged (`detect`), not the first non-empty one.
    let user_agent = raw_header(headers, "user-agent").trim();
    if !user_agent
        .get(..11)
        .is_some_and(|p| p.eq_ignore_ascii_case("claude-cli/"))
        || accepted_user_agent(headers, user_agent, settings)
    {
        return false;
    }
    let seen = version(user_agent);
    let bit = 1u64 << (hash64(&seen) % 64);
    {
        let mut logged = REFUSED.get_or_init(Mutex::default).lock().expect("refused versions");
        let mask = logged.entry(hash64(&credential.id)).or_default();
        if *mask & bit != 0 {
            return false;
        }
        *mask |= bit;
    }
    let baseline = Profile::default_for(settings).user_agent;
    let version = seen.map_or_else(|| "an unknown version".to_owned(), |(a, b, c)| format!("{a}.{b}.{c}"));
    let index = cpa_core::config::credentials::auth_index(credential);
    // A plausible release is refused only for its Stainless versions: Claude Code sends
    // both, so something on the way removed or changed them.
    let advice = if plausible_user_agent(user_agent, settings) {
        let (missing, these, them) = match stainless_versions(headers) {
            (false, false) => (
                "X-Stainless-Package-Version and X-Stainless-Runtime-Version headers",
                "these headers",
                "them",
            ),
            (false, true) => ("X-Stainless-Package-Version header", "this header", "it"),
            _ => ("X-Stainless-Runtime-Version header", "this header", "it"),
        };
        format!(
            "It is missing a well-formed {missing}, which a release newer than that one needs. \
             Claude Code sends {these}, so the client or a proxy in front of cliproxy-rs is \
             stripping or rewriting {them}."
        )
    } else {
        let rule = if settings.header_defaults.stabilize_device_profile {
            "at or above that version within the same major and minor version (stabilize-device-profile)"
        } else {
            "at or above that version within the same major version"
        };
        format!(
            "Native passthrough needs a native claude-cli User-Agent {rule}. Update Claude Code, \
             or set claude-header-defaults user-agent, package-version and runtime-version to a \
             newer release."
        )
    };
    tracing::warn!(
        "claude: Claude Code {version} on credential {index} is not passed through; requests \
         use the {baseline} identity. {advice}"
    );
    true
}

/// Whether `note_refused` already logged something for this credential.
#[cfg(test)]
pub(crate) fn refused_logged(credential_id: &str) -> bool {
    REFUSED.get().is_some_and(|m| {
        m.lock()
            .unwrap()
            .get(&hash64(&credential_id.to_owned()))
            .is_some_and(|b| *b != 0)
    })
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

pub(crate) fn package_version_ok(s: &str) -> bool {
    let parts: Vec<_> = s.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}
pub(crate) fn runtime_version_ok(s: &str) -> bool {
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

/// Go `claudeDeviceProfileScopedKey`.
fn scope_string(credential_id: &str, api_key: &str, profile: &Profile) -> String {
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
    key
}

fn scope_key(credential_id: &str, api_key: &str, profile: &Profile) -> [u8; 32] {
    Sha256::digest(scope_string(credential_id, api_key, profile).as_bytes()).into()
}

/// Go `normalizeClaudeDeviceProfile`: the configured platform, and the baseline
/// software unless the tuple matches it exactly.
fn normalize(mut profile: Profile, baseline: &Profile) -> Profile {
    profile.os = baseline.os.clone();
    profile.arch = baseline.arch.clone();
    if !profile.meets(baseline) {
        profile.user_agent = baseline.user_agent.clone();
        profile.package_version = baseline.package_version.clone();
        profile.runtime_version = baseline.runtime_version.clone();
    }
    profile
}

/// Go `shouldUpgradeClaudeDeviceProfile`.
fn should_upgrade(candidate: &Profile, current: &Profile) -> bool {
    match (candidate.version(), current.version()) {
        (Some(c), Some(e)) => c > e,
        (Some(_), None) => true,
        _ => false,
    }
}

/// The confirmed caller's own tuple, platform pinned, when it is exactly the baseline.
fn baseline_candidate(headers: &HeaderMap, settings: &Settings, baseline: &Profile) -> Option<Profile> {
    candidate(headers, settings)
        .map(|mut c| {
            c.os = baseline.os.clone();
            c.arch = baseline.arch.clone();
            c
        })
        .filter(|c| c.meets(baseline))
}

/// Go `ResolveClaudeDeviceProfileRequired`: Home KV while a Home client is current,
/// otherwise the local cache. In Home mode an unreachable Home fails the request.
pub(crate) async fn resolve_required(
    credential_id: &str,
    api_key: &str,
    headers: &HeaderMap,
    settings: &Settings,
) -> Result<Profile, String> {
    match cpa_home::kv::current_client() {
        Ok(None) => Ok(resolve(credential_id, api_key, headers, settings)),
        Ok(Some(client)) => resolve_home(&client, credential_id, api_key, headers, settings).await,
        Err(error) => Err(error.to_string()),
    }
}

/// Go `claudeDeviceProfileKVValue`.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct KvValue {
    user_agent: String,
    package_version: String,
    runtime_version: String,
    os: String,
    arch: String,
}

/// Go `json.Marshal(claudeDeviceProfileKVValueFromProfile(profile))`: struct field
/// order and Go's string escaping.
fn kv_json(profile: &Profile) -> Vec<u8> {
    let mut out = b"{".to_vec();
    let fields = [
        ("user_agent", &profile.user_agent),
        ("package_version", &profile.package_version),
        ("runtime_version", &profile.runtime_version),
        ("os", &profile.os),
        ("arch", &profile.arch),
    ];
    for (i, (name, value)) in fields.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        cpa_common::json::marshal_str(&mut out, name.as_bytes(), true);
        out.push(b':');
        cpa_common::json::marshal_str(&mut out, value.as_bytes(), true);
    }
    out.push(b'}');
    out
}

/// Go `readClaudeDeviceProfileValueFromHome`: `None` for a missing or empty record.
async fn read_home(client: &cpa_home::Client, key: &str, baseline: &Profile) -> Result<Option<Profile>, String> {
    let Some(raw) = client.kv_get(key).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let value: KvValue = serde_json::from_slice(&raw)
        .map_err(|e| format!("home kv: decode value: {}", cpa_home::error::redacted_decode_text(&e)))?;
    let profile = Profile {
        user_agent: value.user_agent.trim().to_owned(),
        package_version: value.package_version.trim().to_owned(),
        runtime_version: value.runtime_version.trim().to_owned(),
        os: value.os.trim().to_owned(),
        arch: value.arch.trim().to_owned(),
    };
    if profile.user_agent.is_empty() {
        return Ok(None);
    }
    Ok(Some(normalize(profile, baseline)))
}

/// Go `resolveClaudeDeviceProfileHome`: one profile per credential scope shared by
/// every node, written under a 5-second lock and kept for seven days after last use.
pub(crate) async fn resolve_home(
    client: &cpa_home::Client,
    credential_id: &str,
    api_key: &str,
    headers: &HeaderMap,
    settings: &Settings,
) -> Result<Profile, String> {
    let baseline = Profile::default_for(settings);
    let candidate = baseline_candidate(headers, settings, &baseline);
    let scope = scope_string(
        credential_id,
        api_key,
        candidate.as_ref().unwrap_or(&Profile::default()),
    );
    let value_key = format!("cpa:claude:device-profile:{}", cpa_home::kv::hash_key_part(&scope));
    let expire = |key: String| async move { client.kv_expire(&key, PROFILE_TTL).await.map_err(|e| e.to_string()) };
    let Some(candidate) = candidate else {
        // Go `readClaudeDeviceProfileFromHome`.
        return match read_home(client, &value_key, &baseline).await? {
            Some(profile) => {
                expire(value_key).await?;
                Ok(profile)
            }
            None => Ok(baseline),
        };
    };
    let lock_key = format!("cpa:claude:device-profile-lock:{}", cpa_home::kv::hash_key_part(&scope));
    let locked = client
        .kv_set_nx(&lock_key, b"1", LOCK_TTL)
        .await
        .map_err(|e| e.to_string())?;
    let cached = read_home(client, &value_key, &baseline).await?;
    if let Some(cached) = cached.as_ref().filter(|c| !should_upgrade(&candidate, c)) {
        expire(value_key).await?;
        return Ok(cached.clone());
    }
    if !locked {
        return cached.ok_or_else(|| "home kv device profile lock not acquired and profile missing".to_owned());
    }
    let raw = kv_json(&candidate);
    let options = cpa_home::SetOptions {
        ex: PROFILE_TTL,
        ..Default::default()
    };
    if !client
        .kv_set(&value_key, &raw, options)
        .await
        .map_err(|e| e.to_string())?
    {
        return Err("home kv device profile write skipped".into());
    }
    Ok(candidate)
}

/// `resolveClaudeDeviceProfileLocal`: per-credential stabilized profile, 7-day TTL.
/// Only an exact baseline tuple from a confirmed caller may populate or refresh it.
pub(crate) fn resolve(credential_id: &str, api_key: &str, headers: &HeaderMap, settings: &Settings) -> Profile {
    let now = Instant::now();
    let baseline = Profile::default_for(settings);
    let candidate = baseline_candidate(headers, settings, &baseline);
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
            entry.profile = normalize(entry.profile.clone(), &baseline);
            entry.expires = now + PROFILE_TTL;
            if !should_upgrade(&candidate, &entry.profile) {
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
            entry.profile = normalize(entry.profile.clone(), &baseline);
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
        assert!(!plausible_user_agent("claude-cli/2.1.279 (external, cli)", &s));
        // Newer minor releases of the same major pass (Go refuses them); another major
        // and non-native shapes do not.
        assert!(plausible_user_agent("claude-cli/2.2.0 (external, cli)", &s));
        assert!(plausible_user_agent("claude-cli/2.10.3 (external, cli)", &s));
        assert!(!plausible_user_agent("claude-cli/3.0.0 (external, cli)", &s));
        assert!(!plausible_user_agent("claude-cli/1.9.999 (external, cli)", &s));
        assert!(!plausible_user_agent("claude-cli/2.1.280", &s));
        assert!(!newer_than_baseline("claude-cli/2.1.280 (external, cli)", &s));
        assert!(newer_than_baseline("claude-cli/2.1.281 (external, cli)", &s));
        assert!(newer_than_baseline("claude-cli/2.2.0 (external, cli)", &s));
        assert!(!newer_than_baseline("claude-cli/3.0.0 (external, cli)", &s));
        // Refused claude-cli versions are logged at most once per credential and version.
        let ua_headers = |ua: &str, extra: &[(&str, &str)]| -> HeaderMap {
            let mut h = HeaderMap::new();
            h.insert("user-agent", ua.parse().unwrap());
            for (k, v) in extra {
                h.insert(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
            }
            h
        };
        let cred = |id: &str| {
            cpa_core::credential::Credential::from_file(
                std::path::Path::new("/fake"),
                &std::path::Path::new("/fake").join(id),
                serde_json::json!({"type": "claude"}).as_object().unwrap().clone(),
            )
            .unwrap()
        };
        let (a, b) = (cred("refused-a.json"), cred("refused-b.json"));
        let old = "claude-cli/2.1.220 (external, cli)";
        assert!(note_refused(&a, &ua_headers(old, &[]), &s));
        assert!(!note_refused(&a, &ua_headers(old, &[]), &s));
        assert!(note_refused(&b, &ua_headers(old, &[]), &s));
        assert!(note_refused(
            &a,
            &ua_headers("claude-cli/3.0.0 (external, cli)", &[]),
            &s
        ));
        assert!(!note_refused(
            &a,
            &ua_headers(
                "claude-cli/2.2.0 (external, cli)",
                &[
                    ("x-stainless-package-version", "0.120.4"),
                    ("x-stainless-runtime-version", "v27.0.1")
                ]
            ),
            &s
        ));
        assert!(!note_refused(&a, &ua_headers("OpenAI/Python 1.0", &[]), &s));
        // A client cycling versions costs one fixed entry per credential. Other tests
        // write to the same global map concurrently, so only this key is checked.
        let cycling = cred("refused-cycling.json");
        let logged = (0..1000)
            .filter(|n| {
                let ua = format!("claude-cli/2.0.{n} (external, cli)");
                note_refused(&cycling, &ua_headers(&ua, &[]), &s)
            })
            .count();
        assert!(logged <= 64, "{logged}");
        let mask = REFUSED.get().unwrap().lock().unwrap()[&hash64(&cycling.id)];
        assert!(mask != 0 && mask.count_ones() as usize == logged, "{mask:b} {logged}");
        // A newer release without its Stainless versions is refused, and the line names
        // exactly the headers missing or malformed, and where they were lost.
        let capture = crate::kimi_fixture::LogCapture::default();
        let guard = capture.install();
        let line = |id: &str, headers: &HeaderMap| {
            capture.0.lock().unwrap().clear();
            assert!(note_refused(&cred(id), headers, &s), "{id}");
            capture.0.lock().unwrap().concat()
        };
        let newer = "claude-cli/2.2.3 (external, cli)";
        let both = line("refused-bare.json", &ua_headers(newer, &[]));
        assert!(
            both.contains("missing a well-formed X-Stainless-Package-Version and X-Stainless-Runtime-Version headers,"),
            "{both}"
        );
        assert!(
            both.contains("rewriting them.") && !both.contains("Update Claude Code"),
            "{both}"
        );
        let runtime = line(
            "refused-runtime.json",
            &ua_headers(newer, &[("x-stainless-package-version", "0.120.4")]),
        );
        assert!(
            runtime.contains("missing a well-formed X-Stainless-Runtime-Version header,")
                && !runtime.contains("Package-Version"),
            "{runtime}"
        );
        let package = line(
            "refused-package.json",
            &ua_headers(
                newer,
                &[
                    ("x-stainless-package-version", "0.120"),
                    ("x-stainless-runtime-version", "v27.0.1"),
                ],
            ),
        );
        assert!(
            package.contains("missing a well-formed X-Stainless-Package-Version header,")
                && !package.contains("Runtime-Version"),
            "{package}"
        );
        // A release refused for its version gets the update advice instead.
        let outdated = line("refused-outdated.json", &ua_headers(old, &[]));
        assert!(
            outdated.contains("Update Claude Code") && !outdated.contains("X-Stainless"),
            "{outdated}"
        );
        drop(guard);
        // Detection reads the first User-Agent value even when it is empty, so a
        // claude-cli value after it was never judged and is not logged.
        let mut second = HeaderMap::new();
        second.append("user-agent", http::HeaderValue::from_static(""));
        second.append("user-agent", http::HeaderValue::from_static(old));
        assert!(!note_refused(&cred("refused-second.json"), &second, &s));
        let bare = cred("refused-bare.json");
        let full = [
            ("x-stainless-package-version", "0.120.4"),
            ("x-stainless-runtime-version", "v27.0.1"),
        ];
        assert!(!note_refused(
            &bare,
            &ua_headers("claude-cli/2.2.4 (external, cli)", &full),
            &s
        ));
        assert_eq!(default_version(&s), "2.1.280");
    }

    /// A stabilized device profile forwards only the exact baseline tuple, so Go's
    /// 2.1 patch rule stays: a newer minor release is cloaked as on master.
    #[test]
    fn stabilized_profiles_keep_go_patch_rule() {
        let mut s = Settings::default();
        s.header_defaults.stabilize_device_profile = true;
        assert!(!plausible_user_agent("claude-cli/2.2.0 (external, cli)", &s));
        assert!(plausible_user_agent("claude-cli/2.1.290 (external, cli)", &s));
        assert!(!newer_than_baseline("claude-cli/2.2.0 (external, cli)", &s));
        // Nothing newer is forwarded under a stabilized profile, so Go's rules apply:
        // baseline billing, and no Stainless requirement.
        let newer_patch = "claude-cli/2.1.290 (external, cli)";
        assert!(!newer_than_baseline(newer_patch, &s));
        assert_eq!(billing_version(newer_patch, true, &s), "2.1.280");
        let mut bare = HeaderMap::new();
        bare.insert("user-agent", newer_patch.parse().unwrap());
        assert!(accepted_user_agent(&bare, newer_patch, &s));
    }

    #[test]
    fn billing_version_follows_a_confirmed_newer_release() {
        let s = Settings::default();
        let newer = "claude-cli/2.2.3 (external, cli)";
        assert_eq!(billing_version(newer, true, &s), "2.2.3");
        assert_eq!(
            billing_version(newer, false, &s),
            "2.1.280",
            "cloaked keeps the baseline"
        );
        assert_eq!(
            billing_version("claude-cli/2.1.280 (external, cli)", true, &s),
            "2.1.280"
        );
        assert_eq!(billing_version("OpenAI/Python 1.0", true, &s), "2.1.280");
    }

    /// Go `resolveClaudeDeviceProfileHome` against the same scenarios, recorded by
    /// the reference's zz_rustgolden_test.go: the KV calls in order, their keys,
    /// values and TTLs, and the profile or error each request resolves to.
    #[tokio::test]
    async fn home_kv_profiles_match_go() {
        let golden: serde_json::Value =
            serde_json::from_str(include_str!("testdata/go_device_profile_home.json")).unwrap();
        for case in golden["cases"].as_array().unwrap() {
            let scenario = &case["scenario"];
            let name = scenario["name"].as_str().unwrap();
            let values = std::sync::Arc::new(Mutex::new(HashMap::new()));
            if let Some(preset) = scenario["preset"].as_object() {
                for (k, v) in preset {
                    values.lock().unwrap().insert(k.clone(), v.as_str().unwrap().to_owned());
                }
            }
            let home = cpa_home::fake::FakeHome::start(super::super::kv_test::kv_home(values.clone())).await;
            let client = home.client();
            let mut settings = Settings::default();
            let config = |key: &str| scenario["config"][key].as_str().unwrap_or_default().to_owned();
            settings.header_defaults.user_agent = config("user-agent");
            settings.header_defaults.package_version = config("package-version");
            settings.header_defaults.os = config("os");
            settings.header_defaults.arch = config("arch");
            let steps = scenario["steps"].as_array().unwrap();
            for (i, (step, want)) in steps.iter().zip(case["results"].as_array().unwrap()).enumerate() {
                let mut headers = HeaderMap::new();
                for (k, v) in step["headers"].as_object().unwrap() {
                    headers.insert(
                        http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                        v.as_str().unwrap().parse().unwrap(),
                    );
                }
                let before = home.commands().len();
                let got = resolve_home(
                    &client,
                    step["auth"].as_str().unwrap(),
                    step["api_key"].as_str().unwrap(),
                    &headers,
                    &settings,
                )
                .await;
                let calls: Vec<_> = home.commands()[before..]
                    .iter()
                    .filter_map(|c| super::super::kv_test::as_go_call(c))
                    .collect();
                assert_eq!(serde_json::Value::from(calls), want["calls"], "{name} step {i}: calls");
                match got {
                    Ok(p) => assert_eq!(
                        [p.user_agent, p.package_version, p.runtime_version, p.os, p.arch],
                        serde_json::from_value::<[String; 5]>(want["profile"].clone()).unwrap(),
                        "{name} step {i}: profile"
                    ),
                    Err(e) => assert_eq!(e, want["error"].as_str().unwrap(), "{name} step {i}: error"),
                }
            }
        }
    }
}
