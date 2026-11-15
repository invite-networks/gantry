# Security Policy

## Supported versions

Gantry is in 0.x rapid development. Only the latest release receives
security fixes.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it
privately through GitHub instead:

1. Go to the repository's **Security** tab.
2. Choose **Report a vulnerability**.

Include the Gantry version (`gantry version`), the runtime and version you
use, and the steps to reproduce. We aim to acknowledge reports within three
business days and will keep you updated until a fix is released.

Gantry runs as root and manages host networking and firewalls, so reports
about privilege boundaries, namespace escapes, firewall bypasses and the
`gantry-oci` runtime shim are especially welcome.
