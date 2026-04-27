# codexia

Rust gateway that logs in with OpenAI Codex OAuth and exposes an OpenAI-compatible API.

## Usage

```bash
cargo run -- login
cargo run -- config
cargo run -- serve
```

`login` prints the Codex OAuth URL. Complete the login in a browser, then paste
the full redirected URL from the browser address bar, for example
`http://localhost:1455/auth/callback?code=...&state=...`. This matches
OpenClaw's remote/headless fallback and does not require the gateway host to be
reachable from the public internet.

OpenAI-compatible chat request:

```bash
curl http://127.0.0.1:14550/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{
    "model": "gpt-5.5",
    "messages": [{"role": "user", "content": "hello"}]
  }'
```

Optional local API key protection:

```bash
CODEXIA_API_KEY=local-secret cargo run -- serve
curl http://127.0.0.1:14550/v1/models -H 'authorization: Bearer local-secret'
```

Interactive runtime configuration:

```bash
codexia config
codexia config show
codexia config reset
```

The config file is stored at `~/.codexia/config.json` by default and is used as
the fallback source for `codexia serve` and `codexia daemon install`.

Manually refresh the stored Codex OAuth token while the server is running:

```bash
curl -X POST http://127.0.0.1:14550/v1/auth/refresh \
  -H 'authorization: Bearer local-secret'
```

Check token expiry, account metadata, and available rate-limit windows:

```bash
codexia status
```

Fetch the same status data over HTTP:

```bash
curl http://127.0.0.1:14550/v1/status \
  -H 'authorization: Bearer local-secret'
```

Example response:

```json
{
  "account_id": "acc_123",
  "token": {
    "expires_at": 1778098507,
    "remaining_seconds": 813427,
    "expires_at_local": "2026-05-05 12:15:07 +08:00"
  },
  "account": {
    "name": "Personal",
    "email": "user@example.com",
    "structure": "personal",
    "plan": "chatgptpro",
    "has_active_subscription": true,
    "subscription_expires_at": "2026-05-11T15:16:00+00:00",
    "subscription_expires_at_local": "2026-05-11 23:16:00 +08:00",
    "subscription_remaining_seconds": 1212345
  },
  "credits_balance": 0,
  "rate_limits": [
    {
      "name": "5h",
      "remaining_percent": 97.0,
      "reset_at": "1777297264",
      "reset_at_local": "2026-04-27 21:41:04 +08:00",
      "reset_in_seconds": 8658
    },
    {
      "name": "weekly",
      "remaining_percent": 68.0,
      "reset_at": "1777400385",
      "reset_at_local": "2026-04-29 02:19:45 +08:00",
      "reset_in_seconds": 111779
    }
  ],
  "warnings": []
}
```

Install Codexia as a per-user background daemon:

```bash
codexia daemon install
codexia daemon start
codexia daemon restart
codexia daemon stop
codexia daemon uninstall
```

On macOS, Codexia installs a LaunchAgent at
`~/Library/LaunchAgents/com.codexia.daemon.plist`. On Linux, it installs a
systemd user unit at `~/.config/systemd/user/codexia.service`.

The daemon runs `codexia serve` with the options passed at install time:

```bash
codexia daemon install \
  --bind 127.0.0.1:14550 \
  --api-key local-secret
```

Models returned by `/v1/models` default to OpenClaw's `openai-codex` registry:

```text
gpt-5.1
gpt-5.1-codex-max
gpt-5.1-codex-mini
gpt-5.2
gpt-5.2-codex
gpt-5.3-codex
gpt-5.3-codex-spark
gpt-5.4
gpt-5.4-mini
gpt-5.5
gpt-5.5-mini
```

Override or extend the list with CLI flags or environment variables:

```bash
cargo run -- serve --models gpt-5.5,gpt-5.5-mini
CODEXIA_EXTRA_MODELS=my-model cargo run -- serve
CODEXIA_MODELS_FILE=models.json cargo run -- serve
```

`models.json` may be a JSON array or an object:

```json
["gpt-5.5", "gpt-5.5-mini"]
```

```json
{
  "models": ["gpt-5.5"],
  "extra_models": ["my-model"]
}
```

Credentials are stored at `~/.codexia/auth.json` by default. Override with
`--auth-file`, `CODEXIA_AUTH_FILE`, or `CODEXIA_HOME`.

The OAuth flow follows OpenClaw/pi-ai's Codex flow: PKCE, manual paste of the
`http://localhost:1455/auth/callback?...` redirect URL, token exchange at
`https://auth.openai.com/oauth/token`, and Codex requests to
`https://chatgpt.com/backend-api/codex/responses`.

## Disclaimer

Codexia is an unofficial tool and is not affiliated with, endorsed by, or
supported by OpenAI. Use it at your own risk and make sure your usage complies
with the terms that apply to your account and the upstream services.

## License

Copyright (c) 2026 Codexia contributors.

Licensed under the GNU Lesser General Public License v3.0 only. See [LICENSE](LICENSE).
