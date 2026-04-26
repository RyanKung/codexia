# codexia

Rust gateway that logs in with OpenAI Codex OAuth and exposes an OpenAI-compatible API.

## Usage

```bash
cargo run -- login
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
    "model": "gpt-5.4",
    "messages": [{"role": "user", "content": "hello"}]
  }'
```

Optional local API key protection:

```bash
CODEXIA_API_KEY=local-secret cargo run -- serve
curl http://127.0.0.1:14550/v1/models -H 'authorization: Bearer local-secret'
```

Manually refresh the stored Codex OAuth token while the server is running:

```bash
curl -X POST http://127.0.0.1:14550/v1/auth/refresh \
  -H 'authorization: Bearer local-secret'
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
```

Override or extend the list with CLI flags or environment variables:

```bash
cargo run -- serve --models gpt-5.4,gpt-5.4-mini
CODEXIA_EXTRA_MODELS=my-model cargo run -- serve
CODEXIA_MODELS_FILE=models.json cargo run -- serve
```

`models.json` may be a JSON array or an object:

```json
["gpt-5.4", "gpt-5.4-mini"]
```

```json
{
  "models": ["gpt-5.4"],
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

Licensed under the MIT License. See [LICENSE](LICENSE).
