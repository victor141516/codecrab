# Authentication and providers

## ChatGPT Plus/Pro

```bash
codecrab auth login
codecrab auth status
codecrab auth logout
```

CodeCrab uses OAuth PKCE, refreshes tokens automatically, and uses the subscription's Codex allowance. The default OpenAI profile has `auth = "auto"`: it selects ChatGPT OAuth when a login exists, otherwise its configured API key. Use `oauth` or `api_key` to require one path explicitly.

When the selected provider is the official OpenAI service and ChatGPT OAuth is active, both clients show a compact quota indicator. Open `/usage` in the terminal or select the indicator in the web header to inspect every provider-reported window, percentage used and remaining, and the automatic reset time in your local timezone. Window lengths and reset-credit availability come from OpenAI rather than from hardcoded plan assumptions.

If OpenAI grants the account manual reset credits, the same view can redeem one at any time after explicit confirmation. Credits and quota belong to the OpenAI account, not to an individual conversation, so they can be viewed or used from any eligible OpenAI session. CodeCrab refreshes usage after successful turns and reset attempts. A failed refresh leaves the last known values marked stale and disables redemption until a refresh succeeds; usage failures never block chatting.

Quota and reset-credit support uses private ChatGPT subscription endpoints. This is not a public compatibility contract and may change without notice. The feature is hidden for OpenAI API-key sessions and OpenAI-compatible providers.

## API keys and compatible providers

Provider profiles are managed in the global `config.toml` shown by `codecrab config`. For example, add a local provider without authentication with:

```toml
[providers.local]
model = "local-model"
base_url = "http://localhost:11434/v1"
auth = "none"
fetch_models = false
allowed_models = ["local-model"]

[providers.local.model_capabilities.local-model]
input_modalities = ["text"]
output_modalities = ["text"]
```

List the configured profiles without exposing their API keys:

```bash
codecrab providers
```

Provider authentication modes are `auto`, `oauth`, `api_key`, and `none`. Edit provider secrets directly in `config.toml`; normal configuration and provider-list output omit them.

Profiles keep their base URL, current key, model, discovered or manually declared catalog, reasoning options, service tiers, modalities, and context limits together. Sessions store the profile name and model selection but never copy provider secrets. Use `/providers` in the terminal or the provider control above the web composer to change only the current session; the choice survives resume without changing `active_provider` or any other session. See [`codecrab.example.toml`](../codecrab.example.toml) for complete OpenAI, remote-compatible, and local-provider examples.

New sessions with `model = "auto"` prefer GPT-5.6 Sol with high reasoning and Fast speed when the live provider catalog offers that combination. Otherwise CodeCrab uses the provider's first model and declared defaults; it does not invent unsupported model variants.
