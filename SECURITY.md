# Security Policy

`audible-rs` handles authentication material for your Audible/Amazon
account (signing keys, access/refresh tokens, website cookies). Security
is therefore a first-class concern, even at this early stage. Thank you
for helping keep it safe.

## Status

This project is in its **alpha** phase (pre-1.0, tagged pre-releases).
Only the **latest release** is supported: security fixes land on the
`main` branch and ship with the next release — there are no back-ports
to older versions. This policy will be tightened as the project matures.

## Reporting a vulnerability

**Please do not report security issues in public issues, pull requests
or discussions.**

Report privately via GitHub's **[Private Vulnerability
Reporting](https://github.com/mkb79/audible-rs/security/advisories/new)**
(the "Report a vulnerability" button under the repository's *Security*
tab). If that is unavailable to you, email `mkb79@hackitall.de` with the
details.

Please include:

- a description of the issue and its impact,
- the steps or a minimal proof of concept to reproduce it,
- the affected commit/version and your platform,
- **never** real credentials, tokens or auth files — redact them, or use
  a synthetic reproduction.

This is a solo, unpaid, spare-time project, so responses are best-effort:
expect an acknowledgement within a couple of weeks. Coordinated
disclosure is appreciated — please give a reasonable window for a fix
before publishing.

## Scope

**In scope** — issues in this codebase, for example:

- leakage of credentials, tokens, keys or cookies (in logs, error
  messages, output, exports, the database or on-disk files),
- weaknesses in the auth-file encryption envelope or key handling,
- authentication/authorization flaws in the session agent or plugin
  broker (token scoping, account binding, the admin/TCP boundary),
- the plugin capability model failing to contain what a plugin can do
  **through this tool** (RPC scopes, selector inheritance).

**Out of scope** — for example:

- using the tool against an Audible/Amazon account you do not own, or in
  violation of Audible's terms of service (see the disclaimer in the
  README),
- OS-level sandboxing of a plugin process's *own* network or filesystem
  behaviour: the capability model governs what a plugin can do through
  `audible`, but a plugin is still an ordinary process on your machine —
  only run plugins you trust,
- vulnerabilities in third-party dependencies (report those upstream;
  Dependabot tracks them here),
- attacks that require an already-compromised local account with read
  access to your config/auth directory.

## What the tool already does

For context, some defensive properties by design:

- auth material is stored in an encrypted envelope (Argon2id +
  XChaCha20-Poly1305) by default,
- credentials never appear in logs, errors or output at any verbosity
  level; secret-bearing files are created with restrictive permissions,
- plugins receive capabilities, not secrets — no token, key or auth path
  leaves the host process,
- the session agent's admin surface is not reachable over TCP, and app
  tokens are scoped, optionally account-bound, and auditable.
