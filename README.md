# canis

A single-binary, simple media server.
Use the browser for actually displaying the videos so we only have to worry about serving the data.

The idea is to run this bound to some closed off interface (e.g. Tailscale) so one doesn't have to worry about authentication. There is a very simple Cookie authorization check implemented for good measure.

## Auth
Set `AUTH_COOKIE_NAME` and `AUTH_COOKIE_VALUE`. Both need setting or the auth check doesn't run.
If both are set the app is going to return a 401 unless queried with the cookie.

## Example

```bash
BIND_ADDRESS=127.0.0.1 PORT=3344 \
AUTH_COOKIE_NAME=X-Hot-Dog AUTH_COOKIE_VALUE=yummy canis
```
To run locally with the cookie auth and an analytics snippet:

```bash
BIND_ADDRESS=127.0.0.1 PORT=3344 \
AUTH_COOKIE_NAME=X-Hot-Dog AUTH_COOKIE_VALUE=yummy \
ANALYTICS_TAG='<script async src="https://analytics.example.com/script.js" data-website-id="xxxx"></script>' canis
```

to run locally on port 3344 with some basic Cookie authentication.
