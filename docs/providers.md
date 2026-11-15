# Providers and logins

A provider is either a preset or a table in your config.
`--model <preset>/<id>` picks a preset by name, for example
`--model mistral/mistral-large-latest`, and needs no config table.
The preset names are in `reagent --help`.

A table names a provider, its preset and what it overrides:

```toml
[providers.mine]
preset = "mistral"
api_key = "..."
```

A preset whose key comes from the environment is read from that variable, so a
run with `MISTRAL_API_KEY` set needs no table at all.
`reagent models` lists what a provider serves, and `reagent login <provider>`
saves the login of a provider that has one.

## The mistral preset

`mistral` talks to `https://api.mistral.ai/v1` on the `openai-chat` wire.
Its key comes from `MISTRAL_API_KEY`:

```toml
[providers.mistral]
# key from MISTRAL_API_KEY, or: api_key = "..."
```

```bash
export MISTRAL_API_KEY=...
reagent -p "fix the failing test" --model mistral/mistral-large-latest
```

Its model list is read from `GET /models` and keeps the models whose
`capabilities.completion_chat` is true and that are not archived.
The context window comes from `max_context_length`, and the prices from
models.dev provider `mistral`.
The reasoning level goes in `reasoning_effort`, and `--reasoning off` sends
`none`; the Mistral Large 4 models take `low`, `medium` and `high` only.
The request carries no `stream_options` field: Mistral does not list it, so the
run reads usage from the last chunk when the endpoint sends one.

## The mistral-vibe preset

`mistral-vibe` is the same endpoint and the same flags, with the key of a
Mistral account that has the Vibe plan.
No environment variable holds that key: `reagent login mistral-vibe` saves it.

```toml
[providers.mistral-vibe]
# key from `reagent login mistral-vibe`, or: api_key = "..."
```

```bash
reagent login mistral-vibe
reagent -p "fix the failing test" --model mistral-vibe/mistral-large-latest
```

## The mistral-vibe login

`reagent login mistral-vibe [--no-browser] [--as <name>]` asks the Mistral
console for a sign-in page, shows it, and waits as the account confirms, at most
once every three seconds.
Sign in with the account that has the Vibe plan.
The exchange gives an API key that does not expire, saved in the auth file with
mode 0600 under `mistral-vibe`, or under the name `--as` gives.
A workspace without the Vibe plan fails with a line that says so.

`reagent logout mistral-vibe` removes the key.
Mistral documents no revoke call, so the logout drops the local copy only.
