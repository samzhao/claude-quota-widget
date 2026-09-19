# Design notes

How the app works and why it is built this way. For what it is, see the [README](../README.md).

## Decisions

| Topic | Decision |
|-------|----------|
| Stack | Tauri 2: Rust backend, plain TypeScript frontend, no UI framework |
| Login | Drive the official CLI: `claude auth login --claudeai` with `CLAUDE_CONFIG_DIR` pointed at a throwaway temp folder |
| Token storage | App-owned macOS keychain service `Claude Quota Widget`, one item per account, keyed by an app-generated UUID |
| Token renewal | The app renews the logins it owns. Nothing else holds those grants, so there are no rotation races |
| Claude Code's own login | Shown as a read-only row. Never written, renewed or deleted, because Claude Code owns that grant |
| Pacing | A disk-backed cache decides when the network may be touched |

## Things learned the hard way

1. **Usage is per token, not per "active" account.** Any valid login can be asked for its own usage, so every account can be shown at once.
2. **The `limits` array is the real source.** It has `session` (5-hour), `weekly_all`, and `weekly_scoped` entries carrying `scope.model.display_name` (for example Fable). Model-scoped limits have no top-level field. Other top-level keys are unlabeled internal names and are ignored.
3. **An idle 5-hour window reports 0% with no reset time.** So "0% and no reset" cannot be used on its own to decide a limit is unused.
4. **The usage endpoint throttles quickly.** A handful of calls in a few minutes is enough for a 429, with a `Retry-After` of a minute or two.
5. **Refresh tokens rotate.** The token endpoint returns a new refresh token, so the response must be stored before the new access token is used. Losing it logs the account out.
6. **Keychain item naming.** Claude Code stores the default login under `Claude Code-credentials`, and a login for any other config folder under `Claude Code-credentials-<first 8 hex of sha256(config folder path)>`, with the macOS username as the keychain account.
7. **The CLI login needs stdin held open.** Its browser callback listener is tied to stdin, so closing it early ends the login.

## Modules

```
src-tauri/src/
  accounts.rs     list of managed accounts (labels only, no secrets)
  keychain.rs     read / write / delete through the `security` CLI
  credentials.rs  the OAuth blob, kept as raw JSON so unknown fields survive
  login.rs        temp folder -> claude auth login -> lift tokens -> clean up
  oauth.rs        renew an access token, merge the rotated refresh token
  usage.rs        fetch and parse usage limits
  cache.rs        last reading per account, plus all pacing and backoff rules
  labels.rs       free-text label per account (also for the read-only default login)
  machines.rs     optional SSH check of which account other machines are signed in to
  lib.rs          Tauri commands tying the above together
src/
  main.ts         grid and cards views, settings, scheduling the next check
  styles.css      two themes: warm and instrument
```

## Flows

Add account:
1. Make a private temp folder and run the CLI login with it as the config folder. The browser opens.
2. When the CLI exits cleanly, read the identity from `claude auth status --json` and the tokens from the scoped keychain item.
3. Store the tokens under the app's own keychain service.
4. Always delete the scoped keychain item and the temp folder, whether or not the login worked.
5. Adding an email that is already listed replaces its tokens instead of adding a second row.

Check usage:
1. For each managed account whose token expires within 5 minutes, renew and store it first.
2. Ask the cache whether each account is due. Only due accounts touch the network, in parallel.
3. Record the result. A throttled or failed check keeps the previous reading on screen.

## Pacing rules

- A reading younger than 5 minutes is reused. The Refresh button lowers that to 1 minute.
- A first 429 waits for the server's `Retry-After`. Repeated 429s fall back to a doubling backoff from 5 to 30 minutes.
- Other failures back off from 1 to 15 minutes.
- A rejected token is not retried in a loop, and a dead refresh token stops renewal until the account is added again.
- The cache is saved to disk, so restarting the app costs no requests.

## Other machines

Opt-in. For each configured machine the app runs one `ssh -o BatchMode=yes … -- <destination> 'sh -s'` and feeds a fixed probe script on stdin.

- The script runs under plain `sh` on purpose: a zsh login shell aborts on an unmatched `claude-*` glob.
- It runs `claude auth status --json` for `~/.claude` and for each profile folder that Claude Code has actually written a config into, then maps running `claude` processes to a profile through their `CLAUDE_CONFIG_DIR` environment.
- The destination is validated to a host-safe alphabet, may not start with `-`, and is passed as a single argument after `--`, so it cannot become an ssh option or reach a shell.
- A machine that cannot be reached drops its badges rather than showing stale "in use" marks. Failures are translated into a plain sentence with the fix (untrusted host key, no key login, unreachable).
- Accounts are matched to sightings by email, case-insensitively.

## Security notes

- Keychain access goes through `/usr/bin/security`. Items it creates trust `security` itself, so rebuilt development binaries do not trigger keychain prompts.
- Writes use `security -i` over stdin, so a token never appears in a process argument list. Values are base64 and restricted to a quote-free alphabet so the command cannot be broken out of.
- The delete helper refuses the default `Claude Code-credentials` service outright.
- Error responses are reduced to a status code. Bodies are never shown or logged, in case they echo request details.
- The frontend receives labels, percentages and timestamps only.

## Risks

The usage endpoint, token endpoint and OAuth client id are undocumented Claude Code internals. They can change without notice. Each lives in exactly one file (`usage.rs`, `oauth.rs`) so a change is a one-line fix.
