# wled-mcp

MCP server for controlling [WLED](https://kno.wled.ge/) LED controllers. Discovers devices on the local network via mDNS and exposes tools for managing state, presets, effects, and schedules.

## Running

### Public (no auth)

```sh
cargo run
```

### OAuth

```sh
AUTH_TYPE=oauth CLIENT_ID=my-client CLIENT_SECRET=my-secret cargo run
```

Claude will go through the OAuth flow automatically -- authorize auto-approves (no user interaction), and tokens last 365 days.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `AUTH_TYPE` | `public` | `public` or `oauth` |
| `CLIENT_ID` | -- | Required when `AUTH_TYPE=oauth` |
| `CLIENT_SECRET` | -- | Required when `AUTH_TYPE=oauth` |
| `BIND_ADDRESS` | `0.0.0.0:3000` | Address to bind |
| `BASE_URL` | `http://{BIND_ADDRESS}` | Public URL (set this when behind a reverse proxy) |
| `RUST_LOG` | `wled_mcp=info` | Log level |

## Endpoints

| Path | Description |
|---|---|
| `/` | Landing page |
| `/mcp` | MCP endpoint (point Claude here) |
| `/.well-known/oauth-authorization-server` | OAuth metadata (oauth mode) |
| `/.well-known/oauth-protected-resource` | Protected resource metadata (oauth mode) |
| `/oauth/authorize` | Authorization endpoint (oauth mode) |
| `/oauth/token` | Token endpoint (oauth mode) |
| `/oauth/register` | Dynamic client registration (oauth mode) |
