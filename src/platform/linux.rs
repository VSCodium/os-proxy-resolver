/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See LICENSE.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

//! Linux: GNOME's `org.gnome.system.proxy` GSettings tree, read via one
//! `gsettings list-recursively` invocation (recurses into the .http/.https/
//! .socks child schemas). No GNOME (or no `gsettings` binary) means no OS
//! config — the env-var layer above this is then the only source, which is
//! the right default for headless boxes. KDE and proxy authentication are
//! non-goals.
//!
//! Change watching: `dconf watch /system/proxy/` (recursive) when available,
//! falling back to `gsettings monitor org.gnome.system.proxy` (top-level keys
//! only). Both are long-running child processes whose stdout lines signal
//! changes.

use super::{OsProxyConfig, StaticRules};
use crate::bypass::BypassRules;
use crate::types::{LinuxProxyConfig, PlatformProxyConfig, ProxyKind};
use std::collections::HashMap;
use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};

pub(crate) fn read_config() -> OsProxyConfig {
    let output = Command::new("gsettings")
        .args(["list-recursively", "org.gnome.system.proxy"])
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return OsProxyConfig::default(),
    };
    parse_gsettings_output(&String::from_utf8_lossy(&output.stdout))
}

fn parse_gsettings_output(text: &str) -> OsProxyConfig {
    // Lines look like: `org.gnome.system.proxy.http host 'proxy.example.com'`
    let mut values: HashMap<(String, String), String> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, char::is_whitespace);
        if let (Some(schema), Some(key), Some(value)) = (parts.next(), parts.next(), parts.next()) {
            values.insert(
                (schema.to_string(), key.to_string()),
                value.trim().to_string(),
            );
        }
    }
    let get = |schema: &str, key: &str| {
        values
            .get(&(format!("org.gnome.system.proxy{schema}"), key.to_string()))
            .map(String::as_str)
    };

    let mode = get("", "mode").map(unquote).unwrap_or_default();
    let ignore_hosts = get("", "ignore-hosts")
        .map(parse_string_array)
        .unwrap_or_default();
    let mut config = OsProxyConfig {
        platform: Some(PlatformProxyConfig::Linux(LinuxProxyConfig {
            mode: (!mode.is_empty()).then(|| mode.clone()),
            ignore_hosts: ignore_hosts.clone(),
        })),
        ..Default::default()
    };
    match mode.as_str() {
        "auto" => {
            config.pac_url = get("", "autoconfig-url")
                .map(unquote)
                .filter(|s| !s.is_empty());
            // GNOME semantics: "auto" with no PAC URL means WPAD.
            config.auto_detect = config.pac_url.is_none();
        }
        "manual" => {
            let mut rules = StaticRules::default();
            rules.http = host_port(get(".http", "host"), get(".http", "port")).map(ProxyKind::Http);
            rules.https =
                host_port(get(".https", "host"), get(".https", "port")).map(ProxyKind::Http);
            rules.socks =
                host_port(get(".socks", "host"), get(".socks", "port")).map(ProxyKind::Socks);
            rules.bypass = BypassRules::parse(ignore_hosts.iter().map(|s| s.as_str()));
            if !rules.is_empty() {
                config.static_rules = Some(rules);
            }
        }
        _ => {} // "none", unset, or unknown -> direct
    }
    config
}

fn unquote(s: &str) -> String {
    s.trim().trim_matches('\'').to_string()
}

fn host_port(host: Option<&str>, port: Option<&str>) -> Option<String> {
    let host = unquote(host?);
    if host.is_empty() {
        return None;
    }
    let port = port
        .and_then(|p| p.trim().parse::<u16>().ok())
        .filter(|&p| p != 0)?;
    Some(format!("{host}:{port}"))
}

/// Parse a GVariant string array like `['localhost', '127.0.0.0/8']`
/// (possibly with an `@as` type annotation when empty).
fn parse_string_array(s: &str) -> Vec<String> {
    let s = s.trim().trim_start_matches("@as").trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or("");
    inner
        .split(',')
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

// --------------------------------------------------------------------------
// Change watcher

pub(crate) struct Watcher {
    child: Arc<Mutex<Option<Child>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub(crate) fn spawn_watcher(on_change: Arc<dyn Fn() + Send + Sync>) -> Watcher {
    spawn_watcher_thread(spawn_system_watcher, on_change)
}

fn spawn_system_watcher() -> Option<Child> {
    let mut dconf = Command::new("dconf");
    dconf.args(["watch", "/system/proxy/"]);
    configure_watcher_command(&mut dconf);
    let dconf_error = match dconf.spawn() {
        Ok(child) => return Some(child),
        Err(error) => error,
    };

    let mut gsettings = Command::new("gsettings");
    gsettings.args(["monitor", "org.gnome.system.proxy"]);
    configure_watcher_command(&mut gsettings);
    match gsettings.spawn() {
        Ok(child) => Some(child),
        Err(gsettings_error) => {
            log::debug!(
                "proxy watcher: failed to spawn dconf ({dconf_error}) and gsettings \
                 ({gsettings_error}); changes will not be detected"
            );
            None
        }
    }
}

fn spawn_watcher_thread(
    spawn_child: impl FnOnce() -> Option<Child> + Send + 'static,
    on_change: Arc<dyn Fn() + Send + Sync>,
) -> Watcher {
    let child = Arc::new(Mutex::new(None));
    let thread_child = child.clone();
    let (started_tx, started_rx) = mpsc::sync_channel(0);
    let thread = std::thread::Builder::new()
        .name("os-proxy-watch".into())
        .spawn(move || {
            let Some(mut spawned_child) = spawn_child() else {
                let _ = started_tx.send(());
                return;
            };
            let Some(stdout) = spawned_child.stdout.take() else {
                let _ = spawned_child.kill();
                let _ = spawned_child.wait();
                log::debug!("proxy watcher: child stdout was not piped");
                let _ = started_tx.send(());
                return;
            };
            *thread_child
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(spawned_child);
            let _ = started_tx.send(());

            let reader = std::io::BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                // dconf watch prints the changed path on an unindented
                // line, then the value indented; only count the former.
                if !line.is_empty() && !line.starts_with(char::is_whitespace) {
                    on_change();
                }
            }
        })
        .expect("failed to spawn proxy watcher thread");
    started_rx
        .recv()
        .expect("proxy watcher thread stopped during startup");
    Watcher {
        child,
        thread: Some(thread),
    }
}

/// Configure a proxy watcher to terminate if its owning process exits without dropping it.
fn configure_watcher_command(command: &mut Command) {
    let expected_parent = std::process::id() as libc::pid_t;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // SAFETY: pre_exec runs after fork in the single-threaded child. These
    // operations only invoke async-signal-safe Linux system calls.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent may have exited between fork and PR_SET_PDEATHSIG.
            if libc::getppid() != expected_parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// DNS search domains from the OS resolver configuration. On Linux this is
/// `/etc/resolv.conf` (written by systemd-resolved / NetworkManager / the
/// DHCP client), which is the OS-native source.
pub(crate) fn dns_search_domains() -> Vec<String> {
    super::resolv_conf_search_domains()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    const WATCHER_SUBPROCESS_ENV: &str = "OS_PROXY_RESOLVER_WATCHER_SUBPROCESS";

    fn spawn_test_watcher() -> Watcher {
        spawn_watcher_thread(
            || {
                let mut command = Command::new("sleep");
                command.arg("60");
                configure_watcher_command(&mut command);
                Some(command.spawn().expect("failed to spawn test watcher"))
            },
            Arc::new(|| {}),
        )
    }

    fn watcher_child_pid(watcher: &Watcher) -> u32 {
        watcher
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .expect("test watcher child was not started")
            .id()
    }

    fn process_is_running(pid: u32) -> bool {
        let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(error) => panic!("failed to read status for process {pid}: {error}"),
        };
        stat.rsplit_once(") ")
            .and_then(|(_, fields)| fields.chars().next())
            .is_some_and(|state| state != 'Z')
    }

    fn wait_for_process_exit(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_is_running(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    struct ProcessGuard(Option<u32>);

    impl Drop for ProcessGuard {
        fn drop(&mut self) {
            let Some(pid) = self.0 else {
                return;
            };
            // SAFETY: kill with a positive PID and SIGKILL has no memory-safety
            // requirements. It is only a fallback for a failed test.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }

    #[test]
    fn watcher_outlives_the_thread_that_created_it() {
        let (watcher, pid) = std::thread::spawn(|| {
            let watcher = spawn_test_watcher();
            let pid = watcher_child_pid(&watcher);
            (watcher, pid)
        })
        .join()
        .expect("watcher creator thread panicked");

        assert!(
            process_is_running(pid),
            "watcher exited with its short-lived caller thread"
        );
        drop(watcher);
        assert!(
            wait_for_process_exit(pid),
            "watcher did not exit when dropped"
        );
    }

    #[test]
    fn watcher_exits_when_parent_process_exits_without_drop() {
        let pid_file = std::env::temp_dir().join(format!(
            "os-proxy-resolver-watcher-{}.pid",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let status = Command::new(std::env::current_exe().expect("test executable unavailable"))
            .arg("watcher_parent_death_subprocess_helper")
            .arg("--nocapture")
            .env(WATCHER_SUBPROCESS_ENV, &pid_file)
            .status()
            .expect("failed to run watcher parent subprocess");
        let pid_result = std::fs::read_to_string(&pid_file);
        let _ = std::fs::remove_file(&pid_file);

        assert!(status.success(), "watcher parent subprocess failed");
        let pid = pid_result
            .expect("watcher parent subprocess did not report its child PID")
            .parse()
            .expect("watcher parent subprocess reported an invalid child PID");
        let mut guard = ProcessGuard(Some(pid));
        let exited = wait_for_process_exit(pid);
        if exited {
            guard.0 = None;
        }
        assert!(exited, "watcher survived after its parent process exited");
    }

    #[test]
    fn watcher_parent_death_subprocess_helper() {
        let Some(pid_file) = std::env::var_os(WATCHER_SUBPROCESS_ENV) else {
            return;
        };
        let watcher = spawn_test_watcher();
        std::fs::write(
            Path::new(&pid_file),
            watcher_child_pid(&watcher).to_string(),
        )
        .expect("failed to report watcher PID");
        std::process::exit(0);
    }

    #[test]
    fn parses_manual_mode() {
        let out = "\
org.gnome.system.proxy mode 'manual'
org.gnome.system.proxy autoconfig-url ''
org.gnome.system.proxy ignore-hosts ['localhost', '127.0.0.0/8', '::1']
org.gnome.system.proxy.http host 'hp.example.com'
org.gnome.system.proxy.http port 3128
org.gnome.system.proxy.https host ''
org.gnome.system.proxy.https port 0
org.gnome.system.proxy.socks host 'sp.example.com'
org.gnome.system.proxy.socks port 1080
";
        let cfg = parse_gsettings_output(out);
        assert!(!cfg.auto_detect);
        assert_eq!(cfg.pac_url, None);
        let rules = cfg.static_rules.unwrap();
        assert_eq!(
            rules.http,
            Some(ProxyKind::Http("hp.example.com:3128".into()))
        );
        assert_eq!(rules.https, None);
        assert_eq!(
            rules.socks,
            Some(ProxyKind::Socks("sp.example.com:1080".into()))
        );
        assert!(rules.bypass.matches("localhost", 80));
        assert!(rules.bypass.matches("127.0.0.1", 80));
        // https falls back to socks
        assert_eq!(
            rules.proxy_for_scheme("https"),
            Some(&ProxyKind::Socks("sp.example.com:1080".into()))
        );
    }

    #[test]
    fn parses_auto_modes() {
        let with_url = "org.gnome.system.proxy mode 'auto'\norg.gnome.system.proxy autoconfig-url 'http://x/p.pac'\n";
        let cfg = parse_gsettings_output(with_url);
        assert!(!cfg.auto_detect);
        assert_eq!(cfg.pac_url.as_deref(), Some("http://x/p.pac"));

        let wpad = "org.gnome.system.proxy mode 'auto'\norg.gnome.system.proxy autoconfig-url ''\n";
        let cfg = parse_gsettings_output(wpad);
        assert!(cfg.auto_detect);
        assert_eq!(cfg.pac_url, None);
    }

    #[test]
    fn none_mode_is_direct() {
        let cfg = parse_gsettings_output("org.gnome.system.proxy mode 'none'\n");
        assert!(!cfg.auto_detect);
        assert!(cfg.pac_url.is_none());
        assert!(cfg.static_rules.is_none());
    }

    #[test]
    fn empty_array_annotation() {
        assert_eq!(parse_string_array("@as []"), Vec::<String>::new());
        assert_eq!(parse_string_array("['a', 'b']"), vec!["a", "b"]);
    }

    fn gsettings_get(schema: &str, key: &str) -> Option<String> {
        let out = Command::new("gsettings")
            .args(["get", schema, key])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn gsettings_set(schema: &str, key: &str, value: &str) -> bool {
        Command::new("gsettings")
            .args(["set", schema, key, value])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Restores the touched GSettings keys on drop. Values captured with
    /// `gsettings get` are already in the exact form `gsettings set` accepts.
    struct GSettingsGuard {
        saved: Vec<(&'static str, &'static str, Option<String>)>,
    }

    impl GSettingsGuard {
        fn save(keys: &[(&'static str, &'static str)]) -> Self {
            let saved = keys
                .iter()
                .map(|&(schema, key)| (schema, key, gsettings_get(schema, key)))
                .collect();
            GSettingsGuard { saved }
        }
    }

    impl Drop for GSettingsGuard {
        fn drop(&mut self) {
            for (schema, key, value) in &self.saved {
                if let Some(value) = value {
                    let _ = gsettings_set(schema, key, value);
                }
            }
        }
    }

    // Round-trips manual and auto proxy configs through GNOME's GSettings and
    // reads them back via `read_config`. Only runs when
    // `OS_PROXY_RESOLVER_OS_TESTS` is set (it mutates the session's proxy
    // settings and needs a working D-Bus/dconf backend), which the dedicated
    // CI job provides via `dbus-run-session`.
    #[test]
    fn os_roundtrip_reads_gsettings_manual_and_auto() {
        if std::env::var_os("OS_PROXY_RESOLVER_OS_TESTS").is_none() {
            eprintln!(
                "skipping os_roundtrip_reads_gsettings_manual_and_auto: \
                 set OS_PROXY_RESOLVER_OS_TESTS=1 to run OS round-trip tests"
            );
            return;
        }
        // No working gsettings / GNOME proxy schema (e.g. headless minimal
        // image) means there is nothing to round-trip through.
        if gsettings_get("org.gnome.system.proxy", "mode").is_none() {
            eprintln!(
                "skipping os_roundtrip_reads_gsettings_manual_and_auto: \
                 gsettings org.gnome.system.proxy unavailable"
            );
            return;
        }

        let _guard = GSettingsGuard::save(&[
            ("org.gnome.system.proxy", "mode"),
            ("org.gnome.system.proxy", "autoconfig-url"),
            ("org.gnome.system.proxy", "ignore-hosts"),
            ("org.gnome.system.proxy.http", "host"),
            ("org.gnome.system.proxy.http", "port"),
            ("org.gnome.system.proxy.socks", "host"),
            ("org.gnome.system.proxy.socks", "port"),
        ]);

        assert!(gsettings_set(
            "org.gnome.system.proxy.http",
            "host",
            "hp.example.com"
        ));
        assert!(gsettings_set("org.gnome.system.proxy.http", "port", "3128"));
        assert!(gsettings_set(
            "org.gnome.system.proxy.socks",
            "host",
            "sp.example.com"
        ));
        assert!(gsettings_set(
            "org.gnome.system.proxy.socks",
            "port",
            "1080"
        ));
        assert!(gsettings_set(
            "org.gnome.system.proxy",
            "ignore-hosts",
            "['localhost', '127.0.0.0/8']"
        ));
        // Set the mode last so the read observes a fully-populated config.
        assert!(gsettings_set("org.gnome.system.proxy", "mode", "manual"));

        let cfg = read_config();
        let rules = cfg
            .static_rules
            .expect("expected static rules from manual gsettings config");
        assert_eq!(
            rules.http,
            Some(ProxyKind::Http("hp.example.com:3128".into()))
        );
        assert_eq!(
            rules.socks,
            Some(ProxyKind::Socks("sp.example.com:1080".into()))
        );
        assert!(rules.bypass.matches("localhost", 80));

        // Auto mode with an explicit PAC URL.
        assert!(gsettings_set(
            "org.gnome.system.proxy",
            "autoconfig-url",
            "http://wpad.example.com/proxy.pac"
        ));
        assert!(gsettings_set("org.gnome.system.proxy", "mode", "auto"));
        let cfg = read_config();
        assert_eq!(
            cfg.pac_url.as_deref(),
            Some("http://wpad.example.com/proxy.pac")
        );
        assert!(!cfg.auto_detect);
    }
}
