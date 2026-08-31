# mvwr

A single-binary, simple media server.

Run it bound to a closed-off interface (e.g. Tailscale) and you don't have to worry about auth. A simple cookie check exists anyway, as defense in depth.

## Running

### Directly

```bash
cargo run --release
```

Example - serve `./media` on `127.0.0.1:3344` with cookie auth:

```bash
BIND_ADDRESS=127.0.0.1 PORT=3344 \
AUTH_COOKIE_NAME=X-Hot-Dog AUTH_COOKIE_VALUE=yummy canis
```

Add `ANALYTICS_TAG='<script async src="https://analytics.example.com/script.js" data-website-id="xxxx"></script>'` to inject a snippet into `<head>` on every page.

### Docker Compose

```bash
cp .env.example .env   # set MEDIA_HOST_PATH, PUBLIC_BASE_URL, TUNNEL_TOKEN
docker compose up -d --build
```

The stack deliberately publishes no ports: canis and cloudflared share an internal network, so the tunnel is the only way in.

### Cloudflare Tunnel

`compose.yml` already runs a cloudflared connector - just set `TUNNEL_TOKEN` (a Zero Trust managed tunnel token) in `.env`. The cookie auth then acts as a second factor behind Cloudflare Access.

For a locally-managed tunnel instead, follow `cloudflared/config.yml`: create the tunnel, route your DNS, drop the credentials JSON into `./cloudflared/credentials.json`, then point the ingress at `http://canis:3000`.

## Configuration

Everything is environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `BIND_ADDRESS` | `127.0.0.1` | interface to bind |
| `PORT` | `3000` | listen port |
| `MEDIA_PATH` | `./media` | directory of videos |
| `AUTH_COOKIE_NAME` / `AUTH_COOKIE_VALUE` | unset | cookie auth (both or neither) |
| `PUBLIC_BASE_URL` | `http://<addr>:<port>` | origin for share links |
| `SHARE_DB_PATH` | `./shares.db` | SQLite share database |
| `THUMB_DIR` | `./thumbs` | share thumbnail cache |
| `ANALYTICS_TAG` | empty | optional HTML snippet for `<head>` |

## Deployment

`deploy.sh` builds and syncs the stack to a remote host via rsync/ssh and re-creates the containers; see `./deploy.sh --help`.
