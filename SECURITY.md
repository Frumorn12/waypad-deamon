# Security Policy

Waypad-daemon accepts network traffic from the local network, so security issues are high priority.

## Supported Versions

The project is pre-1.0. Security fixes are made on the main development branch until a formal release process exists.

## Reporting Vulnerabilities

Do not publish exploitable details before maintainers have time to respond. Open a private advisory if the hosting platform supports it, or contact the repository owner directly.

Include:

- Affected commit or release.
- Steps to reproduce.
- Whether pairing, authentication, encryption, replay protection, storage permissions, or command authorization are involved.
- Logs with secrets redacted.

## Security Model Summary

- The daemon only accepts control commands after encrypted channel setup and device authentication.
- Pairing requires a one-time code generated locally on the Linux host with `waypad-daemon pair-code`.
- Host identity is pinned by the Android app. A changed fingerprint requires re-pairing.
- Trusted-device tokens are random 256-bit bearer tokens and are stored hashed on the host.
- Host state is stored under the user state directory with `0700` directories and `0600` files on Unix.
- Encrypted frames use AES-GCM with monotonically increasing per-direction sequence numbers.
- Pairing attempts are rate limited per source IP.

## Known MVP Risks

- The protocol is intentionally simple and should receive third-party review before broad distribution.
- Manual pairing relies on the user comparing the host fingerprint when discovery is unavailable.
- Device tokens are bearer credentials. Revocation is available with `waypad-daemon devices revoke <id>`.
- Remote input depends on the Wayland RemoteDesktop portal and user approval. The daemon does not bypass compositor security.
