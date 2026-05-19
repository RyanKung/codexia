#![deny(missing_docs)]
#![deny(clippy::all)]
#![deny(clippy::pedantic)]
#![deny(clippy::nursery)]

//! `codexia` command-line entrypoint.
//!
//! The binary provides login, serving, token refresh, status inspection, and
//! daemon management commands on top of the library modules exposed by
//! `codexia`.

use clap::{ArgAction, Args, Parser, Subcommand};
use codexia::{
    Error, Result,
    codex::client::CodexClient,
    config::{AppConfig, AppConfigStore, AuthStore, Credentials, Provider, now_unix},
    daemon::{self, DaemonInstallOptions},
    logging::{self, LogLevel},
    models::resolve_model_list_for_providers,
    oauth::{
        CodexOAuthClient, GrokOAuthClient, create_authorization_flow, parse_authorization_input,
    },
    server::{AppState, UpstreamState, serve},
    status::StatusClient,
    timefmt::{format_duration, format_status_time_human},
    token::TokenManager,
};
use reqwest::Client;
use std::{
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::PathBuf,
    process::Command as ProcessCommand,
    time::Duration,
};
use tokio::time::{MissedTickBehavior, interval};

const INTERACTIVE_TOKEN_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const LOG_TOKEN_STATUS_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_MODEL_FALLBACK: &str = "gpt-5.5";
const CLI_LONG_ABOUT: &str = "\
Codexia is a local OpenAI- and Anthropic-compatible API gateway backed by Codex
or Grok OAuth.

It helps clients that speak either the OpenAI Chat Completions API or the
Anthropic Messages API call the selected upstream after you complete the OAuth
login flow. Credentials are stored locally and can be refreshed automatically
during requests or manually with the refresh command/API.";
const CLI_AFTER_LONG_HELP: &str = "\
Examples:
  codexia login
  codexia login --provider grok
  codexia config
  codexia config show
  codexia serve
  codexia serve --bind 127.0.0.1:14550 --api-key local-secret
  codexia daemon install
  codexia daemon reinstall
  codexia daemon start
  codexia daemon status
  codexia refresh
  codexia status
  codexia update
  curl -X POST http://127.0.0.1:14550/v1/auth/refresh \\
    -H 'authorization: Bearer local-secret'

Environment:
  CODEXIA_API_KEY          Optional local API key for server endpoints
  CODEXIA_PROVIDER         Upstream provider: codex or grok
  CODEXIA_MODEL_FALLBACK   Fallback for unsupported Anthropic model ids
  CODEXIA_AUTH_FILE        Override the credential file path
  CODEXIA_HOME             Override the default config home
Files:
  Credentials default to ~/.codexia/auth.json.
  Runtime config defaults to ~/.codexia/config.json.

Disclaimer:
  Codexia is an unofficial tool and is not affiliated with, endorsed by, or
  supported by OpenAI, Anthropic, or xAI. Use it at your own risk, make sure your
  usage complies with the terms that apply to your account and the upstream
  services, and do not assume the LGPLv3 license overrides upstream account
  restrictions on sharing or reselling personal OAuth-backed access.

Copyright:
  Copyright (c) 2026 Codexia contributors. Licensed under the GNU Lesser
  General Public License v3.0 only.";

/// Top-level CLI parser.
#[derive(Debug, Parser)]
#[command(
    name = "codexia",
    version,
    about = "OpenAI- and Anthropic-compatible API gateway backed by OAuth providers",
    long_about = CLI_LONG_ABOUT,
    after_long_help = CLI_AFTER_LONG_HELP
)]
struct Cli {
    #[arg(
        short = 'v',
        long = "verbose",
        global = true,
        action = ArgAction::Count,
        help = "Increase logging verbosity: -v for request summaries, -vv/-vvv for full tracing"
    )]
    verbose: u8,
    #[command(subcommand)]
    command: Command,
}

/// Supported top-level CLI commands.
#[derive(Debug, Subcommand)]
enum Command {
    #[command(
        about = "Log in with an OAuth provider and save local credentials",
        long_about = "Start the selected OAuth login flow, exchange the authorization code for tokens, and save credentials to the configured auth file."
    )]
    Login {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            env = "CODEXIA_PROVIDER",
            default_value = "codex",
            value_name = "PROVIDER",
            help = "OAuth provider to authenticate: codex or grok"
        )]
        provider: String,
        #[arg(
            long,
            default_value = "pi",
            value_name = "NAME",
            help = "OAuth originator parameter to send during login"
        )]
        originator: String,
    },
    #[command(
        about = "Manage persisted runtime configuration",
        long_about = "Interactively save or inspect default host, port, and API key stored in the Codexia config file."
    )]
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    #[command(
        about = "Serve the OpenAI- and Anthropic-compatible HTTP API",
        long_about = "Serve OpenAI- and Anthropic-compatible endpoints backed by the selected provider, including /v1/models, /v1/chat/completions, /v1/responses, /v1/responses/compact, /v1/messages, /v1/messages/count_tokens, /v1/messages/batches, and /v1/auth/refresh."
    )]
    Serve {
        #[arg(long, value_name = "ADDR", help = "Socket address to listen on")]
        bind: Option<SocketAddr>,
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            env = "CODEXIA_API_KEY",
            value_name = "KEY",
            help = "Optional local API key accepted as Bearer token or x-api-key"
        )]
        api_key: Option<String>,
        #[arg(
            long,
            env = "CODEXIA_PROVIDER",
            value_name = "PROVIDER",
            help = "Upstream provider to serve: codex or grok"
        )]
        provider: Option<String>,
        #[arg(
            long,
            env = "CODEXIA_MODEL_FALLBACK",
            value_name = "MODEL",
            help = "Fallback model for unsupported Anthropic model ids such as claude-sonnet-*"
        )]
        model_fallback: Option<String>,
    },
    #[command(
        about = "Force refresh the saved OAuth token",
        long_about = "Use the saved refresh token to fetch fresh credentials immediately and write them back to the configured auth file."
    )]
    Refresh {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
        #[arg(
            long,
            env = "CODEXIA_PROVIDER",
            value_name = "PROVIDER",
            help = "OAuth provider to refresh: codex or grok. When omitted, refreshes all saved providers."
        )]
        provider: Option<String>,
    },
    #[command(
        about = "Fetch token, account, and rate-limit status",
        long_about = "Refresh credentials if needed, then fetch token expiry, ChatGPT account metadata, and Codex rate-limit windows such as 5h/weekly remaining when available."
    )]
    Status {
        #[arg(long, value_name = "PATH", help = "Credential file to read/write")]
        auth_file: Option<PathBuf>,
    },
    #[command(
        about = "Install the latest Codexia release with cargo",
        long_about = "Run `cargo install --locked --force codexia` so the currently installed Codexia binary is updated to the latest published release. Requires `cargo` to be available on PATH."
    )]
    Update {
        #[arg(
            long,
            value_name = "VERSION",
            help = "Install a specific published version instead of the latest release"
        )]
        version: Option<String>,
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
        long_about = "Write the service definition for the current user and enable autostart. Use `codexia daemon start` to start it immediately."
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
    #[command(about = "Show the installed Codexia daemon status")]
    Status,
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
    #[arg(
        long,
        env = "CODEXIA_API_KEY",
        value_name = "KEY",
        help = "Optional local API key accepted as Bearer token or x-api-key"
    )]
    api_key: Option<String>,
    #[arg(
        long,
        env = "CODEXIA_PROVIDER",
        value_name = "PROVIDER",
        help = "Upstream provider to serve: codex or grok"
    )]
    provider: Option<String>,
    #[arg(
        long,
        env = "CODEXIA_MODEL_FALLBACK",
        value_name = "MODEL",
        help = "Fallback model for unsupported Anthropic model ids such as claude-sonnet-*"
    )]
    model_fallback: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    let log_level = LogLevel::from_verbosity(cli.verbose);
    match cli.command {
        Command::Login {
            auth_file,
            provider,
            originator,
        } => {
            login(
                auth_store(auth_file)?,
                resolve_provider(Some(provider), None)?,
                &originator,
            )
            .await
        }
        Command::Config { command } => config_command(command.as_ref()),
        Command::Serve {
            bind,
            auth_file,
            api_key,
            provider,
            model_fallback,
        } => {
            logging::init(log_level)?;
            let config = load_app_config()?;
            let effective_bind = bind
                .or_else(|| bind_from_config(config.as_ref()))
                .unwrap_or_else(default_bind);
            let effective_auth_file = auth_file.or_else(|| config_auth_file(config.as_ref()));
            let effective_api_key =
                api_key.or_else(|| config_string(config.as_ref(), |item| item.api_key.clone()));
            let effective_model_fallback = resolve_model_fallback(model_fallback, config.as_ref());
            let http = Client::new();
            let store = auth_store(effective_auth_file)?;
            let providers = resolve_served_providers(&store, provider, config.as_ref())?;
            let mut upstreams = Vec::new();
            for provider in &providers {
                let token_manager =
                    TokenManager::new_for_provider(store.clone(), *provider, http.clone());
                token_manager.credentials().await?;
                upstreams.push(UpstreamState {
                    provider: *provider,
                    token_manager,
                    client: CodexClient::new_for_provider(http.clone(), *provider),
                });
            }
            let model_list = resolve_model_list_for_providers(&providers)?;
            println!("listening on http://{effective_bind}");
            for upstream in &upstreams {
                spawn_token_expiry_display(upstream.token_manager.clone());
            }
            serve(
                effective_bind,
                AppState::new_multi_with_model_fallback(
                    upstreams,
                    effective_api_key,
                    model_list,
                    effective_model_fallback,
                ),
            )
            .await
        }
        Command::Refresh {
            auth_file,
            provider,
        } => refresh(auth_store(auth_file)?, provider).await,
        Command::Status { auth_file } => status(auth_store(auth_file)?).await,
        Command::Update { version } => update(version.as_deref()),
        Command::Daemon { command } => daemon_command(command, cli.verbose),
    }
}

/// Reinstalls Codexia from crates.io through Cargo.
fn update(version: Option<&str>) -> Result<()> {
    let mut command = build_update_command(version);
    println!("running {command:?}");
    let status = command.status()?;
    if status.success() {
        match version {
            Some(version) => println!("updated codexia to version {version}"),
            None => println!("updated codexia to the latest published release"),
        }
        Ok(())
    } else {
        Err(Error::upstream(format!(
            "cargo install exited with status {status}"
        )))
    }
}

/// Builds the Cargo command used by `codexia update`.
#[must_use]
fn build_update_command(version: Option<&str>) -> ProcessCommand {
    let mut command = ProcessCommand::new("cargo");
    command.args(["install", "--locked", "--force", "codexia"]);
    if let Some(version) = version {
        command.args(["--version", version]);
    }
    command
}

/// Handles interactive configuration, inspection, and reset commands.
fn config_command(command: Option<&ConfigCommand>) -> Result<()> {
    let store = app_config_store()?;
    match command {
        None => configure(&store),
        Some(ConfigCommand::Show) => show_config(&store),
        Some(ConfigCommand::Reset) => reset_config(&store),
    }
}

/// Maps daemon-specific CLI requests to the platform-specific service helpers.
fn daemon_command(command: DaemonCommand, verbosity: u8) -> Result<()> {
    match command {
        DaemonCommand::Install(options) => {
            daemon::install(&resolve_daemon_install_options(options, verbosity)?)
        }
        DaemonCommand::Reinstall(options) => {
            daemon::reinstall(&resolve_daemon_install_options(options, verbosity)?)
        }
        DaemonCommand::Start => daemon::start(),
        DaemonCommand::Restart => daemon::restart(),
        DaemonCommand::Status => daemon::status(),
        DaemonCommand::Stop => daemon::stop(),
        DaemonCommand::Uninstall => daemon::uninstall(),
    }
}

fn resolve_daemon_install_options(
    options: DaemonInstallCliOptions,
    verbosity: u8,
) -> Result<DaemonInstallOptions> {
    let config = load_app_config()?;
    let effective_bind = options
        .bind
        .or_else(|| bind_from_config(config.as_ref()))
        .unwrap_or_else(default_bind);
    let effective_auth_file = options
        .auth_file
        .or_else(|| config_auth_file(config.as_ref()));
    let effective_api_key = options
        .api_key
        .or_else(|| config_string(config.as_ref(), |item| item.api_key.clone()));
    let effective_provider = options
        .provider
        .map(|provider| provider.parse::<Provider>())
        .transpose()?;
    let effective_model_fallback = resolve_model_fallback(options.model_fallback, config.as_ref());
    Ok(DaemonInstallOptions {
        executable: options.executable.map_or_else(std::env::current_exe, Ok)?,
        bind: effective_bind.to_string(),
        auth_file: effective_auth_file,
        verbosity,
        api_key: effective_api_key,
        provider: effective_provider,
        model_fallback: effective_model_fallback,
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
fn configure(store: &AppConfigStore) -> Result<()> {
    let existing = store.load()?.unwrap_or_default();
    let bind_host = prompt_string(
        "Bind host",
        existing.bind_host.as_deref().unwrap_or("127.0.0.1"),
    )?;
    let bind_port = prompt_port("Bind port", existing.bind_port.unwrap_or(14550))?;
    let api_key = prompt_optional_string(
        "Local API key (leave blank to disable)",
        existing.api_key.as_deref(),
    )?;
    let auth_file = prompt_optional_path(
        "Credential file path (leave blank for default ~/.codexia/auth.json)",
        existing.auth_file.as_deref(),
    )?;
    let provider = prompt_string("Provider", existing.provider.unwrap_or_default().as_str())?
        .parse::<Provider>()?;
    let model_fallback = prompt_optional_string(
        "Fallback model for unsupported Anthropic ids (leave blank for default gpt-5.5)",
        existing
            .model_fallback
            .as_deref()
            .or(Some(DEFAULT_MODEL_FALLBACK)),
    )?;

    let config = AppConfig {
        bind_host: Some(bind_host),
        bind_port: Some(bind_port),
        auth_file,
        api_key,
        model_fallback,
        provider: Some(provider),
    };
    store.save(&config)?;
    println!("saved runtime config to {}", store.path().display());
    Ok(())
}

fn show_config(store: &AppConfigStore) -> Result<()> {
    match store.load()? {
        Some(config) => {
            println!("{}", serde_json::to_string_pretty(&config)?);
            Ok(())
        }
        None => Err(Error::config(format!(
            "no runtime config found at {}; run `codexia config` first",
            store.path().display()
        ))),
    }
}

fn resolve_model_fallback(
    cli_or_option: Option<String>,
    config: Option<&AppConfig>,
) -> Option<String> {
    cli_or_option
        .or_else(|| config_string(config, |item| item.model_fallback.clone()))
        .or_else(|| Some(DEFAULT_MODEL_FALLBACK.to_owned()))
}

fn resolve_provider(cli_or_option: Option<String>, config: Option<&AppConfig>) -> Result<Provider> {
    cli_or_option.map_or_else(
        || Ok(config.and_then(|item| item.provider).unwrap_or_default()),
        |value| value.parse(),
    )
}

fn resolve_served_providers(
    store: &AuthStore,
    cli_or_option: Option<String>,
    config: Option<&AppConfig>,
) -> Result<Vec<Provider>> {
    if let Some(value) = cli_or_option {
        return Ok(vec![value.parse()?]);
    }

    let mut providers = store
        .load_all()?
        .into_iter()
        .map(|credentials| credentials.provider)
        .collect::<Vec<_>>();
    if providers.is_empty() {
        providers.push(config.and_then(|item| item.provider).unwrap_or_default());
    }
    providers.sort_unstable();
    providers.dedup();
    Ok(providers)
}

fn reset_config(store: &AppConfigStore) -> Result<()> {
    store.delete()?;
    println!("removed runtime config at {}", store.path().display());
    Ok(())
}

/// Runs the interactive OAuth login flow and persists the resulting credentials.
async fn login(store: AuthStore, provider: Provider, originator: &str) -> Result<()> {
    let http = Client::new();
    let flow = match provider {
        Provider::Codex => create_authorization_flow(originator)?,
        Provider::Grok => {
            GrokOAuthClient::default()
                .create_authorization_flow()
                .await?
        }
    };
    println!(
        "Open this URL to authenticate with {}:\n{}\n",
        provider.display_name(),
        flow.authorize_url
    );
    println!(
        "After login, your browser may fail to load the localhost callback. Copy the full address from the browser address bar and paste it here."
    );

    let code = prompt_authorization_code(&flow.state)?;
    let credentials = match provider {
        Provider::Codex => {
            CodexOAuthClient::new(http)
                .exchange_authorization_code(&code, &flow.verifier)
                .await?
        }
        Provider::Grok => {
            GrokOAuthClient::default()
                .exchange_authorization_code(&code, &flow.verifier)
                .await?
        }
    };
    store.save(&credentials)?;
    let subject = credential_subject(&credentials);
    println!(
        "logged in {subject} and saved credentials to {}",
        store.path().display()
    );
    Ok(())
}

/// Forces a refresh of the saved OAuth credentials and writes them back to disk.
async fn refresh(store: AuthStore, provider: Option<String>) -> Result<()> {
    let credentials = if let Some(provider) = provider {
        let provider = provider.parse::<Provider>()?;
        store.load_provider(provider)?.ok_or_else(|| {
            Error::config(format!(
                "not logged in for {provider}; run `codexia login --provider {provider}` first"
            ))
        })?
    } else {
        let all = store.load_all()?;
        if all.is_empty() {
            return Err(Error::config("not logged in; run `codexia login` first"));
        }
        for credentials in all {
            let refreshed = refresh_credentials(&credentials).await?;
            store.save(&refreshed)?;
            println!("refreshed {}", credential_subject(&refreshed));
        }
        return Ok(());
    };

    let refreshed = refresh_credentials(&credentials).await?;
    store.save(&refreshed)?;
    println!("refreshed {}", credential_subject(&refreshed));
    Ok(())
}

async fn refresh_credentials(credentials: &Credentials) -> Result<Credentials> {
    match credentials.provider {
        Provider::Codex => {
            CodexOAuthClient::default()
                .refresh_token(&credentials.refresh_token)
                .await
        }
        Provider::Grok => {
            GrokOAuthClient::default()
                .refresh_token(&credentials.refresh_token)
                .await
        }
    }
}

/// Fetches token, plan, and rate-limit status and renders a human-readable report.
async fn status(store: AuthStore) -> Result<()> {
    let http = Client::new();
    let stored = store
        .load()?
        .ok_or_else(|| Error::config("not logged in; run `codexia login` first"))?;
    let provider = stored.provider;
    let token_manager = TokenManager::new_for_provider(store, provider, http.clone());
    let credentials = token_manager.credentials().await?;
    println!("provider: {}", credentials.provider);
    println!("token: {}", token_expiry_message(&credentials));
    if provider == Provider::Grok {
        println!("status: Grok account and rate-limit status are not available yet");
        return Ok(());
    }
    let snapshot = StatusClient::new(http, CodexClient::default_base_url())
        .fetch_status(&credentials)
        .await;

    println!("account_id: {}", credentials.account_id);

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
                use std::fmt::Write as _;
                let _ = write!(line, ", resets {}", format_status_time_human(&reset_at));
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
        .map_or_else(AuthStore::from_default_path, Ok)
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
        Some(credentials) if credentials.expires_at > now_unix() => {
            token_expiry_message(&credentials)
        }
        Some(_) => token_manager.credentials().await.map_or_else(
            |error| format!("token refresh failed: {error}"),
            |credentials| token_expiry_message(&credentials),
        ),
        None => "token status unavailable: not logged in; run `codexia login` first".to_owned(),
    }
}

/// Renders a human-readable expiry message for one credential set.
fn token_expiry_message(credentials: &Credentials) -> String {
    let remaining_secs = credentials.expires_at.saturating_sub(now_unix());
    let subject = credential_subject(credentials);
    if remaining_secs == 0 {
        format!("token expired ({subject})")
    } else {
        format!(
            "token expires in {} ({subject})",
            format_duration(remaining_secs),
        )
    }
}

fn credential_subject(credentials: &Credentials) -> String {
    if credentials.account_id.is_empty() {
        format!("{} credentials", credentials.provider.display_name())
    } else {
        format!(
            "{} account {}",
            credentials.provider.display_name(),
            credentials.account_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, DEFAULT_MODEL_FALLBACK, bind_from_config, build_update_command,
        resolve_model_fallback,
    };
    use codexia::timefmt::format_duration;

    #[test]
    fn reuses_shared_duration_formatting() {
        assert_eq!(format_duration(90_061), "1d 01h 01m 01s");
    }

    #[test]
    fn builds_bind_address_from_config() {
        let config = AppConfig {
            bind_host: Some("127.0.0.1".into()),
            bind_port: Some(14550),
            model_fallback: None,
            ..AppConfig::default()
        };

        assert_eq!(
            bind_from_config(Some(&config)).map(|item| item.to_string()),
            Some("127.0.0.1:14550".to_owned())
        );
    }

    #[test]
    fn builds_update_command_for_latest_release() {
        let command = build_update_command(None);
        let args = command
            .get_args()
            .map(|item| item.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(args, ["install", "--locked", "--force", "codexia"]);
    }

    #[test]
    fn builds_update_command_for_specific_version() {
        let command = build_update_command(Some("0.3.3"));
        let args = command
            .get_args()
            .map(|item| item.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            [
                "install",
                "--locked",
                "--force",
                "codexia",
                "--version",
                "0.3.3"
            ]
        );
    }

    #[test]
    fn uses_default_model_fallback_when_unset() {
        assert_eq!(
            resolve_model_fallback(None, None),
            Some(DEFAULT_MODEL_FALLBACK.to_owned())
        );
    }

    #[test]
    fn prefers_explicit_model_fallback_over_default() {
        let config = AppConfig {
            model_fallback: Some("gpt-5.4".into()),
            ..AppConfig::default()
        };

        assert_eq!(
            resolve_model_fallback(Some("gpt-5.3-codex".into()), Some(&config)),
            Some("gpt-5.3-codex".into())
        );
        assert_eq!(
            resolve_model_fallback(None, Some(&config)),
            Some("gpt-5.4".into())
        );
    }
}
