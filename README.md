![Windows](https://img.shields.io/badge/platform-Windows-blue)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

# Claude Code Usage Monitor

![Screenshot](.github/animation.gif)

A lightweight Windows taskbar widget for people already using Claude Code, with optional Codex, Google Antigravity, Kimi, and GitHub Copilot usage display.

It sits in your taskbar and shows how much of your Claude Code, Codex, Antigravity, and/or Kimi usage window you have left, without needing to open the terminal or the provider site.

## What You Get

- A **5h** bar for your current 5-hour Claude usage window
- A **7d** bar for your current 7-day window
- Optional Codex usage bars alongside Claude Code
- Optional Antigravity model usage bars for Google's 5-hour and weekly Gemini quota windows
- Optional Kimi usage bars for the Kimi For Coding 5-hour and 7-day limit windows
- Optional GitHub Copilot bars for AI credits consumed and monthly budget spend
- A live countdown until each limit resets
- Configurable refresh interval, from 15 seconds to 1 hour
- A small native widget that lives directly in the Windows taskbar
- System tray icon badges showing your enabled model usage percentage
- Left-click the tray icon to toggle the taskbar widget on or off
- Right-click options for refresh, displayed models, bar length, update frequency, language, startup, widget visibility, and updates
- Multi-monitor taskbar placement, so the widget can live on the taskbar for the screen you prefer

## Who This Is For

This app is for Windows users who already have **Claude Code (CLI or App) installed and signed in**.

Codex support is optional. To show Codex usage, install and sign in to the Codex CLI, then enable Codex from the right-click **Models** menu.

Antigravity support is optional too. To show Antigravity usage, install and sign in to Google Antigravity, then enable the **Antigravity** model from the right-click **Models** menu.

Kimi support is optional as well, and is the only provider that needs manual setup, because Kimi has no local CLI credential store to read. See [Kimi Setup](#kimi-setup) below.

It works best if you want a simple "how close am I to the limit?" display that is always visible.

## Requirements

- Windows 10 or Windows 11
- Claude Code (CLI or App) installed and authenticated
- Optional: Codex CLI installed and authenticated, if you want Codex usage
- Optional: Google Antigravity installed and authenticated, if you want Antigravity usage
- Optional: A Kimi For Coding subscription and a one-time token setup, if you want Kimi usage
- Optional: A GitHub Copilot seat, if you want Copilot usage

If you use Claude Code through WSL, that is supported too. The monitor can read your Claude Code credentials from Windows or from your WSL environment.

## Install

Install the latest version from WinGet:

```powershell
winget install CodeZeno.ClaudeCodeUsageMonitor
```

If you prefer not to use WinGet, you can still download the latest `claude-code-usage-monitor.exe` from the [Releases](https://github.com/CodeZeno/Claude-Code-Usage-Monitor/releases) page and run it directly.

## Use

After installing with WinGet, run:

```powershell
claude-code-usage-monitor
```

Once running, it will appear in your taskbar and as one or more tray icons in the notification area.

- Drag the left divider to move the taskbar widget
- On multi-monitor setups, drag the widget onto another Windows taskbar to move it to that screen
- Right-click the taskbar widget or tray icon for refresh, displayed models, bar length, update frequency, Start with Windows, reset position, language, updates, and exit
- Left-click the tray icon to toggle the taskbar widget on or off
- Enable `Start with Windows` from the right-click menu if you want it to launch automatically when you sign in

### Models

Use the right-click **Models** menu to choose what the widget displays:

- **Claude Code** is enabled by default
- **Codex** can be enabled alongside Claude Code or shown by itself
- **Antigravity** can be enabled alongside the other providers or shown by itself as its own model column
- **Kimi** can be enabled alongside the other providers or shown by itself, once it has been set up
- **Copilot** can be enabled alongside the other providers or shown by itself

When multiple models are shown, each model has its own usage bar and matching usage text color. Antigravity prefers Google's Gemini quota summary when available and falls back to model quota data when needed. Kimi maps its 5-hour and 7-day coding rate limits onto the same two bars.

### Kimi Setup

Unlike the other providers, Kimi has no local CLI credential file to read, so the refresh token has to be supplied once by hand.

1. Sign in at [kimi.com](https://www.kimi.com) in your browser.
2. Open DevTools (F12) and run this in the Console:

   ```js
   copy(localStorage.getItem('refresh_token'))
   ```

3. Create `%APPDATA%\ClaudeCodeUsageMonitor\kimi.json` and paste the token in:

   ```json
   {
     "refresh_token": "PASTE_TOKEN_HERE"
   }
   ```

4. Enable **Kimi** from the right-click **Models** menu.

The app exchanges that refresh token for a short-lived access token and caches it in the same file. Kimi refresh tokens last about 90 days and roll forward each time they are used, so this normally only needs doing once. If the widget starts showing `!` for Kimi, repeat the steps above with a fresh token.

Treat `kimi.json` like a password: the refresh token grants access to your Kimi account, and the file is not encrypted.

### Bar Length

Each provider's usage bar is drawn as a row of segments. By default the widget picks the length automatically, shrinking the bars as you enable more providers so the whole thing still fits on the taskbar:

| Providers shown | Segments each |
| --- | --- |
| 1 | 10 |
| 2 | 5 |
| 3 or more | 4 |

To override this, right-click the widget and choose **Bar Length**, then pick `5`, `10`, `15`, or `20`. The chosen length applies to every provider, and the widget resizes itself to fit. Pick **Auto** to go back to the automatic behaviour.

Longer bars give finer visual resolution but take more taskbar space, so 20 segments across several providers can get wide.

### Copilot Setup

Copilot works differently from the other providers, because GitHub does not expose a rate-limit window for it. The two bars show:

| Row | Shows |
| --- | --- |
| Top (5h) | AI credits consumed this billing month |
| Bottom (7d) | Organization budget spent, as a percentage, with the dollar amount in place of the countdown |

**The credits row works with no setup.** It reads the Copilot OAuth token that the editor extensions already wrote to `%LOCALAPPDATA%\github-copilot\apps.json` when you signed in.

However, GitHub reports `entitlement: 0` and `unlimited: true` for token-based billing seats, and publishes no included-credit allowance through any API. Without a denominator the bar cannot fill, so it shows the raw count instead, like `0% - 719cr`.

To get a real percentage, find your allowance under *Organization settings -> Billing -> Included usage and credits*, which lists a line like `Included AI credits: 3,000 credits included`. Put that number in `%APPDATA%\ClaudeCodeUsageMonitor\copilot.json`:

```json
{
  "included_credits": 3000
}
```

The allowance scales with your seat count, so re-check it if seats are added or removed.

**The budget row needs a personal access token.** Create a fine-grained token at `github.com/settings/personal-access-tokens/new`:

- **Resource owner**: your organization (not your personal account)
- **Organization permissions -> Administration**: Read-only
- **Organization permissions -> GitHub Copilot Business**: Read-only

Your organization must also allow fine-grained tokens, under *Organization settings -> Personal access tokens*. Then add the token and org to the same file:

```json
{
  "token": "github_pat_...",
  "org": "your-org-name",
  "included_credits": 3000
}
```

The budget row reads the Copilot budget you configured under *Organization settings -> Billing -> Budgets and alerts*, and divides this month's Copilot spend by it. If no budget exists, the bar stays empty and only the dollar amount shows.

Note that billing figures are organization-wide. If your organization has several Copilot seats, the budget row reflects everyone's spend, not just yours.

Treat `copilot.json` like a password: the token grants read access to your organization's billing, and the file is not encrypted.

### System Tray Icon

The tray icon shows your current 5-hour usage as a percentage badge.

If multiple providers are enabled, the app shows one tray icon per provider. If only one model is enabled, it shows one tray icon.

The Claude Code tray icon uses the same warm usage colors as the Claude bar. The Codex tray icon uses a black and white badge style. The Antigravity tray icon uses a blue badge style. The Kimi tray icon uses a purple badge style. The Copilot tray icon uses a green badge style.

Hovering over a tray icon shows the usage values for that model.

## Diagnostics

If you need to troubleshoot startup or visibility issues, run:

```powershell
claude-code-usage-monitor --diagnose
```

This writes a log file to:

```text
%TEMP%\claude-code-usage-monitor.log
```

Settings are saved to:

```text
%APPDATA%\ClaudeCodeUsageMonitor\settings.json
```

## Account Support

This app works with the same account types that Claude Code itself supports.

As of **March 19, 2026**, Anthropic's Claude Code setup documentation says:

- **Supported:** Pro, Max, Teams, Enterprise, and Console accounts
- **Not supported:** the free Claude.ai plan

If Anthropic changes Claude Code availability in the future, this app should follow whatever Claude Code supports, as long as the usage data remains exposed through the same authenticated endpoints.

## Privacy And Security

This project is **open source**, so you can inspect exactly what it does.

What the app reads:

- Your local Claude Code OAuth credentials from `~/.claude/.credentials.json`
- If needed, the same credentials file inside an installed WSL distro
- If Codex is enabled, your local Codex credentials from `$CODEX_HOME/auth.json` or `~/.codex/auth.json`
- If Antigravity is enabled, your local Antigravity OAuth token from Windows Credential Manager target `gemini:antigravity`
- If Kimi is enabled, the refresh token you saved in `%APPDATA%\ClaudeCodeUsageMonitor\kimi.json`
- If Copilot is enabled, your local Copilot OAuth token from `%LOCALAPPDATA%\github-copilot\apps.json`, and the optional personal access token you saved in `%APPDATA%\ClaudeCodeUsageMonitor\copilot.json`

What the app sends over the network:

- Requests to Anthropic's Claude endpoints to read your usage and rate-limit information
- Requests to ChatGPT's Codex usage endpoint to read your Codex usage and rate-limit information, if Codex is enabled
- Requests to Google's Cloud Code / Antigravity endpoints to read your Antigravity quota information, if Antigravity is enabled
- Requests to Kimi's token refresh and subscription stats endpoints to read your Kimi rate-limit information, if Kimi is enabled
- Requests to GitHub's Copilot quota and organization billing endpoints to read your credit and budget usage, if Copilot is enabled
- Requests to GitHub only if you use the app's update check / self-update feature
- If proxy environment variables such as `HTTPS_PROXY`, `HTTP_PROXY`, or `ALL_PROXY` are set, those outbound requests may use that proxy

What the app stores locally:

- Widget position
- Selected taskbar / screen
- Widget visibility
- Polling frequency
- Bar length
- Language preference
- Last update check time
- Displayed model preferences
- If Kimi is enabled, your Kimi refresh token and a cached access token, in `kimi.json` (unencrypted)
- If Copilot budget display is enabled, your GitHub personal access token, in `copilot.json` (unencrypted)

What it does **not** do:

- It does not send your credentials to any other server
- It does not use a separate backend service
- It does not collect analytics or telemetry
- It does not upload your project files
- It does not directly edit your Codex credentials file

Notes:

- If your Claude Code token is expired, the app may ask the local Claude CLI to refresh it in the background
- If your Codex token is expired, the app may ask the local Codex CLI to refresh it in the background. The monitor does not write `auth.json` itself; any credential update is handled by the Codex CLI.
- If your Antigravity token is expired, open Antigravity and sign in again. The monitor does not write Windows Credential Manager entries itself.
- If your Kimi token is expired, save a fresh refresh token in `kimi.json`. Unlike the other providers, the app does write this file, to store the rotated refresh token and the cached access token.
- Portable installs can update themselves by downloading the latest release from this repository
- Proxies should be trusted because proxied usage requests include your OAuth bearer token inside the TLS connection

## How It Works

The monitor:

1. Finds your enabled model login credentials
2. Reads your current usage from Anthropic, ChatGPT, Google's Antigravity, Kimi, and/or GitHub endpoints
3. Shows the result directly in the Windows taskbar
4. Keeps the widget aligned with the selected taskbar and tray area
5. Refreshes periodically in the background

If the newer usage endpoint is unavailable, it can fall back to reading the rate-limit headers returned by Claude's Messages API.

## Open Source

This project is licensed under MIT.

If you want to inspect the behavior or audit the code, everything is in this repository.
