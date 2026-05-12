# codexia

[![CI](https://github.com/RyanKung/codexia/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/RyanKung/codexia/actions/workflows/ci.yml)
[![Release](https://github.com/RyanKung/codexia/actions/workflows/release.yml/badge.svg?branch=master)](https://github.com/RyanKung/codexia/actions/workflows/release.yml)

Rust gateway that logs in with OpenAI Codex OAuth and exposes OpenAI- and
Anthropic-compatible APIs.

## Usage Boundaries

Codexia is intended for local, personal compatibility testing and self-hosted
automation against credentials you are authorized to use.

Before you deploy or distribute it, keep these boundaries in mind:

- do not treat Codexia as an official OpenAI or Anthropic product or integration
- do not use OpenAI or Anthropic logos, trade dress, or branding in a way that implies endorsement
- do not share one person's OAuth-backed access with other users, customers, or a public service
- do not resell, sublicense, or expose pooled account access through Codexia
- review the upstream terms, policies, and any workplace data-handling requirements that apply to your account

If you need multi-user, customer-facing, or revenue-generating usage, you
should get legal review first and prefer a deployment model based on official
commercial API access rather than consumer-style OAuth credentials.

See [LEGAL.md](LEGAL.md) for a more detailed risk breakdown.

Codexia itself is licensed under LGPLv3, which does not prohibit commercial
use of the software. That does not mean your upstream account terms allow
commercial deployment, shared access, resale, or hosted service operation.

## Usage

```bash
cargo install codexia
codexia login
codexia config
codexia serve

# later, update to the latest published release
codexia update
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

If Claude Code or its explore/sub-agent path still emits Anthropic-native model
ids such as `claude-sonnet-*`, enable a local fallback:

```bash
CODEXIA_MODEL_FALLBACK=gpt-5.5 codexia serve --api-key local-secret
```

With that flag enabled, Codexia rewrites known unsupported Anthropic model ids
to the configured fallback before calling Codex.

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

You can combine it with the model fallback when running Claude Code against the
gateway:

```bash
CODEXIA_API_KEY=local-secret \
CODEXIA_MODEL_FALLBACK=gpt-5.5 \
codexia serve
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
Windows does not currently implement native daemon/service management; use WSL
and run the Linux build there if you need `codexia daemon` commands.

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
```

Credentials are stored at `~/.codexia/auth.json` by default. Override with
`--auth-file`, `CODEXIA_AUTH_FILE`, or `CODEXIA_HOME`.

Runtime config also supports an optional `model_fallback` value, and the CLI
accepts `--model-fallback` / `CODEXIA_MODEL_FALLBACK`.

OpenAI compatibility currently covers:

- `GET /v1/models`
- `POST /v1/chat/completions`
- `POST /v1/responses`
- `POST /v1/images/generations`
- `POST /v1/responses/compact`
- `POST /v1/responses/input_tokens`

On `POST /v1/chat/completions`, Codexia accepts common OpenAI compatibility
fields such as `temperature`, `max_tokens`, `max_completion_tokens`, and
`max_output_tokens`, but the current Codex upstream rejects those parameters.
Codexia therefore accepts them without error and omits them from the upstream
Codex request, so they should be treated as compatibility no-ops rather than
effective sampling or output-length controls.

`/v1/responses` currently supports `previous_response_id` only as an
in-memory continuation mechanism within the same running Codexia process. It is
not exposed as a public retrievable/deletable response resource, and it should
not be treated as durable storage across daemon restarts or process exits.

Image generation is exposed in two compatibility shapes:

- OpenAI-style `POST /v1/images/generations`
- OpenAI Responses hosted tool `{"type":"image_generation"}`

Current image-generation caveats:

- OpenAI `POST /v1/responses` supports streaming image-generation events
- `POST /v1/images/generations` remains non-streaming
- Anthropic `POST /v1/messages` image generation streaming is exposed as a
  Codexia extension that emits `image` content blocks only once the upstream
  response completes
- generated images are returned as base64 payloads
- Anthropic compatibility uses a Codexia extension that returns
  `content: [{"type":"image","source":{"type":"base64",...}}]` on
  `POST /v1/messages` when the request includes a tool named `image_generation`

Anthropic compatibility currently covers:

- `GET /v1/models` with an `anthropic-version` header
- `POST /v1/messages`
- `POST /v1/messages/count_tokens`
- `POST /v1/messages/batches`
- `GET /v1/messages/batches`
- `GET /v1/messages/batches/{batch_id}`
- `POST /v1/messages/batches/{batch_id}/cancel`
- `DELETE /v1/messages/batches/{batch_id}`
- `GET /v1/messages/batches/{batch_id}/results`
- `x-api-key` or `authorization: Bearer ...` local auth
- Anthropic-style SSE events for streaming text and tool use

Message batches execute asynchronously in a background task. Cancellation is
best-effort at request boundaries inside the batch worker: requests that have
already started are allowed to finish, while not-yet-started requests are
marked as `canceled`.

The implementation intentionally follows Ollama's compatibility strategy where
possible: Anthropic headers are accepted, locally configured auth is enforced,
and unsupported advanced Anthropic-only features are ignored rather than
rejected when possible.

The OAuth flow follows OpenClaw/pi-ai's Codex flow: PKCE, manual paste of the
`http://localhost:1455/auth/callback?...` redirect URL, token exchange at
`https://auth.openai.com/oauth/token`, and Codex requests to
`https://chatgpt.com/backend-api/codex/responses`.

## Disclaimer

Codexia is an unofficial compatibility tool. It is not affiliated with,
endorsed by, or supported by OpenAI or Anthropic.

You are responsible for making sure your usage complies with the terms,
policies, account restrictions, and data-handling obligations that apply to
your upstream account and deployment environment. In particular, do not assume
that personal OAuth-backed access can be shared, resold, or safely exposed as a
multi-user hosted service. The LGPLv3 license for this repository does not
change those upstream restrictions.

## License

Copyright (c) 2026 Codexia contributors.

Licensed under the GNU Lesser General Public License v3.0 only. See [LICENSE](LICENSE).
