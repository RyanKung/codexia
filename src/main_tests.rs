use super::{
    AppConfig, DEFAULT_MODEL_FALLBACK, bind_from_config, build_update_command, credential_subject,
    daemon_endpoint_lines, format_login_provider_choice, format_models,
    new_provider_daemon_restart_hint, parse_login_provider_choice, resolve_model_fallback,
    resolve_status_providers, token_expiry_message,
};
use rotom::config::{Credentials, Provider, now_unix};
use rotom::timefmt::format_duration;

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

    assert_eq!(args, ["install", "--locked", "--force", "rotom"]);
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
            "rotom",
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

#[test]
fn formats_models_grouped_by_provider() {
    let output = format_models(&[Provider::Codex, Provider::Grok]);

    assert!(output.contains("OpenAI (codex)\n  gpt-5.1"));
    assert!(output.contains("Grok (grok)\n  grok-4.3"));
    assert!(output.contains("\n\nGrok (grok)"));
}

#[test]
fn formats_single_provider_models() {
    let output = format_models(&[Provider::Grok]);

    assert!(output.starts_with("Grok (grok)\n"));
    assert!(output.contains("  grok-4\n"));
    assert!(!output.contains("gpt-5.5"));
}

#[test]
fn parses_login_provider_choices() {
    assert_eq!(parse_login_provider_choice("").unwrap(), Provider::Codex);
    assert_eq!(parse_login_provider_choice("1").unwrap(), Provider::Codex);
    assert_eq!(parse_login_provider_choice("2").unwrap(), Provider::Grok);
    assert_eq!(
        parse_login_provider_choice("openai").unwrap(),
        Provider::Codex
    );
    assert_eq!(parse_login_provider_choice("grok").unwrap(), Provider::Grok);
}

#[test]
fn formats_login_provider_choice_with_status() {
    let credentials = vec![Credentials {
        provider: Provider::Codex,
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        expires_at: now_unix() + 90,
        account_id: "account".into(),
    }];

    let openai = format_login_provider_choice(Provider::Codex, &credentials);
    let grok = format_login_provider_choice(Provider::Grok, &credentials);

    assert!(openai.starts_with("openai (logged in, expires in "));
    assert_eq!(grok, "grok");
}

#[test]
fn credential_subject_does_not_include_account_id() {
    let credentials = Credentials {
        provider: Provider::Codex,
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        expires_at: now_unix() + 90,
        account_id: "account-secret".into(),
    };

    assert_eq!(credential_subject(&credentials), "Codex credentials");
    let token_message = token_expiry_message(&credentials);
    assert!(token_message.contains("(Codex credentials)"));
    assert!(!token_message.contains("account-secret"));
}

#[test]
fn resolves_all_saved_status_providers_by_default() {
    let providers = resolve_status_providers(
        vec![
            Credentials {
                provider: Provider::Grok,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: String::new(),
            },
            Credentials {
                provider: Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: "account".into(),
            },
        ],
        None,
    )
    .unwrap();

    assert_eq!(providers, [Provider::Codex, Provider::Grok]);
}

#[test]
fn filters_status_provider_when_requested() {
    let providers = resolve_status_providers(
        vec![
            Credentials {
                provider: Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: "account".into(),
            },
            Credentials {
                provider: Provider::Grok,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: String::new(),
            },
        ],
        Some(Provider::Grok),
    )
    .unwrap();

    assert_eq!(providers, [Provider::Grok]);
}

#[test]
fn rejects_missing_status_provider() {
    assert!(
        resolve_status_providers(
            vec![Credentials {
                provider: Provider::Codex,
                access_token: "access".into(),
                refresh_token: "refresh".into(),
                expires_at: now_unix() + 90,
                account_id: "account".into(),
            }],
            Some(Provider::Grok),
        )
        .is_err()
    );
}

#[test]
fn formats_daemon_endpoint_lines_with_base_url() {
    let lines = daemon_endpoint_lines("http://127.0.0.1:14550");

    assert!(lines.contains(&"  GET        http://127.0.0.1:14550/health".to_owned()));
    assert!(lines.contains(&"  POST       http://127.0.0.1:14550/v1/chat/completions".to_owned()));
    assert!(lines.contains(&"  GET,POST   http://127.0.0.1:14550/v1/messages/batches".to_owned()));
    assert!(
        lines.contains(&"  POST       http://127.0.0.1:14550/v1/images/generations".to_owned())
    );
}

#[test]
fn formats_new_provider_daemon_restart_hint() {
    assert_eq!(
        new_provider_daemon_restart_hint(Provider::Grok),
        "If rotom daemon is already running, run `rotom daemon restart` to serve newly logged-in Grok models."
    );
}
