<p align="center">
  <img src="logo.png" alt="rotom logo" width="144">
</p>

# rotom

[![CI](https://github.com/RyanKung/rotom/actions/workflows/ci.yml/badge.svg?branch=master)](https://github.com/RyanKung/rotom/actions/workflows/ci.yml)
[![Release](https://github.com/RyanKung/rotom/actions/workflows/release.yml/badge.svg?branch=master)](https://github.com/RyanKung/rotom/actions/workflows/release.yml)

Use your Codex, Grok, Kiro, or Cursor OAuth login from any tool that speaks the
OpenAI or Anthropic API.

rotom is a small local gateway. You log in once, run `rotom serve`, and point
Claude Code, the OpenAI/Anthropic SDKs, or any compatible client at the local
address.

## Demo

Claude Code running through Grok and GPT:

<p align="center">
  <img src="demos/claude-grok-4.3.gif" alt="Claude Code using grok-4.3 through rotom" width="720">
</p>
<p align="center">
  <img src="demos/claude-gpt-5.5.gif" alt="Claude Code using gpt-5.5 through rotom" width="720">
</p>

## Quick Start

```bash
cargo install rotom
rotom login                                   # pick a provider, finish in browser
rotom serve --bind 127.0.0.1:14550 --api-key local-secret
```

Point Claude Code (or any Anthropic-compatible client) at the gateway:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:14550
export ANTHROPIC_AUTH_TOKEN=local-secret
export ANTHROPIC_MODEL="gpt-5.5"

claude
```

That's it. Quick sanity check:

```bash
claude -p "Reply with the single word OK"
```

> Point `ANTHROPIC_BASE_URL` at the server root, **not** `/v1` — clients append
> `/v1/messages` themselves. `ANTHROPIC_AUTH_TOKEN` is your local `--api-key`,
> not an upstream token. Use a model that `/v1/models` lists (e.g. `gpt-5.5`).

## Logging In

`rotom login` lists the providers and runs the chosen flow. Skip the prompt with
a flag:

| Provider     | Command                        |
| ------------ | ------------------------------ |
| OpenAI/Codex | `rotom login --provider openai`|
| Grok (xAI)   | `rotom login --provider grok`  |
| Kiro         | `rotom login --kiro`           |
| Cursor       | `rotom login --cursor`         |

- **Codex / Grok**: browser login, then paste the redirected
  `http://localhost:.../auth/callback?...` URL back into the terminal.
- **Kiro**: browser login via Kiro's portal callback (Google/GitHub).
- **Cursor**: browser approval that rotom polls for — no localhost callback.

Credentials are stored per provider in `~/.rotom/auth.json` (override with
`ROTOM_AUTH_FILE` or `ROTOM_HOME`). Logging in to one provider never replaces
another, and `serve` exposes every logged-in provider at once. If a daemon is
already running, restart it to pick up a new provider:

```bash
rotom daemon restart
```

## Models

```bash
rotom models                      # everything rotom exposes
rotom models --provider grok      # one provider
```

Common ids include `gpt-5.5`, `grok-4.3`, Kiro's `claude-*` family, and
`cursor/auto`. rotom fetches the live registry where the provider supports it
and falls back to built-in aliases otherwise.

Unknown Anthropic ids like `claude-sonnet-*` are rewritten to a fallback
(default `gpt-5.5`). Override with `--model-fallback` or `ROTOM_MODEL_FALLBACK`.

## Use With SDKs

OpenAI-compatible:

```bash
curl http://127.0.0.1:14550/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer local-secret' \
  -d '{"model": "gpt-5.5", "messages": [{"role": "user", "content": "hello"}]}'
```

Anthropic-compatible:

```bash
curl http://127.0.0.1:14550/v1/messages \
  -H 'content-type: application/json' \
  -H 'x-api-key: local-secret' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model": "gpt-5.5", "max_tokens": 1024, "messages": [{"role": "user", "content": "hello"}]}'
```

## Running As a Service

Run a background daemon instead of `rotom serve`:

```bash
rotom daemon install --bind 127.0.0.1:14550 --api-key local-secret
rotom daemon start
rotom daemon status      # also: restart / stop / uninstall
```

macOS uses a LaunchAgent; Linux uses a systemd user unit. On Windows, use WSL.
The `--api-key` is stored in `~/.rotom/config.json` rather than embedded in the
service definition.

## Other Commands

```bash
rotom status                      # version, token expiry, auth, endpoints
rotom refresh                     # refresh OAuth tokens for all providers
rotom config                      # interactive config (~/.rotom/config.json)
rotom update                      # update to the latest release
```

`--bind` accepts a comma-separated list and CIDR selectors, e.g.
`--bind 127.0.0.1:14550,192.168.1.0/24:14550`. Token refresh and status are also
available over HTTP at `/v1/auth/refresh` and `/v1/status`.

## Supported Endpoints

OpenAI:

- `GET /v1/models`, `POST /v1/chat/completions`
- `POST /v1/responses` (+ retrieve / delete / cancel / input_items / compact /
  input_tokens)
- `POST /v1/images/generations`

Anthropic:

- `GET /v1/models`, `POST /v1/messages`, `POST /v1/messages/count_tokens`
- Message batches: `POST/GET /v1/messages/batches` (+ get / cancel / delete /
  results)
- `x-api-key` or `authorization: Bearer ...` auth, SSE streaming for text and
  tool use

Image generation is exposed both as `POST /v1/images/generations` and as the
Responses hosted tool `{"type":"image_generation"}`; generated images are
returned as base64.

## Provider Notes

These upstreams are not all plain model APIs, so rotom adapts requests and
quietly drops controls the upstream cannot honor.

- **Codex**: accepts `temperature` / `max_tokens` and similar fields but does
  not forward them (Codex rejects them upstream). `/v1/responses` keeps a local
  replay behavior for existing clients.
- **Grok**: uses xAI's native Responses API and forwards supported controls
  (`temperature`, `top_p`, `max_output_tokens`, `stop`, ...).
- **Kiro**: mapped to Kiro's `GenerateAssistantResponse` schema (text, tools,
  tool results, history, inline base64 images/documents). Remote image/document
  URLs are rejected, not fetched.
- **Cursor**: an agent runtime, so client tools are **bridged** rather than
  executed by rotom — they are exposed to Cursor as a `rotom-tools` MCP server,
  Cursor's tool calls come back as standard `tool_call`/`tool_use` items for your
  client to run, and the results feed back into the same stream. Cursor's own
  built-in tools are declined. Requests default to agent mode; override with
  `ROTOM_CURSOR_AGENT_MODE`. Multimodal and sampling controls are not forwarded.

## Disclaimer

rotom is an unofficial compatibility tool. It is not affiliated with, endorsed
by, or supported by OpenAI, Anthropic, xAI, Kiro, or Cursor.

You are responsible for complying with the terms and account restrictions of
your upstream provider. In particular, do not assume personal OAuth access can
be shared, resold, or exposed as a multi-user hosted service. The LGPLv3 license
does not change those upstream restrictions.

## License

Copyright (c) 2026 rotom contributors.

Licensed under the GNU Lesser General Public License v3.0 only. See [LICENSE](LICENSE).
