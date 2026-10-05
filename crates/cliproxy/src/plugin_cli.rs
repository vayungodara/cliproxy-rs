//! Plugin command-line flags in the binary (cmd/server/main.go): plugins load from the
//! bootstrap config before the flag parse and declare their flags; when one is given,
//! the plugins that own it run instead of the server (Go `RegisterCommandLineFlags`,
//! `HasTriggeredCommandLineFlags`, `ExecuteCommandLine`).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::ArgAction;
use cpa_core::config::Config;
use cpa_plugin::Host;
use cpa_plugin::cli::{CliFlag, Output};

/// Plugin-declared flags as Go's `flag` package sees them: a `flag.Value` looked up by
/// name, `IsBoolFlag` for bools, and `Set` called for every occurrence as it is parsed.
pub trait PluginFlags {
    /// `Some(is_bool)` for a plugin flag.
    fn lookup(&self, name: &str) -> Option<bool>;
    fn set(&self, name: &str, value: &str) -> Result<(), String>;
}

/// No plugin flags (the `discover` subcommand, and Go's discover mode).
impl PluginFlags for () {
    fn lookup(&self, _: &str) -> Option<bool> {
        None
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
}

impl PluginFlags for Host {
    fn lookup(&self, name: &str) -> Option<bool> {
        self.command_line_flag(name).map(|f| f.kind == "bool")
    }
    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        self.set_command_line_flag(name, value)
    }
}

/// Go `pluginBootstrapConfigPath`: the `-config` value from a raw scan of the arguments
/// (up to `--`), else the default path.
pub fn bootstrap_config_path(args: &[String], default: &str) -> PathBuf {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--" => break,
            "-config" | "--config" => {
                return match args.get(i + 1) {
                    Some(path) => PathBuf::from(path),
                    None => default_config_path(default),
                };
            }
            _ => {
                if let Some(path) = arg.strip_prefix("-config=").or_else(|| arg.strip_prefix("--config=")) {
                    return PathBuf::from(path);
                }
            }
        }
        i += 1;
    }
    default_config_path(default)
}

/// Go `defaultPluginBootstrapConfigPath`.
fn default_config_path(default: &str) -> PathBuf {
    if !default.trim().is_empty() {
        return PathBuf::from(default);
    }
    match std::env::current_dir() {
        Ok(wd) => wd.join("config.yaml"),
        Err(_) => PathBuf::from("config.yaml"),
    }
}

/// Go `loadPluginBootstrapConfig`: the file's config; an empty one (plugins off) when it
/// is missing, empty or invalid.
pub fn load_bootstrap_config(path: &Path) -> Config {
    let empty = || Config::parse("").expect("an empty config parses");
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("failed to read plugin bootstrap config: {e}");
            }
            return empty();
        }
    };
    let text = String::from_utf8_lossy(&raw);
    if text.trim().is_empty() {
        return empty();
    }
    Config::parse(&text).unwrap_or_else(|e| {
        tracing::warn!("failed to parse plugin bootstrap config: {e:#}");
        empty()
    })
}

/// Loads plugins from the bootstrap config and registers their flags. `builtin` says
/// whether the binary already has a flag of that name. Returns the host and the flags
/// it accepted.
pub async fn bootstrap(args: &[String], builtin: &dyn Fn(&str) -> bool) -> (Host, Vec<CliFlag>) {
    let host = Host::new();
    let cfg = load_bootstrap_config(&bootstrap_config_path(args, ""));
    host.apply_config(Arc::new(cfg)).await;
    let flags = host.register_command_line_flags(builtin).await;
    (host, flags)
}

/// The command with the plugin flags added, so `-h` lists them as Go's usage does.
/// Parsing never reaches clap for them (the flag rewrite consumes them first).
pub fn with_flags(mut cmd: clap::Command, flags: &[CliFlag]) -> clap::Command {
    for flag in flags {
        // Go has no -version flag; a plugin may own the name.
        if flag.name == "version" {
            cmd = cmd.disable_version_flag(true);
        }
        let action = if flag.kind == "bool" {
            ArgAction::SetTrue
        } else {
            ArgAction::Set
        };
        // clap (without its `string` feature) takes static names; the few plugin flags
        // are leaked once at startup.
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        cmd = cmd.arg(
            clap::Arg::new(leak(format!("plugin:{}", flag.name)))
                .long(leak(flag.name.clone()))
                .help(leak(flag.usage.clone()))
                .action(action),
        );
    }
    cmd
}

/// Go `flag.CommandLine.VisitAll` for the built-in flags: each flag's `Value.String()`
/// after the parse (`flag.Func` flags print as empty).
pub fn builtin_values(cmd: &clap::Command, matches: &clap::ArgMatches) -> Vec<(String, String)> {
    cmd.get_arguments()
        .filter(|arg| !arg.get_id().as_str().starts_with("plugin:"))
        .filter_map(|arg| {
            let name = arg.get_long()?;
            // log-file and working-dir belong to this process, not Go's plugin
            // command-line contract.
            if matches!(name, "help" | "version" | "log-file" | "working-dir") {
                return None;
            }
            let id = arg.get_id().as_str();
            let value = match arg.get_action() {
                ArgAction::SetTrue => matches.get_flag(id).to_string(),
                ArgAction::Append => String::new(),
                _ => {
                    if let Ok(Some(v)) = matches.try_get_one::<String>(id) {
                        v.clone()
                    } else if let Ok(Some(v)) = matches.try_get_one::<i64>(id) {
                        v.to_string()
                    } else if let Ok(Some(v)) = matches.try_get_one::<u16>(id) {
                        v.to_string()
                    } else {
                        String::new()
                    }
                }
            };
            Some((name.to_owned(), value))
        })
        .collect()
}

/// Go's `ExecuteCommandLine` step in main: applies the loaded config, then runs the
/// plugins whose flags were given, writing their output and saving the auths they
/// produce to the auth directory. Returns the exit code when a plugin handled the run.
pub async fn execute(host: &Host, config: &Config, config_path: &Path, builtin: &[(String, String)]) -> Option<i32> {
    host.apply_config(Arc::new(config.clone())).await;
    if !host.has_triggered_command_line_flags() {
        return None;
    }
    let raw: Vec<String> = std::env::args().collect();
    let program = raw.first().cloned().unwrap_or_default();
    let auth_dir = host.host_config_summary().auth_dir;
    let persist = |auth: cpa_plugin::auth::PluginAuth| auth.save_file(&auth_dir, false);
    let (code, handled, output) = host
        .execute_command_line(
            &program,
            raw.get(1..).unwrap_or_default(),
            &config_path.to_string_lossy(),
            builtin,
            &persist,
        )
        .await;
    for item in output {
        let written = match item {
            Output::Stdout(data) => std::io::stdout()
                .write_all(&data)
                .and_then(|()| std::io::stdout().flush()),
            Output::Stderr(data) => std::io::stderr().write_all(&data),
        };
        if let Err(e) = written {
            tracing::warn!("pluginhost: failed to write command-line plugin output: {e}");
        }
    }
    handled.then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bootstrap_path_scans_like_go() {
        // Go scans every argument (not only flags) until `--`.
        assert_eq!(
            bootstrap_config_path(&args(&["x", "-config", "a.yaml"]), "d"),
            PathBuf::from("a.yaml")
        );
        assert_eq!(
            bootstrap_config_path(&args(&["--config=b.yaml"]), "d"),
            PathBuf::from("b.yaml")
        );
        assert_eq!(bootstrap_config_path(&args(&["-config"]), "d"), PathBuf::from("d"));
        assert_eq!(
            bootstrap_config_path(&args(&["--", "-config", "a"]), "d"),
            PathBuf::from("d")
        );
        // `-config` takes the next argument even when it looks like a flag.
        assert_eq!(
            bootstrap_config_path(&args(&["-config", "-tui"]), "d"),
            PathBuf::from("-tui")
        );
        assert_eq!(bootstrap_config_path(&args(&["---config=c"]), "d"), PathBuf::from("d"));
    }
}
