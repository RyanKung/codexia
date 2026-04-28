use crate::{Error, Result};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

const LAUNCHD_LABEL: &str = "com.codexia.daemon";
const SYSTEMD_UNIT: &str = "codexia.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInstallOptions {
    pub executable: PathBuf,
    pub bind: String,
    pub auth_file: Option<PathBuf>,
    pub api_key: Option<String>,
}

pub fn install(options: DaemonInstallOptions) -> Result<()> {
    match platform()? {
        Platform::MacOs => install_launchd(options),
        Platform::Linux => install_systemd(options),
    }
}

pub fn reinstall(options: DaemonInstallOptions) -> Result<()> {
    uninstall()?;
    install(options)
}

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

pub fn stop() -> Result<()> {
    match platform()? {
        Platform::MacOs => {
            let domain = launchd_domain()?;
            run_command("launchctl", ["bootout", &domain, LAUNCHD_LABEL])
        }
        Platform::Linux => systemctl(["stop", SYSTEMD_UNIT]),
    }
}

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

fn install_launchd(options: DaemonInstallOptions) -> Result<()> {
    let plist = launchd_plist_path()?;
    let parent = plist
        .parent()
        .ok_or_else(|| Error::config("launchd plist path has no parent directory"))?;
    fs::create_dir_all(parent)?;

    let log_dir = codexia_home()?;
    fs::create_dir_all(&log_dir)?;
    fs::write(&plist, launchd_plist(&options, &log_dir))?;
    println!("installed {}", plist.display());
    println!("run `codexia daemon start` to start now; launchd will load it on login");
    Ok(())
}

fn install_systemd(options: DaemonInstallOptions) -> Result<()> {
    let unit = systemd_unit_path()?;
    let parent = unit
        .parent()
        .ok_or_else(|| Error::config("systemd unit path has no parent directory"))?;
    fs::create_dir_all(parent)?;

    fs::write(&unit, systemd_unit(&options))?;
    systemctl(["daemon-reload"])?;
    systemctl(["enable", SYSTEMD_UNIT])?;
    println!("installed {}", unit.display());
    println!("run `codexia daemon start` to start now");
    Ok(())
}

fn serve_args(options: &DaemonInstallOptions) -> Vec<String> {
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
    if let Some(api_key) = &options.api_key {
        args.push("--api-key".to_owned());
        args.push(api_key.clone());
    }
    args
}

fn launchd_plist(options: &DaemonInstallOptions, log_dir: &Path) -> String {
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
    let command = serve_args(options)
        .into_iter()
        .map(|arg| systemd_quote(&arg))
        .collect::<Vec<_>>()
        .join(" ");

    format!(
        r#"[Unit]
Description=Codexia OpenAI-compatible Codex OAuth gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={}
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
"#,
        command
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
            "command failed with status {status}: {:?}",
            command
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
            "daemon management is only supported on macOS and Linux",
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
            api_key: Some("local secret".into()),
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
                "--api-key",
                "local secret",
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
}
