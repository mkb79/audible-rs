# The auth-file password

The auth file is encrypted by default, so every command that talks to
Audible needs its passphrase. Each `audible` run is a separate process,
which by default means entering the passphrase every time. Four sources
are available:

| Source | Where the passphrase comes from |
| --- | --- |
| `prompt` | Requested interactively. The default; nothing is stored. |
| `env` | `AUDIBLE_AUTH_PASSWORD_<NAME>`, falling back to `AUDIBLE_AUTH_PASSWORD`. |
| `command` | The stdout of a nominated command. |
| `file` | A `0600` `authorized_keys`-style file (`passwords` in the config dir), keyed by account. The passphrase is stored in plaintext. |

Each source is configured per account — `password_source`,
`password_command` and `password_file` all live under
`[accounts.<name>]` — but the command takes no account argument. It acts
on the **selected** account, which is resolved from the global
`-a/--account` flag, then `AUDIBLE_ACCOUNT`, then `default_account`, then
the sole account if only one is configured:

```bash
audible account password source <prompt|env|command|file>   # selected account
audible account password source <prompt|env|command|file> -a <account>
```

## Environment variables

`AUDIBLE_AUTH_PASSWORD_<NAME>` is checked first, then
`AUDIBLE_AUTH_PASSWORD` as a fallback, so several accounts can each carry
their own passphrase with one shared default. `<NAME>` is the account name
uppercased with `-` replaced by `_` — account `book-club` reads
`AUDIBLE_AUTH_PASSWORD_BOOK_CLUB`.

These variables are only consulted when the source is `env`. Exporting one
while an account is still on the default `prompt` source has no effect on
`library`, `download` and the other data commands, and prompting
continues. The `account` maintenance commands are the exception: they also
accept the variables as a non-interactive fallback on a `prompt` account.

## Using an OS keychain or password manager

`command` mode is the general-purpose hook: any command that prints a
secret to stdout is accepted, which covers the mainstream secret stores
without audible-rs integrating with them individually. Each recipe below
stores the passphrase once, then points the account at it.

`audible account password source command` verifies that the command
opens the auth file before saving anything, so an incorrect command fails
immediately rather than locking the account.

Throughout the recipes, `<account>` is the audible-rs account name. It
selects the account via `-a`, and is reused as the secret store's own key
so that several accounts can keep separate entries; the store does not
require that they match.

### macOS — Keychain

`security` is built in. Passing `-w` as the final option prompts for the
passphrase, keeping it out of shell history:

```bash
security add-generic-password -s audible-rs -a <account> -U -w
audible account password source command -a <account> \
  --command 'security find-generic-password -w -s audible-rs -a <account>'
```

### Linux — GNOME Keyring or KWallet

Both speak the Secret Service API, so `secret-tool` addresses either.
The package is `libsecret-tools` on Debian/Ubuntu and `libsecret` on Arch
and Fedora:

```bash
secret-tool store --label='audible-rs' service audible-rs account <account>
audible account password source command -a <account> \
  --command 'secret-tool lookup service audible-rs account <account>'
```

`secret-tool` requires a D-Bus session and an unlocked keyring, so it may
fail or block over SSH and in containers.

### Linux — pass

The standard Unix password store is GPG-backed and works over SSH via
`gpg-agent`:

```bash
pass insert audible-rs
audible account password source command -a <account> \
  --command 'pass show audible-rs | head -n1'
```

### Windows — PowerShell SecretManagement

The `SecretManagement` and `SecretStore` modules are Microsoft-maintained.
`SecretStore` keeps its own encrypted vault; adding the
`SecretManagement.Windows.CredMan` extension targets Credential Manager
instead:

```powershell
Install-Module Microsoft.PowerShell.SecretManagement, Microsoft.PowerShell.SecretStore -Scope CurrentUser
Set-Secret -Name audible-rs -Secret (Read-Host -AsSecureString)
audible account password source command -a <account> `
  --command 'pwsh -NoProfile -Command "Get-Secret -Name audible-rs -AsPlainText"'
```

Credential Manager has no built-in command that reads a stored password
back — `cmdkey` writes but cannot retrieve — which is why the modules
above are the route to it.

### Windows — DPAPI

This needs no additional modules. `ConvertFrom-SecureString` encrypts to
the current Windows user, so only that user can read the file back.
`-AsPlainText` on `ConvertFrom-SecureString` requires PowerShell 7 or
later:

```powershell
Read-Host -AsSecureString | ConvertFrom-SecureString | Set-Content "$env:APPDATA\audible\pass.dpapi"
# then, as the password_command:
pwsh -NoProfile -Command "Get-Content $env:APPDATA\audible\pass.dpapi | ConvertTo-SecureString | ConvertFrom-SecureString -AsPlainText"
```

### Any platform — 1Password CLI

[`op`](https://developer.1password.com/docs/cli/) supports desktop-app
integration for Touch ID and Windows Hello unlock. The following assumes
the password has been stored in an item called `audible-rs` in the `Private`
vault.

```bash
audible account password source command -a <account> \
  --command 'op read "op://Private/audible-rs/password"'
```

Bitwarden (`bw get password audible-rs`, which needs `BW_SESSION`) and
KeePassXC (`keepassxc-cli show -a Password -q vault.kdbx audible-rs`)
follow the same pattern.
