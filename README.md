# liteton

A CLI that points your coding tools ("harnesses") at a [LiteLLM](https://docs.litellm.ai/) proxy and shows how much of your LiteLLM budget is left.

Run `liteton install`, pick your tools and models, and liteton adds a `litellm` provider to each tool's config, with model limits, prices and reasoning settings from your proxy. It merges into your existing config instead of overwriting it, backs up every file before changing it, and `liteton uninstall` removes only what it added.

| Harness | What liteton configures |
|---|---|
| **VSCode** (Copilot Chat) | A `litellm` custom endpoint in `chatLanguageModels.json`. The API key goes into VSCode's encrypted secret storage. |
| **opencode** | A `litellm` provider in `opencode.jsonc`/`opencode.json`. The API key goes into opencode's `auth.json`. |
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
| `--harness <vscode\|opencode\|cursor>...` | Harnesses to configure. Without it, liteton asks and preselects the ones it detects. Cursor is never preselected. |
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

- Writes the `litellm` entry to every VSCode profile's `chatLanguageModels.json`: `~/Library/Application Support/Code/User/chatLanguageModels.json` for the Default profile, and `…/Code/User/profiles/<id>/chatLanguageModels.json` for each other profile. The profile list comes from VSCode's `User/globalStorage/storage.json`. Profiles set to use the Default profile's language models are skipped, because VSCode reads the Default file for them.
- Existing files are merged: comments, formatting and your other entries are kept. If a `litellm` entry already exists, liteton merges into it and reuses its secret. Profiles without the file get one, and all new entries share one secret.
- Stores the API key in VSCode's secret storage (`state.vscdb`), encrypted the same way VSCode does it, with the "Code Safe Storage" password from the Keychain.
- Open VSCode once before installing, so that `state.vscdb` exists.

### opencode

- Writes `provider.litellm` to `~/.config/opencode/opencode.jsonc` (or `opencode.json` if that is the file you use), including costs, limits and reasoning variants.
- Adds the API key to `~/.local/share/opencode/auth.json` and keeps all other entries. The file is written with mode `0600`.
- `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are respected.

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

For reasoning models, liteton offers the levels `none`, `low`, `medium`, `high` and `xhigh` (VSCode `supportsReasoningEffort`, opencode `variants`). You can change them in `~/.config/liteton/config.toml`, for all models or per model:

```toml
base_url = "https://litellm.example.com"
reasoning_efforts = ["low", "medium", "high"]

[model_reasoning_efforts]
"azure/gpt-5-mini" = ["minimal", "low", "medium", "high"]
```

Run `liteton install` again afterwards to apply the change.

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
| `src/harness/` | One module per harness; each produces a plan of changes that `apply.rs` writes |
| `src/vscdb/` | Reading/writing `state.vscdb` and Electron safeStorage encryption |
| `src/ui/` | Prompts, install flow, tables, dashboard |

## License

[Apache License 2.0](LICENSE)
