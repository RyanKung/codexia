# codexia

Rust gateway that logs in with OpenAI Codex OAuth and exposes OpenAI- and
Anthropic-compatible APIs.

## Usage

```bash
cargo install codexia
codexia login
codexia config
codexia serve
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

Anthropic-compatible Messages request:

```bash
curl http://127.0.0.1:14550/v1/messages \
  -H 'content-type: application/json' \
  -H 'x-api-key: local-secret' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{
    "model": "gpt-5.5",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": "hello"}]
  }'
```

Claude Code / Anthropic SDK setup:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:14550
export ANTHROPIC_AUTH_TOKEN=local-secret
claude --model gpt-5.5
```

`ANTHROPIC_BASE_URL` should point at the Codexia server root, not `/v1`,
because Anthropic clients append `/v1/messages` themselves. Streaming emits
Anthropic-style `message_delta` `stop_reason` and cumulative
`usage.output_tokens`.

Full Claude Code flow:

1. Log in and save Codex OAuth credentials:

   ```bash
   codexia login
   ```

2. Start Codexia with a supported model list and a local API key:

   ```bash
   codexia serve --bind 127.0.0.1:14550 --api-key local-secret
   ```

3. Point Claude Code at the local gateway:

   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:14550
   export ANTHROPIC_AUTH_TOKEN=local-secret
   ```

4. Run Claude Code against a model that Codexia exposes:

   ```bash
   claude --model gpt-5.5
   ```

For non-interactive validation, this works:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:14550 \
ANTHROPIC_AUTH_TOKEN=local-secret \
claude -p --model gpt-5.5 "Reply with the single word OK"
```

Common pitfalls:

- Do not set `ANTHROPIC_BASE_URL` to `http://127.0.0.1:14550/v1`; Claude Code appends `/v1/messages` itself.
- Use a model that `/v1/models` actually returns, such as `gpt-5.5`. If Claude Code defaults to `claude-sonnet-*`, the request will fail because Codexia proxies Codex models, not Anthropic-hosted model IDs.
- `ANTHROPIC_AUTH_TOKEN` is only the local gateway key configured with `--api-key`; it is not your upstream OpenAI/Codex OAuth token.
- If you prefer a background service, install the daemon first and then point `ANTHROPIC_BASE_URL` at the daemon address instead of running `codexia serve` manually.

Optional local API key protection:

```bash
CODEXIA_API_KEY=local-secret codexia serve
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
codexia daemon reinstall
codexia daemon start
codexia daemon status
codexia daemon restart
codexia daemon stop
codexia daemon uninstall
```

On macOS, Codexia installs a LaunchAgent at
`~/Library/LaunchAgents/com.codexia.daemon.plist`. On Linux, it installs a
systemd user unit at `~/.config/systemd/user/codexia.service`.

On Linux, inspect the per-user service with:

```bash
codexia daemon status
systemctl --user status codexia.service
```

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

Credentials are stored at `~/.codexia/auth.json` by default. Override with
`--auth-file`, `CODEXIA_AUTH_FILE`, or `CODEXIA_HOME`.

Anthropic compatibility currently covers:

- `POST /v1/messages`
- `POST /v1/messages/count_tokens`
- `x-api-key` or `authorization: Bearer ...` local auth
- Anthropic-style SSE events for streaming text and tool use

The implementation intentionally follows Ollama's compatibility strategy where
possible: Anthropic headers are accepted, locally configured auth is enforced,
and unsupported advanced Anthropic-only features are ignored rather than
rejected when possible.

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
