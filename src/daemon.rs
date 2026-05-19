use crate::{Error, Result, config::Provider};
use std::{
    env,
    fmt::Write as _,
    fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::Command,
};

const LAUNCHD_LABEL: &str = "com.codexia.daemon";
const SYSTEMD_UNIT: &str = "codexia.service";

/// Options used to install the user-scoped background daemon service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInstallOptions {
    /// Absolute or resolvable path to the `codexia` executable to launch.
    pub executable: PathBuf,
    /// Bind address passed to `codexia serve`.
    pub bind: String,
    /// Optional auth file passed through to the daemon process.
    pub auth_file: Option<PathBuf>,
    /// Repeated `-v` count passed through to `codexia serve`.
    pub verbosity: u8,
    /// Optional API key injected into the daemon process arguments.
    pub api_key: Option<String>,
    /// Upstream provider passed through to `codexia serve`.
    pub provider: Option<Provider>,
    /// Optional fallback model passed through to `codexia serve`.
    pub model_fallback: Option<String>,
}

/// Installs the daemon service definition for the current platform.
///
/// # Errors
///
/// Returns an error when the platform is unsupported, filesystem writes fail,
/// or the platform service manager rejects the installation.
pub fn install(options: &DaemonInstallOptions) -> Result<()> {
    match platform()? {
        Platform::MacOs => install_launchd(options),
        Platform::Linux => install_systemd(options),
    }
}

/// Reinstalls the daemon by removing any existing service definition first.
///
/// # Errors
///
/// Returns an error when uninstalling the existing service or installing the
/// replacement definition fails.
pub fn reinstall(options: &DaemonInstallOptions) -> Result<()> {
    uninstall()?;
    install(options)
}

/// Starts the installed daemon service for the current user.
///
/// # Errors
///
/// Returns an error when the service is not installed or the platform service
/// manager rejects the start request.
pub fn start() -> Result<()> {
    match platform()? {
        Platform::MacOs => {
            let plist = launchd_plist_path()?;
            let domain = launchd_domain()?;
            if !plist.exists() {
                return Err(Error::config(
                    "daemon is not installed; run `codexia daemon install` first",
                ));
            }
            run_command("launchctl", ["bootstrap", &domain, path_str(&plist)?])?;
            Ok(())
        }
        Platform::Linux => systemctl(["start", SYSTEMD_UNIT]),
    }
}

/// Stops the running daemon service for the current user.
///
/// # Errors
///
/// Returns an error when the platform service manager rejects the stop request.
pub fn stop() -> Result<()> {
    match platform()? {
        Platform::MacOs => {
            let domain = launchd_domain()?;
            run_command("launchctl", ["bootout", &domain, LAUNCHD_LABEL])
        }
        Platform::Linux => systemctl(["stop", SYSTEMD_UNIT]),
    }
}

/// Restarts the daemon service for the current user.
///
/// # Errors
///
/// Returns an error when the platform service manager rejects the restart request.
pub fn restart() -> Result<()> {
    match platform()? {
        Platform::MacOs => {
            let domain = launchd_domain()?;
            let target = format!("{domain}/{LAUNCHD_LABEL}");
            run_command("launchctl", ["kickstart", "-k", &target])
        }
        Platform::Linux => systemctl(["restart", SYSTEMD_UNIT]),
    }
}

/// Shows the daemon service status for the current user.
///
/// # Errors
///
/// Returns an error when the platform service manager rejects the status request.
pub fn status() -> Result<()> {
    match platform()? {
        Platform::MacOs => status_launchd(),
        Platform::Linux => systemctl(["status", SYSTEMD_UNIT, "--no-pager"]),
    }
}

/// Removes the daemon service definition and stops it if it is running.
///
/// # Errors
///
/// Returns an error when the service definition cannot be removed from disk.
pub fn uninstall() -> Result<()> {
    match platform()? {
        Platform::MacOs => {
            let domain = launchd_domain()?;
            let _ = run_command("launchctl", ["bootout", &domain, LAUNCHD_LABEL]);
            let plist = launchd_plist_path()?;
            remove_if_exists(&plist)?;
            println!("removed {}", plist.display());
            Ok(())
        }
        Platform::Linux => {
            let _ = systemctl(["stop", SYSTEMD_UNIT]);
            let _ = systemctl(["disable", SYSTEMD_UNIT]);
            let unit = systemd_unit_path()?;
            remove_if_exists(&unit)?;
            let _ = systemctl(["daemon-reload"]);
            println!("removed {}", unit.display());
            Ok(())
        }
    }
}

fn install_launchd(options: &DaemonInstallOptions) -> Result<()> {
    let plist = launchd_plist_path()?;
    let parent = plist
        .parent()
        .ok_or_else(|| Error::config("launchd plist path has no parent directory"))?;
    fs::create_dir_all(parent)?;

    let log_dir = codexia_home()?;
    fs::create_dir_all(&log_dir)?;
    fs::write(&plist, launchd_plist(options, &log_dir))?;
    println!("installed {}", plist.display());
    println!("run `codexia daemon start` to start now; launchd will load it on login");
    println!("run `codexia daemon status` to inspect the user service");
    Ok(())
}

fn install_systemd(options: &DaemonInstallOptions) -> Result<()> {
    let unit = systemd_unit_path()?;
    let parent = unit
        .parent()
        .ok_or_else(|| Error::config("systemd unit path has no parent directory"))?;
    fs::create_dir_all(parent)?;

    fs::write(&unit, systemd_unit(options))?;
    systemctl(["daemon-reload"])?;
    systemctl(["enable", SYSTEMD_UNIT])?;
    println!("installed {}", unit.display());
    println!("run `codexia daemon start` to start now");
    println!("inspect it with `codexia daemon status` or `systemctl --user status {SYSTEMD_UNIT}`");
    Ok(())
}

fn status_launchd() -> Result<()> {
    let plist = launchd_plist_path()?;
    let log_dir = codexia_home()?;
    let stdout_log = log_dir.join("codexia.out.log");
    let stderr_log = log_dir.join("codexia.err.log");

    if !plist.exists() {
        print_launchd_status_panel(&LaunchdStatusView {
            label: LAUNCHD_LABEL,
            plist: &plist,
            stdout_log: &stdout_log,
            stderr_log: &stderr_log,
            installed: false,
            loaded: false,
            state: Some("not installed"),
            pid: None,
            last_exit_code: None,
        });
        return Ok(());
    }

    let domain = launchd_domain()?;
    let target = format!("{domain}/{LAUNCHD_LABEL}");
    let output = Command::new("launchctl")
        .args(["print", &target])
        .output()?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout);
        let summary = parse_launchctl_print_summary(&text);
        print_launchd_status_panel(&LaunchdStatusView {
            label: LAUNCHD_LABEL,
            plist: &plist,
            stdout_log: &stdout_log,
            stderr_log: &stderr_log,
            installed: true,
            loaded: true,
            state: summary.state,
            pid: summary.pid,
            last_exit_code: summary.last_exit_code,
        });
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_launchctl_service_missing(&stderr) {
        print_launchd_status_panel(&LaunchdStatusView {
            label: LAUNCHD_LABEL,
            plist: &plist,
            stdout_log: &stdout_log,
            stderr_log: &stderr_log,
            installed: true,
            loaded: false,
            state: Some("stopped"),
            pid: None,
            last_exit_code: None,
        });
        return Ok(());
    }

    Err(Error::config(format!(
        "launchctl print failed with status {}: {}",
        output.status,
        stderr.trim()
    )))
}

fn serve_args(options: &DaemonInstallOptions) -> Vec<String> {
    // Build the exact argv list so both launchd and systemd invoke `codexia serve`
    // without relying on shell parsing.
    let mut args = vec![
        options.executable.display().to_string(),
        "serve".to_owned(),
        "--bind".to_owned(),
        options.bind.clone(),
    ];

    if let Some(path) = &options.auth_file {
        args.push("--auth-file".to_owned());
        args.push(path.display().to_string());
    }
    for _ in 0..options.verbosity {
        args.push("-v".to_owned());
    }
    if let Some(api_key) = &options.api_key {
        args.push("--api-key".to_owned());
        args.push(api_key.clone());
    }
    if let Some(provider) = options.provider {
        args.push("--provider".to_owned());
        args.push(provider.to_string());
    }
    if let Some(model_fallback) = &options.model_fallback {
        args.push("--model-fallback".to_owned());
        args.push(model_fallback.clone());
    }
    args
}

fn launchd_plist(options: &DaemonInstallOptions, log_dir: &Path) -> String {
    // launchd expects each argument as a distinct XML string entry in
    // `ProgramArguments`, so escape each value before embedding it.
    let args = serve_args(options)
        .into_iter()
        .map(|arg| format!("        <string>{}</string>", xml_escape(&arg)))
        .collect::<Vec<_>>()
        .join("\n");
    let stdout = log_dir.join("codexia.out.log");
    let stderr = log_dir.join("codexia.err.log");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{}</string>
    <key>ProgramArguments</key>
    <array>
{}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{}</string>
    <key>StandardErrorPath</key>
    <string>{}</string>
</dict>
</plist>
"#,
        LAUNCHD_LABEL,
        args,
        xml_escape(&stdout.display().to_string()),
        xml_escape(&stderr.display().to_string())
    )
}

fn systemd_unit(options: &DaemonInstallOptions) -> String {
    // Quote each argument individually because `ExecStart=` is parsed as a single
    // command line by systemd rather than as a structured argv array.
    let command = serve_args(options)
        .into_iter()
        .map(|arg| systemd_quote(&arg))
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        r"[Unit]
Description=Codexia OpenAI-compatible Codex OAuth gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={command}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"
    )
}

fn launchd_plist_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn systemd_unit_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".config/systemd/user").join(SYSTEMD_UNIT))
}

fn codexia_home() -> Result<PathBuf> {
    if let Ok(path) = env::var("CODEXIA_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(home_dir()?.join(".codexia"))
}

fn home_dir() -> Result<PathBuf> {
    env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| Error::config("HOME is not set"))
}

fn launchd_domain() -> Result<String> {
    let output = Command::new("id").arg("-u").output()?;
    if !output.status.success() {
        return Err(Error::config("failed to determine current user id"));
    }
    let uid = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(format!("gui/{uid}"))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct LaunchctlPrintSummary<'a> {
    state: Option<&'a str>,
    pid: Option<&'a str>,
    last_exit_code: Option<&'a str>,
}

struct LaunchdStatusView<'a> {
    label: &'a str,
    plist: &'a Path,
    stdout_log: &'a Path,
    stderr_log: &'a Path,
    installed: bool,
    loaded: bool,
    state: Option<&'a str>,
    pid: Option<&'a str>,
    last_exit_code: Option<&'a str>,
}

fn parse_launchctl_print_summary(text: &str) -> LaunchctlPrintSummary<'_> {
    let mut summary = LaunchctlPrintSummary::default();

    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("state = ") {
            summary.state = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("pid = ") {
            summary.pid = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("last exit code = ") {
            summary.last_exit_code = Some(value.trim());
        }
    }

    summary
}

fn is_launchctl_service_missing(stderr: &str) -> bool {
    let stderr = stderr.trim();
    stderr.contains("Could not find service")
        || stderr.contains("service does not exist")
        || stderr.contains("No such process")
}

fn print_launchd_status_panel(view: &LaunchdStatusView<'_>) {
    print!("{}", format_launchd_status_panel(view));
}

fn format_launchd_status_panel(view: &LaunchdStatusView<'_>) -> String {
    let mut output = String::new();
    let overall = overall_status_label(view);

    let _ = writeln!(
        output,
        "{}",
        styled(&format!("Codexia daemon ({})", overall.0), overall.1, true)
    );
    let _ = writeln!(output, "{}", "-".repeat(32));
    push_kv(&mut output, "Manager", "launchd");
    push_kv(&mut output, "Label", view.label);
    push_kv(
        &mut output,
        "Installed",
        bool_label(view.installed, "yes", "no"),
    );
    push_kv(&mut output, "Loaded", bool_label(view.loaded, "yes", "no"));
    push_kv(&mut output, "State", &styled(overall.0, overall.1, false));

    if let Some(pid) = view.pid {
        push_kv(&mut output, "PID", pid);
    }
    if let Some(code) = view.last_exit_code {
        push_kv(&mut output, "Last exit", code);
    }

    output.push('\n');
    let _ = writeln!(output, "Paths");
    let _ = writeln!(output, "{}", "-".repeat(32));
    push_kv(&mut output, "plist", &view.plist.display().to_string());
    push_kv(
        &mut output,
        "stdout log",
        &view.stdout_log.display().to_string(),
    );
    push_kv(
        &mut output,
        "stderr log",
        &view.stderr_log.display().to_string(),
    );

    output.push('\n');
    let _ = writeln!(output, "Hints");
    let _ = writeln!(output, "{}", "-".repeat(32));
    if !view.installed {
        let _ = writeln!(
            output,
            "  run `codexia daemon install` to create the LaunchAgent"
        );
    } else if !view.loaded {
        let _ = writeln!(
            output,
            "  run `codexia daemon start` to load the LaunchAgent now"
        );
    } else {
        let _ = writeln!(
            output,
            "  run `codexia daemon restart` after changing daemon options"
        );
    }
    let _ = writeln!(
        output,
        "  run `tail -n 50 {}` to inspect recent daemon output",
        view.stderr_log.display()
    );

    output
}

fn push_kv(output: &mut String, key: &str, value: &str) {
    let _ = writeln!(output, "{key:>10}  {value}");
}

fn overall_status_label<'a>(view: &'a LaunchdStatusView<'_>) -> (&'a str, AnsiColor) {
    if !view.installed {
        return ("NOT INSTALLED", AnsiColor::Yellow);
    }
    if !view.loaded {
        return ("STOPPED", AnsiColor::Yellow);
    }

    match view.state.unwrap_or("unknown") {
        "running" => ("RUNNING", AnsiColor::Green),
        "waiting" => ("WAITING", AnsiColor::Green),
        "exited" => ("EXITED", AnsiColor::Yellow),
        "failed" => ("FAILED", AnsiColor::Red),
        _ => ("UNKNOWN", AnsiColor::Yellow),
    }
}

const fn bool_label(enabled: bool, yes: &'static str, no: &'static str) -> &'static str {
    if enabled { yes } else { no }
}

fn styled(value: &str, color: AnsiColor, bold: bool) -> String {
    if !std::io::stdout().is_terminal() {
        return value.to_owned();
    }

    let bold_code = if bold { "1;" } else { "" };
    format!("\x1b[{bold_code}{}m{value}\x1b[0m", color.code())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnsiColor {
    Green,
    Yellow,
    Red,
}

impl AnsiColor {
    const fn code(self) -> &'static str {
        match self {
            Self::Green => "32",
            Self::Yellow => "33",
            Self::Red => "31",
        }
    }
}

fn systemctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let mut command = Command::new("systemctl");
    command.arg("--user").args(args);
    run_status(&mut command)
}

fn run_command<const N: usize>(program: &str, args: [&str; N]) -> Result<()> {
    let mut command = Command::new(program);
    command.args(args);
    run_status(&mut command)
}

fn run_status(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::config(format!(
            "command failed with status {status}: {command:?}"
        )))
    }
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| Error::config("path is not valid UTF-8"))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn platform() -> Result<Platform> {
    if cfg!(target_os = "macos") {
        Ok(Platform::MacOs)
    } else if cfg!(target_os = "linux") {
        Ok(Platform::Linux)
    } else {
        Err(Error::config(
            "daemon management is only supported on macOS and Linux; on Windows, use WSL to run the Linux build and daemon commands",
        ))
    }
}

enum Platform {
    MacOs,
    Linux,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> DaemonInstallOptions {
        DaemonInstallOptions {
            executable: "/usr/local/bin/codexia".into(),
            bind: "127.0.0.1:14550".into(),
            auth_file: Some("/tmp/auth file.json".into()),
            verbosity: 2,
            api_key: Some("local secret".into()),
            provider: None,
            model_fallback: Some("gpt-5.5".into()),
        }
    }

    #[test]
    fn builds_serve_arguments() {
        assert_eq!(
            serve_args(&options()),
            vec![
                "/usr/local/bin/codexia",
                "serve",
                "--bind",
                "127.0.0.1:14550",
                "--auth-file",
                "/tmp/auth file.json",
                "-v",
                "-v",
                "--api-key",
                "local secret",
                "--model-fallback",
                "gpt-5.5",
            ]
        );
    }

    #[test]
    fn launchd_plist_uses_program_arguments_array() {
        let plist = launchd_plist(&options(), Path::new("/tmp/codexia"));

        assert!(plist.contains("<key>ProgramArguments</key>"));
        assert!(plist.contains("<string>/usr/local/bin/codexia</string>"));
        assert!(plist.contains("<string>local secret</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
    }

    #[test]
    fn systemd_unit_quotes_exec_start_arguments() {
        let unit = systemd_unit(&options());

        assert!(unit.contains("ExecStart='/usr/local/bin/codexia' 'serve'"));
        assert!(unit.contains("'local secret'"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn parses_launchctl_print_summary() {
        let summary = parse_launchctl_print_summary(
            r"
                gui/501/com.codexia.daemon = {
                    state = running
                    pid = 12345
                    last exit code = 0
                }
            ",
        );

        assert_eq!(
            summary,
            LaunchctlPrintSummary {
                state: Some("running"),
                pid: Some("12345"),
                last_exit_code: Some("0"),
            }
        );
    }

    #[test]
    fn detects_missing_launchctl_service_errors() {
        assert!(is_launchctl_service_missing(
            "Could not find service \"gui/501/com.codexia.daemon\" in domain for user gui: 501"
        ));
        assert!(is_launchctl_service_missing("No such process"));
        assert!(!is_launchctl_service_missing("Operation not permitted"));
    }

    #[test]
    fn formats_not_installed_launchd_status_panel() {
        let panel = format_launchd_status_panel(&LaunchdStatusView {
            label: LAUNCHD_LABEL,
            plist: Path::new("/tmp/com.codexia.daemon.plist"),
            stdout_log: Path::new("/tmp/codexia.out.log"),
            stderr_log: Path::new("/tmp/codexia.err.log"),
            installed: false,
            loaded: false,
            state: Some("not installed"),
            pid: None,
            last_exit_code: None,
        });

        assert!(panel.contains("Codexia daemon (NOT INSTALLED)"));
        assert!(panel.contains("Installed  no"));
        assert!(panel.contains("run `codexia daemon install`"));
    }

    #[test]
    fn formats_running_launchd_status_panel() {
        let panel = format_launchd_status_panel(&LaunchdStatusView {
            label: LAUNCHD_LABEL,
            plist: Path::new("/tmp/com.codexia.daemon.plist"),
            stdout_log: Path::new("/tmp/codexia.out.log"),
            stderr_log: Path::new("/tmp/codexia.err.log"),
            installed: true,
            loaded: true,
            state: Some("running"),
            pid: Some("12345"),
            last_exit_code: Some("0"),
        });

        assert!(panel.contains("Codexia daemon (RUNNING)"));
        assert!(panel.contains("State  RUNNING"));
        assert!(panel.contains("PID  12345"));
        assert!(panel.contains("tail -n 50 /tmp/codexia.err.log"));
    }
}
