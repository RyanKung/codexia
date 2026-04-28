//! `codexio` command-line entrypoint.
//!
//! The binary provides login, serving, token refresh, status inspection, and
//! daemon management commands on top of the library modules exposed by
//! `codexia`.

use clap::{Args, Parser, Subcommand};
use codexia::{
    Error, Result,
    codex::client::CodexClient,
    config::{AppConfig, AppConfigStore, AuthStore, Credentials, now_unix},
    daemon::{self, DaemonInstallOptions},
    models::{ModelOptions, resolve_model_list},
    oauth::{CodexOAuthClient, create_authorization_flow, parse_authorization_input},
    server::{AppState, serve},
    status::StatusClient,
    timefmt::{format_duration, format_status_time_human},
    token::TokenManager,
};
use reqwest::Client;
use std::{
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::PathBuf,
    time::Duration,
};
use tokio::time::{MissedTickBehavior, interval};

const INTERACTIVE_TOKEN_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const LOG_TOKEN_STATUS_INTERVAL: Duration = Duration::from_secs(60);
const CLI_LONG_ABOUT: &str = "\
Codexia is a local OpenAI- and Anthropic-compatible API gateway backed by Codex
OAuth.

It helps clients that speak either the OpenAI Chat Completions API or the
Anthropic Messages API call the Codex backend after you complete the OAuth
login flow. Credentials are stored locally and can be refreshed automatically
during requests or manually with the refresh command/API.";
const CLI_AFTER_LONG_HELP: &str = "\
Examples:
  codexio login
  codexio config
  codexio config show
  codexio serve
  codexio serve --bind 127.0.0.1:14550 --api-key local-secret
  codexio daemon install
  codexio daemon reinstall
  codexio daemon start
  codexio refresh
  codexio status
  curl -X POST http://127.0.0.1:14550/v1/auth/refresh \\
    -H 'authorization: Bearer local-secret'

Environment:
  CODEXIA_API_KEY          Optional local API key for server endpoints
  CODEXIA_AUTH_FILE        Override the credential file path
  CODEXIA_HOME             Override the default config home
  CODEXIA_MODELS           Comma-separated replacement model list
  CODEXIA_EXTRA_MODELS     Comma-separated models appended to defaults
  CODEXIA_MODELS_FILE      JSON file with models/extra_models

Files:
  Credentials default to ~/.codexia/auth.json.
  Runtime config defaults to ~/.codexia/config.json.

Disclaimer:
  Codexia is an unofficial tool and is not affiliated with, endorsed by, or
  supported by OpenAI. Use it at your own risk and make sure your usage complies
  with the terms that apply to your account and the upstream services.

Copyright:
  Copyright (c) 2026 Codexia contributors. Licensed under the GNU Lesser
  General Public License v3.0 only.";

/// Top-level CLI parser.
#[derive(Debug, Parser)]
#[command(
    name = "codexio",
    version,
    about = "OpenAI- and Anthropic-compatible API gateway backed by Codex OAuth",
    long_about = CLI_LONG_ABOUT,
    after_long_help = CLI_AFTER_LONG_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Supported top-level CLI commands.
#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Log in with Codex OAuth and save local credentials",
        long_about = "Start the Codex OAuth login flow, exchange the authorization code for tokens, and save credentials to the configured auth file."
    )]
    Login {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            default_value = "pi",
            value_name = "NAME",
            help = "OAuth originator parameter to send during login"
        )]
        originator: String,
        #[arg(long, help = "Print the login URL without opening a browser")]
        no_browser: bool,
    },
    #[command(
        about = "Manage persisted runtime configuration",
        long_about = "Interactively save or inspect default host, port, backend URL, API key, and model settings stored in the Codexia config file."
    )]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    #[command(
        about = "Serve the OpenAI- and Anthropic-compatible HTTP API",
        long_about = "Serve OpenAI- and Anthropic-compatible endpoints backed by Codex, including /v1/models, /v1/chat/completions, /v1/messages, /v1/messages/count_tokens, and /v1/auth/refresh."
    )]
    Serve {
        #[arg(long, value_name = "ADDR", help = "Socket address to listen on")]
        bind: Option<SocketAddr>,
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(long, value_name = "URL", help = "Codex backend base URL")]
        codex_base_url: Option<String>,
        #[arg(
            long,
            env = "CODEXIA_API_KEY",
            value_name = "KEY",
            help = "Optional local API key accepted as Bearer token or x-api-key"
        )]
        api_key: Option<String>,
        #[arg(
            long,
            env = "CODEXIA_MODELS",
            value_delimiter = ',',
            value_name = "MODEL[,MODEL...]",
            help = "Replace the default model list"
        )]
        models: Vec<String>,
        #[arg(
            long,
            env = "CODEXIA_EXTRA_MODELS",
            value_delimiter = ',',
            value_name = "MODEL[,MODEL...]",
            help = "Append models to the default or configured model list"
        )]
        extra_models: Vec<String>,
        #[arg(
            long,
            env = "CODEXIA_MODELS_FILE",
            value_name = "PATH",
            help = "JSON file containing models and/or extra_models"
        )]
        models_file: Option<PathBuf>,
    },
    #[command(
        about = "Force refresh the saved Codex OAuth token",
        long_about = "Use the saved refresh token to fetch fresh credentials immediately and write them back to the configured auth file."
    )]
    Refresh {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
    },
    #[command(
        about = "Fetch token, account, and rate-limit status",
        long_about = "Refresh credentials if needed, then fetch token expiry, ChatGPT account metadata, and Codex rate-limit windows such as 5h/weekly remaining when available."
    )]
    Status {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            default_value = CodexClient::default_base_url(),
            value_name = "URL",
            help = "ChatGPT backend base URL"
        )]
        codex_base_url: String,
    },
    #[command(
        about = "Install and control the background Codexia service",
        long_about = "Install and control Codexia as a per-user background service. macOS uses launchd LaunchAgents; Linux uses systemd user services."
    )]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

/// Background service management subcommands.
#[derive(Debug, Subcommand)]
enum DaemonCommand {
    #[command(
        about = "Install Codexia as a per-user autostart service",
        long_about = "Write the service definition for the current user and enable autostart. Use `codexio daemon start` to start it immediately."
    )]
    Install(#[command(flatten)] DaemonInstallCliOptions),
    #[command(
        about = "Reinstall Codexia with updated service configuration",
        long_about = "Remove the existing per-user daemon definition if present, then install a fresh one using the provided options and saved runtime config defaults."
    )]
    Reinstall(#[command(flatten)] DaemonInstallCliOptions),
    #[command(about = "Start the installed Codexia daemon")]
    Start,
    #[command(about = "Restart the installed Codexia daemon")]
    Restart,
    #[command(about = "Stop the installed Codexia daemon")]
    Stop,
    #[command(about = "Disable and remove the installed Codexia daemon")]
    Uninstall,
}

/// Runtime configuration management subcommands.
#[derive(Debug, Subcommand)]
enum ConfigCommand {
    #[command(about = "Print the saved runtime configuration as JSON")]
    Show,
    #[command(about = "Delete the saved runtime configuration file")]
    Reset,
}

/// Shared CLI options used by `daemon install` and `daemon reinstall`.
#[derive(Debug, Clone, Args)]
struct DaemonInstallCliOptions {
    #[arg(
        long,
        value_name = "PATH",
        help = "Codexia executable to run; defaults to the current executable"
    )]
    executable: Option<PathBuf>,
    #[arg(
        long,
        value_name = "ADDR",
        help = "Socket address the daemon should listen on"
    )]
    bind: Option<SocketAddr>,
    #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
    auth_file: Option<PathBuf>,
    #[arg(long, value_name = "URL", help = "Codex backend base URL")]
    codex_base_url: Option<String>,
    #[arg(
        long,
        env = "CODEXIA_API_KEY",
        value_name = "KEY",
        help = "Optional local API key accepted as Bearer token or x-api-key"
    )]
    api_key: Option<String>,
    #[arg(
        long,
        env = "CODEXIA_MODELS",
        value_delimiter = ',',
        value_name = "MODEL[,MODEL...]",
        help = "Replace the default model list"
    )]
    models: Vec<String>,
    #[arg(
        long,
        env = "CODEXIA_EXTRA_MODELS",
        value_delimiter = ',',
        value_name = "MODEL[,MODEL...]",
        help = "Append models to the default or configured model list"
    )]
    extra_models: Vec<String>,
    #[arg(
        long,
        env = "CODEXIA_MODELS_FILE",
        value_name = "PATH",
        help = "JSON file containing models and/or extra_models"
    )]
    models_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Login {
            auth_file,
            originator,
            no_browser,
        } => login(auth_store(auth_file)?, &originator, no_browser).await,
        Command::Config { command } => config_command(command),
        Command::Serve {
            bind,
            auth_file,
            codex_base_url,
            api_key,
            models,
            extra_models,
            models_file,
        } => {
            let config = load_app_config()?;
            let effective_bind = bind
                .or_else(|| bind_from_config(config.as_ref()))
                .unwrap_or_else(default_bind);
            let effective_auth_file = auth_file.or_else(|| config_auth_file(config.as_ref()));
            let effective_codex_base_url = codex_base_url
                .or_else(|| config_string(config.as_ref(), |item| item.codex_base_url.clone()))
                .unwrap_or_else(|| CodexClient::default_base_url().to_owned());
            let effective_api_key =
                api_key.or_else(|| config_string(config.as_ref(), |item| item.api_key.clone()));
            let (effective_models, effective_extra_models, effective_models_file) =
                merge_model_options(config.as_ref(), models, extra_models, models_file);
            let http = Client::new();
            let token_manager = TokenManager::new(
                auth_store(effective_auth_file)?,
                CodexOAuthClient::new(http.clone()),
            );
            let codex = CodexClient::new(http, effective_codex_base_url);
            let model_list = resolve_model_list(
                effective_models_file.as_deref(),
                ModelOptions {
                    replacement_models: effective_models,
                    extra_models: effective_extra_models,
                },
            )?;
            println!("listening on http://{effective_bind}");
            spawn_token_expiry_display(token_manager.clone());
            serve(
                effective_bind,
                AppState::new(token_manager, codex, effective_api_key, model_list),
            )
            .await
        }
        Command::Refresh { auth_file } => refresh(auth_store(auth_file)?).await,
        Command::Status {
            auth_file,
            codex_base_url,
        } => status(auth_store(auth_file)?, codex_base_url).await,
        Command::Daemon { command } => daemon_command(command),
    }
}

/// Handles interactive configuration, inspection, and reset commands.
fn config_command(command: Option<ConfigCommand>) -> Result<()> {
    let store = app_config_store()?;
    match command {
        None => configure(store),
        Some(ConfigCommand::Show) => show_config(store),
        Some(ConfigCommand::Reset) => reset_config(store),
    }
}

/// Maps daemon-specific CLI requests to the platform-specific service helpers.
fn daemon_command(command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Install(options) => {
            daemon::install(resolve_daemon_install_options(options)?)
        }
        DaemonCommand::Reinstall(options) => {
            daemon::reinstall(resolve_daemon_install_options(options)?)
        }
        DaemonCommand::Start => daemon::start(),
        DaemonCommand::Restart => daemon::restart(),
        DaemonCommand::Stop => daemon::stop(),
        DaemonCommand::Uninstall => daemon::uninstall(),
    }
}

fn resolve_daemon_install_options(
    options: DaemonInstallCliOptions,
) -> Result<DaemonInstallOptions> {
    let config = load_app_config()?;
    let effective_bind = options
        .bind
        .or_else(|| bind_from_config(config.as_ref()))
        .unwrap_or_else(default_bind);
    let effective_auth_file = options
        .auth_file
        .or_else(|| config_auth_file(config.as_ref()));
    let effective_codex_base_url = options
        .codex_base_url
        .or_else(|| config_string(config.as_ref(), |item| item.codex_base_url.clone()))
        .unwrap_or_else(|| CodexClient::default_base_url().to_owned());
    let effective_api_key = options
        .api_key
        .or_else(|| config_string(config.as_ref(), |item| item.api_key.clone()));
    let (effective_models, effective_extra_models, effective_models_file) = merge_model_options(
        config.as_ref(),
        options.models,
        options.extra_models,
        options.models_file,
    );

    Ok(DaemonInstallOptions {
        executable: options
            .executable
            .map(Ok)
            .unwrap_or_else(std::env::current_exe)?,
        bind: effective_bind.to_string(),
        auth_file: effective_auth_file,
        codex_base_url: effective_codex_base_url,
        api_key: effective_api_key,
        models: effective_models,
        extra_models: effective_extra_models,
        models_file: effective_models_file,
    })
}

/// Resolves the persisted runtime config store path.
fn app_config_store() -> Result<AppConfigStore> {
    AppConfigStore::from_default_path()
}

/// Loads the persisted runtime configuration when present.
fn load_app_config() -> Result<Option<AppConfig>> {
    app_config_store()?.load()
}

/// Interactively prompts for runtime defaults and saves them to disk.
fn configure(store: AppConfigStore) -> Result<()> {
    let existing = store.load()?.unwrap_or_default();
    let bind_host = prompt_string(
        "Bind host",
        existing.bind_host.as_deref().unwrap_or("127.0.0.1"),
    )?;
    let bind_port = prompt_port("Bind port", existing.bind_port.unwrap_or(14550))?;
    let codex_base_url = prompt_string(
        "Codex backend base URL",
        existing
            .codex_base_url
            .as_deref()
            .unwrap_or(CodexClient::default_base_url()),
    )?;
    let api_key = prompt_optional_string(
        "Local API key (leave blank to disable)",
        existing.api_key.as_deref(),
    )?;
    let auth_file = prompt_optional_path(
        "Credential file path (leave blank for default ~/.codexia/auth.json)",
        existing.auth_file.as_deref(),
    )?;
    let models = prompt_csv_list(
        "Replacement models (comma-separated, leave blank to use defaults)",
        &existing.models,
    )?;
    let extra_models = prompt_csv_list(
        "Extra models (comma-separated, leave blank for none)",
        &existing.extra_models,
    )?;
    let models_file = prompt_optional_path(
        "Models JSON file (leave blank to disable)",
        existing.models_file.as_deref(),
    )?;

    let config = AppConfig {
        bind_host: Some(bind_host),
        bind_port: Some(bind_port),
        auth_file,
        codex_base_url: Some(codex_base_url),
        api_key,
        models,
        extra_models,
        models_file,
    };
    store.save(&config)?;
    println!("saved runtime config to {}", store.path().display());
    Ok(())
}

fn show_config(store: AppConfigStore) -> Result<()> {
    match store.load()? {
        Some(config) => {
            println!("{}", serde_json::to_string_pretty(&config)?);
            Ok(())
        }
        None => Err(Error::config(format!(
            "no runtime config found at {}; run `codexio config` first",
            store.path().display()
        ))),
    }
}

fn reset_config(store: AppConfigStore) -> Result<()> {
    store.delete()?;
    println!("removed runtime config at {}", store.path().display());
    Ok(())
}

/// Runs the interactive OAuth login flow and persists the resulting credentials.
async fn login(store: AuthStore, originator: &str, no_browser: bool) -> Result<()> {
    let flow = create_authorization_flow(originator)?;
    println!("Open this URL to authenticate:\n{}\n", flow.authorize_url);
    println!(
        "After login, your browser may fail to load the localhost callback. Copy the full address from the browser address bar and paste it here."
    );

    if !no_browser {
        let _ = webbrowser::open(flow.authorize_url.as_str());
    }

    let code = prompt_authorization_code(&flow.state)?;
    let credentials = CodexOAuthClient::default()
        .exchange_authorization_code(&code, &flow.verifier)
        .await?;
    store.save(&credentials)?;
    println!(
        "logged in account {} and saved credentials to {}",
        credentials.account_id,
        store.path().display()
    );
    Ok(())
}

/// Forces a refresh of the saved OAuth credentials and writes them back to disk.
async fn refresh(store: AuthStore) -> Result<()> {
    let credentials = store
        .load()?
        .ok_or_else(|| Error::config("not logged in; run `codexio login` first"))?;
    let refreshed = CodexOAuthClient::default()
        .refresh_token(&credentials.refresh_token)
        .await?;
    store.save(&refreshed)?;
    println!("refreshed account {}", refreshed.account_id);
    Ok(())
}

/// Fetches token, plan, and rate-limit status and renders a human-readable report.
async fn status(store: AuthStore, codex_base_url: String) -> Result<()> {
    let http = Client::new();
    let token_manager = TokenManager::new(store, CodexOAuthClient::new(http.clone()));
    let credentials = token_manager.credentials().await?;
    let snapshot = StatusClient::new(http, codex_base_url)
        .fetch_status(&credentials)
        .await;

    println!("account_id: {}", credentials.account_id);
    println!("token: {}", token_expiry_message(&credentials));

    if let Some(account) = snapshot.account {
        if let Some(email) = account.email {
            println!("email: {email}");
        }
        if let Some(plan) = account.plan {
            match account.has_active_subscription {
                Some(active) => println!("plan: {plan} (active: {active})"),
                None => println!("plan: {plan}"),
            }
        }
        if let Some(structure) = account.structure {
            println!("account_structure: {structure}");
        }
        if let Some(name) = account.name {
            println!("account_name: {name}");
        }
        if let Some(expires_at) = account.subscription_expires_at {
            println!(
                "subscription_expires_at: {}",
                format_status_time_human(&expires_at)
            );
        }
    }

    if let Some(balance) = snapshot.credits_balance {
        println!("credits_balance: {balance}");
    }

    if snapshot.rate_limits.is_empty() {
        println!("rate_limits: unavailable");
    } else {
        for window in snapshot.rate_limits {
            let mut line = format!(
                "rate_limit_{}: {:.0}% remaining",
                window.name, window.remaining_percent
            );
            if let Some(reset_at) = window.reset_at {
                line.push_str(&format!(", resets {}", format_status_time_human(&reset_at)));
            }
            println!("{line}");
        }
    }

    for warning in snapshot.warnings {
        println!("warning: {warning}");
    }

    Ok(())
}

/// Resolves the configured credential store path.
fn auth_store(path: Option<PathBuf>) -> Result<AuthStore> {
    path.map(AuthStore::new)
        .map(Ok)
        .unwrap_or_else(AuthStore::from_default_path)
}

fn default_bind() -> SocketAddr {
    "127.0.0.1:14550"
        .parse()
        .expect("hardcoded default bind address should parse")
}

fn bind_from_config(config: Option<&AppConfig>) -> Option<SocketAddr> {
    let config = config?;
    let host = config.bind_host.as_deref()?;
    let port = config.bind_port?;
    format!("{host}:{port}").parse().ok()
}

fn config_auth_file(config: Option<&AppConfig>) -> Option<PathBuf> {
    config.and_then(|item| item.auth_file.clone())
}

fn config_string(
    config: Option<&AppConfig>,
    map: impl FnOnce(&AppConfig) -> Option<String>,
) -> Option<String> {
    config.and_then(map)
}

fn merge_model_options(
    config: Option<&AppConfig>,
    models: Vec<String>,
    extra_models: Vec<String>,
    models_file: Option<PathBuf>,
) -> (Vec<String>, Vec<String>, Option<PathBuf>) {
    let replacement_models = if models.is_empty() {
        config.map(|item| item.models.clone()).unwrap_or_default()
    } else {
        models
    };
    let effective_extra_models = if extra_models.is_empty() {
        config
            .map(|item| item.extra_models.clone())
            .unwrap_or_default()
    } else {
        extra_models
    };
    let effective_models_file =
        models_file.or_else(|| config.and_then(|item| item.models_file.clone()));

    (
        replacement_models,
        effective_extra_models,
        effective_models_file,
    )
}

fn prompt_string(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    Ok(if value.is_empty() {
        default.to_owned()
    } else {
        value.to_owned()
    })
}

fn prompt_optional_string(label: &str, default: Option<&str>) -> Result<Option<String>> {
    let suffix = default.map(|item| format!(" [{item}]")).unwrap_or_default();
    print!("{label}{suffix}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    if value.is_empty() {
        Ok(default.map(str::to_owned).filter(|item| !item.is_empty()))
    } else {
        Ok(Some(value.to_owned()))
    }
}

fn prompt_optional_path(label: &str, default: Option<&std::path::Path>) -> Result<Option<PathBuf>> {
    let suffix = default
        .map(|item| format!(" [{}]", item.display()))
        .unwrap_or_default();
    print!("{label}{suffix}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    if value.is_empty() {
        Ok(default.map(ToOwned::to_owned))
    } else {
        Ok(Some(PathBuf::from(value)))
    }
}

fn prompt_csv_list(label: &str, default: &[String]) -> Result<Vec<String>> {
    let default_text = if default.is_empty() {
        String::new()
    } else {
        format!(" [{}]", default.join(","))
    };
    print!("{label}{default_text}: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    let source = if value.is_empty() {
        default.to_vec()
    } else {
        value.split(',').map(str::to_owned).collect()
    };
    Ok(source
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect())
}

fn prompt_port(label: &str, default: u16) -> Result<u16> {
    let value = prompt_string(label, &default.to_string())?;
    value
        .parse::<u16>()
        .map_err(|_| Error::config(format!("invalid port: {value}")))
}

/// Reads the pasted OAuth callback URL or raw authorization code from stdin.
fn prompt_authorization_code(expected_state: &str) -> Result<String> {
    print!("Paste the full redirect URL or authorization code: ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let parsed = parse_authorization_input(&input);
    if parsed
        .state
        .as_deref()
        .is_some_and(|state| state != expected_state)
    {
        return Err(Error::oauth("state mismatch"));
    }

    parsed
        .code
        .ok_or_else(|| Error::oauth("missing authorization code"))
}

/// Periodically prints token expiry state while the local server is running.
fn spawn_token_expiry_display(token_manager: TokenManager) {
    tokio::spawn(async move {
        let interactive = io::stdout().is_terminal();
        let mut ticker = interval(if interactive {
            INTERACTIVE_TOKEN_STATUS_INTERVAL
        } else {
            LOG_TOKEN_STATUS_INTERVAL
        });
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            let status = token_expiry_status(&token_manager).await;
            if interactive {
                print!("\r\x1b[2K{status}");
                let _ = io::stdout().flush();
            } else {
                println!("{status}");
            }
        }
    });
}

/// Builds a one-line token expiry status string for the current credentials snapshot.
async fn token_expiry_status(token_manager: &TokenManager) -> String {
    match token_manager.credentials_snapshot().await {
        Some(credentials) => token_expiry_message(&credentials),
        None => "token status unavailable: not logged in; run `codexio login` first".to_owned(),
    }
}

/// Renders a human-readable expiry message for one credential set.
fn token_expiry_message(credentials: &Credentials) -> String {
    let remaining_secs = credentials.expires_at.saturating_sub(now_unix());
    if remaining_secs == 0 {
        format!("token expired (account {})", credentials.account_id)
    } else {
        format!(
            "token expires in {} (account {})",
            format_duration(remaining_secs),
            credentials.account_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, bind_from_config, merge_model_options};
    use codexia::timefmt::format_duration;
    use std::path::PathBuf;

    #[test]
    fn reuses_shared_duration_formatting() {
        assert_eq!(format_duration(90_061), "1d 01h 01m 01s");
    }

    #[test]
    fn builds_bind_address_from_config() {
        let config = AppConfig {
            bind_host: Some("127.0.0.1".into()),
            bind_port: Some(14550),
            ..AppConfig::default()
        };

        assert_eq!(
            bind_from_config(Some(&config)).map(|item| item.to_string()),
            Some("127.0.0.1:14550".to_owned())
        );
    }

    #[test]
    fn cli_model_values_override_config_models() {
        let config = AppConfig {
            models: vec!["from-config".into()],
            extra_models: vec!["config-extra".into()],
            models_file: Some(PathBuf::from("/tmp/config-models.json")),
            ..AppConfig::default()
        };

        let (models, extra_models, models_file) = merge_model_options(
            Some(&config),
            vec!["from-cli".into()],
            vec!["cli-extra".into()],
            Some(PathBuf::from("/tmp/cli-models.json")),
        );

        assert_eq!(models, vec!["from-cli"]);
        assert_eq!(extra_models, vec!["cli-extra"]);
        assert_eq!(models_file, Some(PathBuf::from("/tmp/cli-models.json")));
    }

    #[test]
    fn config_models_fill_missing_cli_model_values() {
        let config = AppConfig {
            models: vec!["from-config".into()],
            extra_models: vec!["config-extra".into()],
            models_file: Some(PathBuf::from("/tmp/config-models.json")),
            ..AppConfig::default()
        };

        let (models, extra_models, models_file) =
            merge_model_options(Some(&config), Vec::new(), Vec::new(), None);

        assert_eq!(models, vec!["from-config"]);
        assert_eq!(extra_models, vec!["config-extra"]);
        assert_eq!(models_file, Some(PathBuf::from("/tmp/config-models.json")));
    }
}
