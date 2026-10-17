# liteton

A CLI that points your coding tools ("harnesses") at a [LiteLLM](https://docs.litellm.ai/) proxy and shows how much of your LiteLLM budget is left.

Run `liteton install`, pick your tools and models, and liteton adds a `litellm` provider to each tool's config, with model limits, prices and reasoning settings from your proxy. It merges into your existing config instead of overwriting it, backs up every file before changing it, and `liteton uninstall` removes only what it added.

| Harness | What liteton configures |
|---|---|
| **VSCode** (Copilot Chat) | A `litellm` custom endpoint in `chatLanguageModels.json`. The API key goes into VSCode's encrypted secret storage. |
| **opencode** | A `litellm` provider in `opencode.jsonc`/`opencode.json`. The API key goes into opencode's `auth.json`. |
| **opencode 2** | A `litellm` entry under `providers` in the same file, in the v2 format. The API key is stored with `opencode auth import`. |
| **Kilo Code** | A `litellm` provider in `~/.config/kilo/kilo.jsonc`, read by the Kilo CLI, the VS Code extension and the JetBrains plugin. The API key goes into Kilo's `auth.json`. |
| **Cursor** (experimental) | The OpenAI base URL override, the enabled models, and the OpenAI API key in Cursor's settings. |

> **macOS only.** liteton uses the macOS Keychain and the macOS paths of VSCode and Cursor.

## Install

You need a recent stable Rust toolchain (Rust 1.88 or newer). If you use [mise](https://mise.jdx.dev/), `mise install` sets it up from `mise.toml`.

```sh
git clone <this repository>
cd liteton
cargo install --path .
```

This installs the `liteton` binary into `~/.cargo/bin`.

## Quick start

```sh
liteton login      # save the LiteLLM URL and API key, then continue into the install
liteton install    # choose harnesses and models, preview, apply (any time later)
liteton            # open the dashboard
```

## Commands

| Command | Description |
|---|---|
| `liteton` / `liteton dashboard` | Full-screen dashboard: budget, models with prices and limits, harness status. |
| `liteton login` | Save the LiteLLM base URL and API key. The key is checked against the proxy first, then liteton offers to run `install`. |
| `liteton logout` | Remove the saved base URL and API key. |
| `liteton install` | Configure harnesses to use LiteLLM models. |
| `liteton uninstall` | Remove what liteton added to harness configs. |
| `liteton models` | List the available models with pricing and limits. |
| `liteton usage` | Show spend and remaining budget for your key. |

Run `liteton <command> --help` to see all options.

### `login`

```sh
liteton login --base-url https://litellm.example.com
```

liteton asks for the API key in a hidden prompt. You can also pass it with `--api-key` or `LITETON_API_KEY`, but the prompt keeps it out of your shell history. A trailing `/v1` in the URL is removed.

After saving, liteton asks "Configure harnesses now?" and continues straight into `install`. When it isn't running in a terminal (scripts, pipes), it only saves.

### `install`

```sh
liteton install                                   # interactive
liteton install --harness vscode opencode \
                --models azure/gpt-5-mini azure/gpt-5.4-nano
liteton install --dry-run                         # preview only
liteton install --yes                             # no prompts
```

| Option | Description |
|---|---|
| `--harness <vscode\|opencode\|opencode2\|kilo\|cursor>...` | Harnesses to configure. Without it, liteton asks and preselects the ones it detects. Cursor is never preselected. |
| `--models <id>...` | Model ids to install. Without it, liteton asks. On later runs, the models you installed before are preselected. |
| `--dry-run` | Show the changes without writing anything. |
| `-y`, `--yes` | Skip confirmation prompts. Cursor warnings are still printed. |

Before writing, liteton shows a diff of every change and asks for confirmation. Running `install` again updates the existing entries: new models are added, models you deselected are removed, and the secret is only rewritten when the API key has changed.

### `uninstall`

```sh
liteton uninstall
liteton uninstall --harness opencode --yes
```

liteton removes only the models, provider entries and secrets that it created. Entries that already existed before liteton touched them stay where they are. For Cursor, it restores the settings it overwrote. `--dry-run` and `--yes` work like they do for `install`.

### `models` and `usage`

```sh
liteton models          # table
liteton models --json   # machine-readable
liteton usage
liteton usage --json
liteton usage --ping
```

`usage` reads the budget from `/key/info` and falls back to `/user/info`. If your proxy blocks both, `--ping` sends a 1-token request and reads the budget from LiteLLM's response headers. This costs a tiny amount.

### Dashboard keys

| Key | Action |
|---|---|
| `↑`/`↓`, `k`/`j` | Move through the model list |
| `r` | Reload |
| `p` | Read the budget with a 1-token ping (only when the budget endpoints are blocked) |
| `q`, `Esc`, `Ctrl+C` | Quit |

## How it works

### Models

liteton reads the models your key may use from `/v1/models`. It gets context window, max output tokens, prices (including cache and long-context prices), vision and reasoning support from `/model/info`, or from `/model_group/info` if that route is blocked. If neither is available, models are installed without that metadata. Embedding, audio and image models are filtered out.

### VSCode

- Writes the `litellm` entry to every VSCode profile's `chatLanguageModels.json`: `~/Library/Application Support/Code/User/chatLanguageModels.json` for the Default profile, and `…/Code/User/profiles/<id>/chatLanguageModels.json` for each other profile. The profile list comes from VSCode's `User/globalStorage/storage.json`. Profiles set to use the Default profile's language models are skipped, because VSCode reads the Default file for them. A profile whose file can't be parsed is skipped, and the preview names it.
- Existing files are merged: comments, formatting and your other entries are kept. If a `litellm` entry already exists, liteton merges into it and reuses its secret. Profiles without the file get one, and all new entries share one secret.
- Stores the API key in VSCode's secret storage (`state.vscdb`), encrypted the same way VSCode does it, with the "Code Safe Storage" password from the Keychain.
- Open VSCode once before installing, so that `state.vscdb` exists.

### opencode

- Writes `provider.litellm` to `~/.config/opencode/opencode.jsonc` (or `opencode.json` if that is the file you use), including costs, limits and reasoning variants.
- Adds the API key to `~/.local/share/opencode/auth.json` and keeps all other entries. The file is written with mode `0600`.
- `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are respected.

### opencode 2

- opencode 1 and 2 read the same config file. opencode 2 gets `providers.litellm` (v2 format: `capabilities`, `cost.cache`, `variants` as a list), and opencode 1 ignores that key. If you use both, select both: each version keeps its own entry.
- Long-context prices are written exactly. opencode 2's `cost` takes a list of context tiers, so every `above_<N>k_tokens` price from LiteLLM becomes its own tier, e.g. above 100k or 272k. opencode 1 only has a slot at 200k.
- opencode 2 reads `auth.json` only once, when it first starts. So liteton adds its own credential (`cred_liteton_litellm`) with `opencode auth import` and makes it the active LiteLLM credential. Your other credentials are kept, and `liteton uninstall` removes only that one with `opencode auth logout`. This needs opencode 2 as the `opencode` on your `PATH`.
- If you installed with opencode 1 and then migrated your config, the first opencode 2 install takes over the opencode 1 record, so `uninstall` still removes the models liteton added.

### Kilo Code

- Kilo Code is built on opencode 1, and its CLI, VS Code extension and JetBrains plugin share one config. liteton writes `provider.litellm` in the opencode 1 format to `~/.config/kilo/kilo.jsonc`. If you already use `kilo.json`, `opencode.jsonc`, `opencode.json` or `config.json` there, liteton writes the first of those that exists, which is the same file Kilo saves its settings to.
- The API key goes into `~/.local/share/kilo/auth.json`. Other entries are kept, and the file is written with mode `0600`.
- You can run liteton before installing the extension. Kilo only creates a config when none exists, and it edits the file in place when you change settings, so liteton's entry stays. A running extension picks up the change without a restart.
- The legacy Kilo Code extension (v4 and earlier) kept providers in VS Code settings and is not supported.
- Long-context pricing works as in opencode 1, including the `opencode_approximate_long_context` setting.
- Kilo uses only LiteLLM. liteton sets these in Kilo's config, and `uninstall` puts back the values you had before:

  | Key | Value | Why |
  |---|---|---|
  | `enabled_providers` | `["litellm"]` | Hides the Kilo Gateway and every other provider |
  | `model` | `litellm/<model>` | The default chat model; Kilo's own default is a Kilo Gateway model |
  | `small_model` | `litellm/<model>` | Used for titles and summaries |
  | `share` | `"disabled"` | No session sharing through Kilo's servers |
  | `remote_control` | `false` | No remote control through Kilo Cloud |
  | `experimental.openTelemetry` | `false` | No telemetry |

- `install` asks for the default and the small model the first time, preselecting the cheapest model for the small one, and saves your answers as `kilo_model` and `kilo_small_model` in `~/.config/liteton/config.toml`. With `--yes` it takes the first selected model and the cheapest one.
- Kilo's autocomplete only works with the Kilo Gateway, Mistral or Inception, not LiteLLM. If the Kilo VS Code extension is installed, liteton turns it off and sets the default model (`kilo-code.new.autocomplete.enableAutoTrigger`, `kilo-code.new.model.providerID`, `kilo-code.new.model.modelID`) in the `settings.json` of every VS Code profile that has its own settings. Profiles that share the Default profile's settings get them from there. In JetBrains IDEs, turn off autocomplete yourself under Settings > Kilo Code > Autocomplete.

### Cursor (experimental)

Read the warnings that liteton prints before you continue:

- Cursor sends custom-key requests **from its own servers**. Your LiteLLM URL must be reachable from the public internet. liteton refuses `localhost`, `.local` and private IP addresses, so proxies that are only reachable via VPN or company network won't work.
- The OpenAI base URL override is global: while it is on, Cursor's built-in OpenAI models are sent to your proxy too. Turn off "OpenAI API Key" in Cursor Settings > Models to use them normally again.
- Cursor may reject model ids that contain `/`. If that happens, add an alias for the model in LiteLLM.

### VSCode and Cursor must be closed

VSCode and Cursor keep their `state.vscdb` in memory and would overwrite liteton's changes. If the app is running, liteton offers to quit it. It can't do that if you run liteton from the app's integrated terminal; in that case, use another terminal such as Terminal.app.

### Keychain access

To encrypt the API key the way VSCode/Cursor expect it, liteton reads the app's "Safe Storage" password from your login Keychain:

| App | Service | Account |
|---|---|---|
| VSCode | `Code Safe Storage` | `Code Key`, then `Code` |
| Cursor | `Cursor Safe Storage` | `Cursor Key`, then `Cursor` |

If no account name matches, liteton uses the first item with that service. macOS asks whether liteton may access the item; choose **Allow** (or **Always Allow**).

Before writing, liteton decrypts a secret that the app stored itself, to make sure the password is right. If that check fails, nothing is written. If the app has no secrets yet, liteton warns that it could not double-check the encryption.

### Backups and state

| Path | Contents |
|---|---|
| `~/.config/liteton/config.toml` | Base URL and reasoning-effort settings |
| `~/.config/liteton/state.json` | What liteton added to each harness, used by `uninstall` |
| `~/.config/liteton/backups/<timestamp>/` | Copies of every file and database before it was changed |

The API key itself is stored in the macOS Keychain (service `liteton`, account `api-key`), not in `config.toml`. If `XDG_CONFIG_HOME` is set, liteton uses that directory instead of `~/.config`.

## Configuration

### Environment variables

| Variable | Description |
|---|---|
| `LITETON_BASE_URL` | Overrides the saved base URL. |
| `LITETON_API_KEY` | Overrides the API key from the Keychain. |

With both set, liteton works without `liteton login`, e.g. in scripts.

### Reasoning effort levels

For reasoning models, liteton writes the effort levels your proxy reports for each model (VSCode `supportsReasoningEffort`, opencode `variants`). It reads them from `/model/info`: `reasoning_effort_levels` when the model has it, otherwise `low`, `medium` and `high`, plus `none`, `minimal`, `xhigh` and `max` when the matching `supports_<level>_reasoning_effort` flag is true. liteton also corrects a few levels LiteLLM's catalog gets wrong: its `azure/` entries claim `none` for gpt-6.1-sol and gpt-6-astra, which OpenAI rejects. `liteton models` shows the result. Models the proxy reports nothing for get `low`, `medium` and `high`; older LiteLLM versions don't report these fields.

You can override this in `~/.config/liteton/config.toml`. A per-model list wins over what the proxy reports, and the global list only applies to models the proxy reports nothing for:

```toml
base_url = "https://litellm.example.com"
reasoning_efforts = ["low", "medium", "high"]

[model_reasoning_efforts]
"azure/gpt-5-mini" = ["minimal", "low", "medium", "high"]
```

Run `liteton install` again afterwards to apply the change.

In opencode and Kilo configs, each effort variant liteton writes goes on one line, e.g. `{ "id": "low", "settings": { "reasoningEffort": "low" } }`. Variants you wrote yourself keep their layout. Configs written by an older liteton are reformatted this way on the next `install`.

## Troubleshooting

| Problem | Fix |
|---|---|
| `state.vscdb not found; open VSCode once before installing` | Start VSCode (or Cursor) once, then quit it and run `install` again. |
| `reading "Code Safe Storage" from the Keychain: …` | Check in Keychain Access that a "Code Safe Storage" item exists in the login keychain, and choose **Allow** when macOS asks. |
| `round-trip check failed …` | The Keychain password does not decrypt VSCode's existing secrets. liteton wrote nothing. |
| `… must be closed …, but liteton is running inside it` | Run liteton from a terminal outside VSCode/Cursor. |
| `this key may not read /key/info or /user/info` | Run `liteton usage --ping`. |
| `has no /v1/models endpoint, is this a LiteLLM proxy?` | Check the base URL with `liteton login`. |

To undo everything, run `liteton uninstall`. You can also restore a file by hand from `~/.config/liteton/backups/`.

## Development

```sh
cargo build
cargo test
```

Code layout:

| Path | Contents |
|---|---|
| `src/cli.rs` | Command-line arguments |
| `src/config.rs` | Config, credentials, install state |
| `src/litellm/` | LiteLLM API client, model metadata, budget |
| `src/harness/` | One module per harness; each produces a plan of changes that `apply.rs` writes. `opencode/` covers opencode 1, opencode 2 and Kilo Code: a `Schema` per config format and an `App` per tool |
| `src/vscdb/` | Reading/writing `state.vscdb` and Electron safeStorage encryption |
| `src/ui/` | Prompts, install flow, tables, dashboard |

## License

[Apache License 2.0](LICENSE)
