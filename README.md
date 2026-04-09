# wled-mcp

MCP server for controlling [WLED](https://kno.wled.ge/) LED controllers. Discovers devices on the local network via mDNS and exposes tools for managing state, presets, effects, and schedules.

**[Watch the demo](https://www.youtube.com/watch?v=1tnA_GU2660)** — click the image below to see it in action.

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

| Variable | Description | Default |
|---|---|---|
| `AUTH_TYPE` | `public` or `oauth` | `public` |
| `OAUTH_CLIENT_ID` | Required when `AUTH_TYPE=oauth` | -- |
| `OAUTH_CLIENT_SECRET` | Required when `AUTH_TYPE=oauth` | -- |
| `OAUTH_ALLOWED_REDIRECT_URIS` | Comma-separated allowlist of redirect URIs. Patterns ending in `*` are prefix matches. | See below |
| `BIND_ADDRESS` | Address to bind | `0.0.0.0:8080` |
| `RUST_LOG` | Log level | `wled_mcp=info` |

Default `OAUTH_ALLOWED_REDIRECT_URIS`:
```
https://chatgpt.com/connector/oauth/*,https://claude.ai/api/mcp/auth_callback,https://claude.com/api/mcp/auth_callback
```

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

## Future Improvements

Contributions welcome! Here are some areas that could use attention:

- **Docker image / `cargo install` support** — make it easier to run without cloning the repo and building from source.
- **Rate limiting** — the OAuth endpoints have no built-in rate limiting. Fine behind a reverse proxy that handles it, but a standalone deployment could benefit from this.
- **Effects** — experiment with having MCP set effects on patterns (it might be ready for prime time, but I haven't played with it).

## License

[MIT](LICENSE)

---

This project was made possible by [Mozr](https://mozr.com/), where we help businesses design and implement AI-powered integrations and workflows. Have a project in mind? [Let's talk.](https://mozr.com/)
