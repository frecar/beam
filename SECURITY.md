# Security Policy

> **Disclosure**: This project has not undergone a formal security audit. The measures below represent best-effort hardening by the maintainers. If deploying in a sensitive environment, consider commissioning an independent audit.

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Please report vulnerabilities using [GitHub's private vulnerability reporting](https://github.com/frecar/beam/security/advisories/new). This ensures the report is visible only to maintainers until a fix is available.

Include:
- Description of the vulnerability
- Steps to reproduce
- Affected versions
- Potential impact

We aim to acknowledge reports within 48 hours and provide a fix or mitigation within 7 days for critical issues.

## Security Model

Beam is a remote desktop server. Its security model reflects this:

**Authentication**: Linux PAM. Beam does not manage its own user accounts — it delegates to the operating system. Login attempts are rate-limited per-username (5 per 60 seconds) and per-IP (20 per 60 seconds). Only failed attempts count against the limit.

**Session isolation**: Each user gets an isolated virtual X display. Agent processes run as the authenticated user after privilege dropping (`initgroups` -> `setgid` -> `setuid`).

**Transport**: All traffic flows over a single TLS WebSocket connection — video frames, audio frames, and input events. There is no peer-to-peer media path; all data is relayed through the server. A self-signed certificate is auto-generated if no cert is configured.

**Tokens**: JWT (24h expiry, auto-refresh) for session management. Agent and release tokens use CSPRNG generation with constant-time comparison.

**Input sanitization**: Usernames are validated against a strict allowlist. File transfers are jailed to the user's home directory with symlink detection and path traversal prevention. Clipboard content is stripped of terminal control characters.

## Architecture Considerations

The server process runs as root because it needs to:
- Authenticate users via PAM (requires root or specific PAM configuration)
- Spawn agent processes as different users (`setuid`/`setgid`)
- Bind to privileged ports (if configured below 1024)

The systemd unit includes hardening directives (`ProtectSystem`, `ProtectKernelTunables`, `ProtectKernelModules`, etc.) to limit the attack surface despite running as root.

## Out of Scope

The following are known limitations, not vulnerabilities:
- Self-signed TLS certificate warnings in browsers (expected when no cert is configured)
- Local users on the same machine can see agent process environment variables (standard Unix behavior)
- The server binds to `0.0.0.0` by default — use a firewall or set `bind` in `beam.toml` to restrict access
- Rate limiting uses in-memory counters (reset on server restart)

## Audit Status

This project has not undergone a formal security audit. If you are evaluating Beam for sensitive environments, please review the source code directly and consider commissioning an independent audit.

## Supported Versions

Security fixes are applied to the latest release only. We recommend always running the latest version.
