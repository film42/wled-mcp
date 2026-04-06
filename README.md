# wled-mcp

MCP server for controlling [WLED](https://kno.wled.ge/) LED controllers. Discovers devices on the local network via mDNS and exposes tools for managing state, presets, effects, and schedules.

[![Demo video](https://img.youtube.com/vi/1tnA_GU2660/maxresdefault.jpg)](https://www.youtube.com/watch?v=1tnA_GU2660)

## Running

### Public (no auth)

```sh
cargo run
```

### OAuth

```sh
AUTH_TYPE=oauth OAUTH_CLIENT_ID=my-client OAUTH_CLIENT_SECRET=my-secret cargo run
```

The OAuth flow is fully stateless — tokens and auth codes are HMAC-signed with the client secret, so there is no server-side session state. You can restart or scale out without invalidating tokens.

Authorization auto-approves (no user interaction). Access and refresh tokens expire after 365 days.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `AUTH_TYPE` | `public` | `public` or `oauth` |
| `OAUTH_CLIENT_ID` | -- | Required when `AUTH_TYPE=oauth` |
| `OAUTH_CLIENT_SECRET` | -- | Required when `AUTH_TYPE=oauth` |
| `OAUTH_ALLOWED_REDIRECT_URIS` | `https://chatgpt.com/connector/oauth/*,https://claude.ai/api/mcp/auth_callback,https://claude.com/api/mcp/auth_callback` | Comma-separated allowlist of redirect URIs. Patterns ending in `*` are prefix matches. |
| `BIND_ADDRESS` | `0.0.0.0:3000` | Address to bind |
| `RUST_LOG` | `wled_mcp=info` | Log level |

The server derives its public URL from request headers (`X-Forwarded-Proto`, `X-Forwarded-Host`, `Host`), so no explicit base URL configuration is needed when running behind a reverse proxy like Caddy or nginx.

## Endpoints

| Path | Description |
|---|---|
| `/` | Landing page |
| `/mcp` | MCP endpoint (point Claude here) |
| `/.well-known/oauth-authorization-server` | OAuth metadata (oauth mode) |
| `/.well-known/oauth-protected-resource` | Protected resource metadata (oauth mode) |
| `/oauth/authorize` | Authorization endpoint (oauth mode) |
| `/oauth/token` | Token endpoint (oauth mode) |
