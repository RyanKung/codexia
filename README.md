<p align="center">
  <img src="logo.png" alt="rotom logo" width="144">
</p>

# rotom

[![CI](https://github.com/RyanKung/rotom/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/RyanKung/rotom/actions/workflows/ci.yml)
[![Release](https://github.com/RyanKung/rotom/actions/workflows/release.yml/badge.svg?branch=master)](https://github.com/RyanKung/rotom/actions/workflows/release.yml)

Rust gateway that logs in with Codex or Grok OAuth and exposes OpenAI- and
Anthropic-compatible APIs.

## Usage

```bash
cargo install rotom
rotom login
rotom config
rotom serve

# later, update to the latest published release
rotom update
```

`login` lists the available OAuth providers and starts the selected flow. Use
`rotom login --provider openai` or `rotom login --provider grok` to skip the
prompt. Complete the login in a browser, then paste the full redirected URL
from the browser address bar, for example
`http://localhost:1455/auth/callback?code=...&state=...`. This matches
OpenClaw's remote/headless fallback and does not require the gateway host to be
reachable from the public internet.

Grok OAuth uses the same local credential flow with xAI's OAuth endpoints:

```bash
rotom login --provider grok
rotom serve --bind 127.0.0.1:14550 --api-key local-secret
```

Credentials are stored per provider, so logging in to Grok does not replace
Codex credentials. When both are present, `serve` exposes both Codex and Grok
models through the same local OpenAI-compatible and Anthropic-compatible routes.
Use `--provider grok` only when you intentionally want to serve one provider.
If `rotom daemon` is already running when you add a new provider, restart it so
the running service loads the new provider:

```bash
rotom daemon restart
```

xAI may still restrict OAuth API access by account tier even when browser login
succeeds.

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
export ANTHROPIC_MODEL="gpt-5.5"
claude
```

`ANTHROPIC_BASE_URL` should point at the rotom server root, not `/v1`,
because Anthropic clients append `/v1/messages` themselves.

Minimal Claude Code flow:

```bash
rotom login
rotom serve --bind 127.0.0.1:14550 --api-key local-secret

export ANTHROPIC_BASE_URL=http://127.0.0.1:14550
export ANTHROPIC_AUTH_TOKEN=local-secret
export ANTHROPIC_MODEL="gpt-5.5"

claude
```

For non-interactive validation, this works:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:14550 \
ANTHROPIC_AUTH_TOKEN=local-secret \
ANTHROPIC_MODEL="gpt-5.5" \
claude -p "Reply with the single word OK"
```

rotom defaults unsupported Anthropic-native model ids such as
`claude-sonnet-*` to `gpt-5.5`. To override that fallback explicitly:

```bash
ROTOM_MODEL_FALLBACK=gpt-5.5 rotom serve --api-key local-secret
```

rotom rewrites known unsupported Anthropic model ids to the effective
fallback before calling Codex. When you do not configure one explicitly, the
default fallback is `gpt-5.5`.

Common pitfalls:

- Do not set `ANTHROPIC_BASE_URL` to `http://127.0.0.1:14550/v1`; Claude Code appends `/v1/messages` itself.
- Use a model that `/v1/models` actually returns, such as `gpt-5.5`. If Claude Code defaults to `claude-sonnet-*`, the request will fail because rotom proxies Codex models, not Anthropic-hosted model IDs.
- `ANTHROPIC_AUTH_TOKEN` is only the local gateway key configured with `--api-key`; it is not your upstream OpenAI/Codex OAuth token.
- If you prefer a background service, install the daemon first and then point `ANTHROPIC_BASE_URL` at the daemon address instead of running `rotom serve` manually.

Optional local API key protection:

```bash
ROTOM_API_KEY=local-secret rotom serve
curl http://127.0.0.1:14550/v1/models -H 'authorization: Bearer local-secret'
```

You can combine it with the model fallback when running Claude Code against the
gateway:

```bash
ROTOM_API_KEY=local-secret \
ROTOM_MODEL_FALLBACK=gpt-5.5 \
rotom serve
```

Interactive runtime configuration:

```bash
rotom config
rotom config show
rotom config reset
```

The config file is stored at `~/.rotom/config.json` by default and is used as
the fallback source for `rotom serve` and `rotom daemon install`.

Refresh stored OAuth tokens while the server is running:

```bash
curl -X POST http://127.0.0.1:14550/v1/auth/refresh \
  -H 'authorization: Bearer local-secret'
```

From the CLI, `rotom refresh` refreshes all saved providers. Use
`rotom refresh --provider grok` to refresh only one provider.

Check token expiry, account metadata, and rate-limit windows:

```bash
rotom status
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

Install rotom as a per-user background daemon:

```bash
rotom daemon install
rotom daemon reinstall
rotom daemon start
rotom daemon status
rotom daemon restart
rotom daemon stop
rotom daemon uninstall
```

On macOS, rotom installs a LaunchAgent at
`~/Library/LaunchAgents/com.rotom.daemon.plist`. On Linux, it installs a
systemd user unit at `~/.config/systemd/user/rotom.service`.
Windows does not currently implement native daemon/service management; use WSL
and run the Linux build there if you need `rotom daemon` commands.

On Linux, inspect the per-user service with:

```bash
rotom daemon status
systemctl --user status rotom.service
```

The daemon runs `rotom serve` with the options passed at install time:

```bash
rotom daemon install \
  --bind 127.0.0.1:14550 \
  --api-key local-secret
```

List the local model registry grouped by provider:

```bash
rotom models
rotom models --provider openai
rotom models --provider grok
```

Models returned by `/v1/models` include the OpenAI/Codex registry:

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

and the Grok registry:

```text
grok-4.3
grok-4.3-fast
grok-4
```

Credentials are stored at `~/.rotom/auth.json` by default. Override with
`--auth-file`, `ROTOM_AUTH_FILE`, or `ROTOM_HOME`.

Runtime config supports `model_fallback`, and the CLI accepts
`--model-fallback` / `ROTOM_MODEL_FALLBACK`. When unset, rotom defaults the
fallback to `gpt-5.5`.

OpenAI compatibility currently covers:

- `GET /v1/models`
- `POST /v1/chat/completions`
- `POST /v1/responses`
- `POST /v1/images/generations`
- `POST /v1/responses/compact`
- `POST /v1/responses/input_tokens`

On `POST /v1/chat/completions`, rotom accepts common OpenAI compatibility
fields such as `temperature`, `max_tokens`, `max_completion_tokens`, and
`max_output_tokens`, but the current Codex upstream rejects those parameters.
rotom therefore accepts them without error and omits them from the upstream
Codex request, so they should be treated as compatibility no-ops rather than
effective sampling or output-length controls.

`/v1/responses` currently supports `previous_response_id` only as an
in-memory continuation mechanism within the same running rotom process. It is
not exposed as a public retrievable/deletable response resource, and it should
not be treated as durable storage across daemon restarts or process exits.

Image generation is exposed in two compatibility shapes:

- OpenAI-style `POST /v1/images/generations`
- OpenAI Responses hosted tool `{"type":"image_generation"}`

Current image-generation caveats:

- OpenAI `POST /v1/responses` supports streaming image-generation events
- `POST /v1/images/generations` remains non-streaming
- Anthropic `POST /v1/messages` image generation streaming is exposed as a
  rotom extension that emits `image` content blocks only once the upstream
  response completes
- generated images are returned as base64 payloads
- Anthropic compatibility uses a rotom extension that returns
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

The default Codex OAuth flow follows OpenClaw/pi-ai's Codex flow: PKCE, manual
paste of the `http://localhost:1455/auth/callback?...` redirect URL, token
exchange at `https://auth.openai.com/oauth/token`, and Codex requests to
`https://chatgpt.com/backend-api/codex/responses`. Grok OAuth uses xAI OIDC
discovery, PKCE, manual callback paste, and xAI Responses requests under
`https://api.x.ai/v1`.

## Disclaimer

rotom is an unofficial compatibility tool. It is not affiliated with,
endorsed by, or supported by OpenAI, Anthropic, or xAI.

You are responsible for making sure your usage complies with the terms,
policies, account restrictions, and data-handling obligations that apply to
your upstream account and deployment environment. In particular, do not assume
that personal OAuth-backed access can be shared, resold, or safely exposed as a
multi-user hosted service. The LGPLv3 license for this repository does not
change those upstream restrictions.

## License

Copyright (c) 2026 rotom contributors.

Licensed under the GNU Lesser General Public License v3.0 only. See [LICENSE](LICENSE).
