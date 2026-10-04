//! Command-line plugins (internal/pluginhost/command_line.go).
//!
//! Plugins declare flags before the main parse; the binary's Go-rules flag parser asks
//! [`Host::command_line_flag`] for names it does not know and records values with
//! [`Host::set_command_line_flag`]. When any plugin flag was given,
//! [`Host::execute_command_line`] runs the owning plugins instead of the server.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::abi::method;
use crate::api::{
    CommandLineExecutionRequest, CommandLineExecutionResponse, CommandLineFlagValue, CommandLineRegistrationRequest,
    CommandLineRegistrationResponse,
};
use crate::auth::PluginAuth;
use crate::host::Host;
use cpa_core::config::parse_duration;

/// One registered plugin flag (Go `commandLineFlagRecord`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliFlag {
    pub plugin_id: String,
    pub name: String,
    pub usage: String,
    /// bool, string, int, int64, float64 or duration.
    pub kind: String,
    pub default_value: String,
    pub value: String,
    pub set: bool,
}

/// Output of one plugin command, in the order Go writes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Stdout(Bytes),
    Stderr(Bytes),
}

/// Go `validCommandLineFlagName`.
pub fn valid_flag_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name != "help"
        && name != "h"
        && !name.contains([' ', '\t', '\r', '\n', '='])
}

/// Go `normalizeCommandLineFlagType`.
pub fn normalize_flag_type(kind: &str) -> Option<&'static str> {
    Some(match kind.trim().to_lowercase().as_str() {
        "" | "bool" => "bool",
        "string" => "string",
        "int" => "int",
        "int64" => "int64",
        "float64" => "float64",
        "duration" => "duration",
        _ => return None,
    })
}

/// Go `strconv.ParseBool`.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// Go `normalizeCommandLineFlagValue`.
pub fn normalize_flag_value(kind: &str, value: &str) -> Option<String> {
    let blank = value.trim().is_empty();
    match kind {
        "bool" if blank => Some("false".into()),
        "bool" => parse_bool(value).map(|b| b.to_string()),
        "string" => Some(value.to_owned()),
        "int" | "int64" if blank => Some("0".into()),
        "int" | "int64" => value.parse::<i64>().ok().map(|n| n.to_string()),
        "float64" if blank => Some("0".into()),
        "float64" => parse_float(value).map(format_float_g),
        "duration" if blank => Some("0s".into()),
        "duration" => parse_duration(value).map(format_duration),
        _ => None,
    }
}

/// Go `strconv.ParseFloat(s, 64)`: out of range is an error rather than ±Inf, NaN takes
/// no sign, and underscores may separate digits.
/// ponytail: Go's hexadecimal form (`0x1p-2`) is rejected; plugin flags are decimal in
/// practice. Upgrade by porting `atofHex` if a plugin needs it.
pub fn parse_float(s: &str) -> Option<f64> {
    let unsigned = s.strip_prefix(['+', '-']).unwrap_or(s);
    if unsigned.len() != s.len() && unsigned.eq_ignore_ascii_case("nan") {
        return None;
    }
    let digits;
    let text = if s.contains('_') {
        if !underscore_ok(s) {
            return None;
        }
        digits = s.replace('_', "");
        digits.as_str()
    } else {
        s
    };
    let f = text.parse::<f64>().ok()?;
    let literal_inf = unsigned.to_ascii_lowercase().starts_with("inf");
    (!f.is_infinite() || literal_inf).then_some(f)
}

/// Go `strconv.underscoreOK` for a decimal number: each underscore sits between digits.
fn underscore_ok(s: &str) -> bool {
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let mut saw = '^';
    for c in s.chars() {
        if c.is_ascii_digit() {
            saw = '0';
        } else if c == '_' {
            if saw != '0' {
                return false;
            }
            saw = '_';
        } else if saw == '_' {
            return false;
        } else {
            saw = '!';
        }
    }
    saw != '_'
}

/// Go `strconv.FormatFloat(f, 'g', -1, 64)`.
pub fn format_float_g(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "+Inf".into() } else { "-Inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    // Shortest round-trip digits and decimal exponent from Rust's `{:e}`.
    let e = format!("{:e}", f.abs());
    let (mantissa, exp) = e.split_once('e').expect("{:e} has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if f < 0.0 { "-" } else { "" };
    if !(-4..6).contains(&exp) {
        let (first, rest) = digits.split_at(1);
        let frac = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let esign = if exp < 0 { '-' } else { '+' };
        return format!("{sign}{first}{frac}e{esign}{:02}", exp.abs());
    }
    let dp = exp + 1;
    let out = if dp <= 0 {
        format!("0.{}{digits}", "0".repeat((-dp) as usize))
    } else if (dp as usize) >= digits.len() {
        format!("{digits}{}", "0".repeat(dp as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..dp as usize], &digits[dp as usize..])
    };
    format!("{sign}{out}")
}

/// Go `time.Duration.String`.
pub fn format_duration(d: i64) -> String {
    if d == 0 {
        return "0s".into();
    }
    let neg = d < 0;
    let u = d.unsigned_abs();
    let sign = if neg { "-" } else { "" };
    if u < 1_000_000_000 {
        let (unit, scale) = match u {
            0..1_000 => return format!("{sign}{u}ns"),
            1_000..1_000_000 => ("µs", 1_000u64),
            _ => ("ms", 1_000_000u64),
        };
        return format!("{sign}{}{unit}", frac(u, scale));
    }
    let secs = u / 1_000_000_000;
    let nanos = u % 1_000_000_000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let mut out = String::from(sign);
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&frac(s * 1_000_000_000 + nanos, 1_000_000_000));
    out.push('s');
    out
}

/// `v / scale` with the fractional digits Go prints (trailing zeros dropped).
fn frac(v: u64, scale: u64) -> String {
    let whole = v / scale;
    let rem = v % scale;
    if rem == 0 {
        return whole.to_string();
    }
    let width = scale.ilog10() as usize;
    let digits = format!("{rem:0width$}");
    format!("{whole}.{}", digits.trim_end_matches('0'))
}

impl Host {
    /// Go `RegisterCommandLineFlags`: asks every active command-line plugin for its flags
    /// and keeps valid ones that clash neither with a built-in flag (`builtin`) nor with a
    /// higher-priority plugin. Returns the flags accepted now.
    pub async fn register_command_line_flags(&self, builtin: &dyn Fn(&str) -> bool) -> Vec<CliFlag> {
        let mut accepted = Vec::new();
        for record in self.active_records() {
            if !record.plugin.caps.command_line_plugin || self.is_fused(&record.id) || !self.record_current(&record) {
                continue;
            }
            let req = CommandLineRegistrationRequest {
                plugin: record.plugin.metadata.clone(),
            };
            let resp: CommandLineRegistrationResponse =
                match self.call(&record, method::COMMAND_LINE_REGISTER, &req).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!("pluginhost: command-line registrar {} failed: {e}", record.id);
                        continue;
                    }
                };
            for item in resp.flags {
                let name = item.name.trim().to_owned();
                if !valid_flag_name(&name) {
                    tracing::warn!(
                        "pluginhost: plugin {} declared invalid command-line flag {:?}",
                        record.id,
                        item.name
                    );
                    continue;
                }
                let Some(kind) = normalize_flag_type(&item.flag_type) else {
                    tracing::warn!(
                        "pluginhost: plugin {} declared unsupported command-line flag type {:?} for {name}",
                        record.id,
                        item.flag_type
                    );
                    continue;
                };
                let Some(value) = normalize_flag_value(kind, &item.default_value) else {
                    tracing::warn!(
                        "pluginhost: plugin {} declared invalid default value {:?} for {name}",
                        record.id,
                        item.default_value
                    );
                    continue;
                };
                // Go looks the name up on the FlagSet, which already holds the built-in
                // flags and every plugin flag registered before this one.
                let mut state = self.state();
                if builtin(&name) || state.command_line_flags.contains_key(&name) {
                    tracing::warn!(
                        "pluginhost: plugin {} command-line flag {name} conflicts with an existing flag and was skipped",
                        record.id
                    );
                    continue;
                }
                let flag = CliFlag {
                    plugin_id: record.id.clone(),
                    name: name.clone(),
                    usage: item.usage,
                    kind: kind.into(),
                    default_value: value.clone(),
                    value,
                    set: false,
                };
                state.command_line_flags.insert(name, flag.clone());
                accepted.push(flag);
            }
        }
        accepted
    }

    /// A registered plugin flag.
    pub fn command_line_flag(&self, name: &str) -> Option<CliFlag> {
        self.state().command_line_flags.get(name).cloned()
    }

    /// Go `commandLineFlagValue.Set`.
    pub fn set_command_line_flag(&self, name: &str, raw: &str) -> Result<(), String> {
        let mut state = self.state();
        let Some(flag) = state.command_line_flags.get_mut(name) else {
            return Ok(());
        };
        let value = normalize_flag_value(&flag.kind, raw)
            .ok_or_else(|| format!("invalid {} value {}", flag.kind, cpa_common::gostr::quote(raw)))?;
        flag.value = value;
        flag.set = true;
        state.command_line_hits.insert(name.to_owned());
        Ok(())
    }

    /// Go `HasTriggeredCommandLineFlags`.
    pub fn has_triggered_command_line_flags(&self) -> bool {
        !self.state().command_line_hits.is_empty()
    }

    /// Go `ExecuteCommandLine`. `builtin` lists the host's own flags as
    /// `(name, current value)`; `persist` saves an auth a command produced and
    /// returns its path. Returns the exit code, whether any plugin ran, and the output to
    /// write in order.
    pub async fn execute_command_line(
        &self,
        program: &str,
        args: &[String],
        config_path: &str,
        builtin: &[(String, String)],
        persist: &dyn Fn(PluginAuth) -> Result<String, String>,
    ) -> (i32, bool, Vec<Output>) {
        let mut all: BTreeMap<String, CommandLineFlagValue> = builtin
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    CommandLineFlagValue {
                        name: name.clone(),
                        flag_type: String::new(),
                        value: value.clone(),
                        set: false,
                    },
                )
            })
            .collect();
        let mut triggered: BTreeMap<String, BTreeMap<String, CommandLineFlagValue>> = BTreeMap::new();
        {
            let state = self.state();
            for (name, flag) in &state.command_line_flags {
                let value = CommandLineFlagValue {
                    name: name.clone(),
                    flag_type: flag.kind.clone(),
                    value: flag.value.clone(),
                    set: flag.set,
                };
                all.insert(name.clone(), value.clone());
                if state.command_line_hits.contains(name) {
                    triggered
                        .entry(flag.plugin_id.clone())
                        .or_default()
                        .insert(name.clone(), value);
                }
            }
        }
        if triggered.is_empty() {
            return (0, false, Vec::new());
        }
        let mut exit = 0;
        let mut handled = false;
        let mut output = Vec::new();
        let auth_dir = self.host_config_summary().auth_dir;
        for record in self.active_records() {
            if !record.plugin.caps.command_line_plugin || self.is_fused(&record.id) {
                continue;
            }
            let Some(mine) = triggered.get(&record.id) else {
                continue;
            };
            handled = true;
            if !self.record_current(&record) {
                continue;
            }
            let req = CommandLineExecutionRequest {
                plugin: record.plugin.metadata.clone(),
                program: program.into(),
                args: args.to_vec(),
                config_path: config_path.into(),
                host: self.host_config_summary(),
                flags: all.clone(),
                triggered_flags: mine.clone(),
            };
            let mut resp: CommandLineExecutionResponse =
                match self.call(&record, method::COMMAND_LINE_EXECUTE, &req).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        tracing::warn!("pluginhost: command-line plugin {} failed: {e}", record.id);
                        if exit == 0 {
                            exit = 1;
                        }
                        continue;
                    }
                };
            if resp.exit_code == 0 && !resp.auths.is_empty() {
                let mut saved = Vec::new();
                let mut failure = None;
                for (index, data) in std::mem::take(&mut resp.auths).into_iter().enumerate() {
                    let Some(auth) = PluginAuth::from_auth_data(data, "", "", &auth_dir) else {
                        failure = Some(format!("pluginhost: command-line auth {} is invalid", index + 1));
                        break;
                    };
                    let id = auth.id.clone();
                    match persist(auth) {
                        Ok(path) if !path.trim().is_empty() => saved.push(path),
                        Ok(_) => {}
                        Err(e) => {
                            failure = Some(format!("pluginhost: save command-line auth {id}: {e}"));
                            break;
                        }
                    }
                }
                if let Some(message) = failure {
                    push_output(&mut output, Output::Stdout(resp.stdout));
                    push_output(&mut output, Output::Stderr(resp.stderr));
                    push_output(&mut output, Output::Stderr(Bytes::from(format!("{message}\n"))));
                    if exit == 0 {
                        exit = 1;
                    }
                    continue;
                }
                if !saved.is_empty() {
                    let mut stdout = resp.stdout.to_vec();
                    if stdout.last().is_some_and(|b| *b != b'\n') {
                        stdout.push(b'\n');
                    }
                    for path in saved {
                        stdout.extend_from_slice(format!("Authentication saved to {path}\n").as_bytes());
                    }
                    resp.stdout = Bytes::from(stdout);
                }
            }
            push_output(&mut output, Output::Stdout(resp.stdout));
            push_output(&mut output, Output::Stderr(resp.stderr));
            if resp.exit_code != 0 && exit == 0 {
                exit = resp.exit_code as i32;
            }
        }
        (exit, handled, output)
    }
}

fn push_output(out: &mut Vec<Output>, item: Output) {
    let empty = match &item {
        Output::Stdout(b) | Output::Stderr(b) => b.is_empty(),
    };
    if !empty {
        out.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected values printed by Go 1.26 (`strconv.ParseFloat` + `FormatFloat(f, 'g',
    /// -1, 64)`, `time.ParseDuration` + `Duration.String`) for the same inputs.
    #[test]
    fn go_value_formats() {
        for (s, want) in [
            ("0.5", Some("0.5")),
            ("100000", Some("100000")),
            ("1e6", Some("1e+06")),
            ("1234567", Some("1.234567e+06")),
            ("0.0001", Some("0.0001")),
            ("0.00001", Some("1e-05")),
            ("-2.5e-7", Some("-2.5e-07")),
            ("3", Some("3")),
            ("123456789", Some("1.23456789e+08")),
            ("1e21", Some("1e+21")),
            ("1e400", None),
            ("inf", Some("+Inf")),
            ("-Infinity", Some("-Inf")),
            ("1e-400", Some("0")),
            ("+7.25", Some("7.25")),
            ("5e-324", Some("5e-324")),
            ("NaN", Some("NaN")),
            ("nan", Some("NaN")),
            ("+NaN", None),
            ("-nan", None),
            ("-infinity", Some("-Inf")),
            ("+infinity", Some("+Inf")),
            ("infx", None),
            ("1_0", Some("10")),
            ("1__0", None),
            ("_1", None),
            ("1e1_0", Some("1e+10")),
        ] {
            assert_eq!(normalize_flag_value("float64", s).as_deref(), want, "{s}");
        }
        for (s, want) in [
            ("1h30m", Some("1h30m0s")),
            ("1.5s", Some("1.5s")),
            ("300ms", Some("300ms")),
            ("-1h", Some("-1h0m0s")),
            ("2µs", Some("2µs")),
            ("1500us", Some("1.5ms")),
            ("90s", Some("1m30s")),
            ("0", Some("0s")),
            ("7ns", Some("7ns")),
            ("1", None),
            ("1x", None),
            (".s", None),
            ("-", None),
            ("+0", Some("0s")),
            ("1.0000000001h", Some("1h0m0.00000036s")),
            ("0.333333333333333333333h", Some("20m0s")),
            ("2562047h47m16.854775807s", Some("2562047h47m16.854775807s")),
            ("2562047h47m16.854775808s", None),
            ("-2562047h47m16.854775808s", Some("-2562047h47m16.854775808s")),
            ("1h-2m", None),
            ("3.h", Some("3h0m0s")),
            (".5m", Some("30s")),
            ("1.23456789012345678901234567890s", Some("1.23456789s")),
        ] {
            assert_eq!(parse_duration(s).map(format_duration).as_deref(), want, "{s}");
        }
        assert_eq!(parse_duration(""), None);
        assert_eq!(normalize_flag_value("duration", " ").as_deref(), Some("0s"));
        assert_eq!(normalize_flag_value("bool", "T").as_deref(), Some("true"));
        assert_eq!(normalize_flag_value("bool", "yes"), None);
        assert_eq!(normalize_flag_value("int", " "), Some("0".into()));
        assert_eq!(normalize_flag_value("int", "+7").as_deref(), Some("7"));
        assert!(valid_flag_name("x") && !valid_flag_name("-x") && !valid_flag_name("a=b") && !valid_flag_name("help"));
    }
}
