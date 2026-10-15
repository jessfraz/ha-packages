# Security

Report vulnerabilities privately through
[GitHub security advisories](https://github.com/jessfraz/ha-packages/security/advisories/new).
Include the affected version, a reproduction, and the impact. Never include
real API keys, tracking numbers, addresses, or database contents.

The latest release receives security fixes. Keep the service and Home
Assistant updated.

The owner API token grants full access to the local registry. Give automated
clients the separate agent token and rotate either token if exposed. The
dashboard trusts Home Assistant ingress only on its separate listener, which
accepts the Supervisor gateway as its peer.

The API uses HTTP for local app networking. Use a TLS proxy for traffic that
leaves a trusted network. Keep the database, options file, and backups private.
EasyPost credentials stay on the server.
