# OAuth 2.1 for MCP clients

How Claude Code, Cursor and other MCP clients connect to the hosted server without pasting a key
into their config. Implementation: `apps/api/src/oauth/` (`app.ts` endpoints, `cimd.ts` client
metadata, `store.ts` hashed codes and tokens); tests: `apps/api/test/oauth.test.ts`. Plan: E3-T2.
Served when the deployment sets `AGENT_OAUTH_SECRET`.

The gateway is both the protected resource (`/mcp` and the REST routes) and its authorization
server. It follows the MCP 2026-07-28 authorization profile: **Client ID Metadata Documents
replace Dynamic Client Registration**, so there is no registration endpoint.

## Discovery

1. A request to `/mcp` without a valid token answers `401` with
   `WWW-Authenticate: Bearer realm="shogunagent", resource_metadata="<origin>/.well-known/oauth-protected-resource/mcp"`.
2. `GET /.well-known/oauth-protected-resource/mcp` (RFC 9728; also without the `/mcp` suffix):
   `resource` (the MCP URL), `authorization_servers` (this origin), `scopes_supported`
   (`read`, `write`, `run`), `bearer_methods_supported: ["header"]`.
3. `GET /.well-known/oauth-authorization-server` (RFC 8414): `authorization_endpoint`
   `/oauth/authorize`, `token_endpoint` `/oauth/token`, `revocation_endpoint` `/oauth/revoke`,
   `code_challenge_methods_supported: ["S256"]`, `token_endpoint_auth_methods_supported: ["none"]`,
   `client_id_metadata_document_supported: true`, `authorization_response_iss_parameter_supported:
   true`. No `registration_endpoint`.

## Clients are their metadata document

The `client_id` is an HTTPS URL; the JSON there registers the client:

```json
{ "client_id": "https://client.example.com/oauth/client.json",
  "client_name": "Example MCP Client",
  "logo_uri": "https://client.example.com/logo.png",
  "redirect_uris": ["http://127.0.0.1/callback", "https://client.example.com/cb"],
  "token_endpoint_auth_method": "none" }
```

The fetch is guarded against server-side request forgery: `https` only, a path required, no
userinfo, fragment or dot segments; hosts that are `localhost`, single labels, `.local` /
`.internal` / `.home.arpa`, or IP literals in private, loopback, link-local, CGNAT, multicast,
documentation or reserved ranges (IPv4 and IPv6, including mapped / NAT64 / 6to4 forms) are
refused; on Node every address the name resolves to is checked too. Redirects are not followed,
the body is capped at 5 KiB, the request times out after 5 s, and the content type must be JSON.
The document must name itself, list 1–20 redirect URIs (`https:`, or `http:` on a loopback host),
and claim no client secret. Valid documents are cached for their `max-age` (60 s – 24 h).

`redirect_uri` must equal a listed URI exactly; the one exception (RFC 8252 §7.3) is the port of an
`http:` loopback URI.

## The flow

1. **Authorize.** `GET /oauth/authorize?response_type=code&client_id=…&redirect_uri=…&code_challenge=…&code_challenge_method=S256&state=…&scope=read%20write&resource=<mcp url>`.
   A bad client or redirect URI gets an HTML error page (never a redirect). Other errors return to
   the redirect URI (`invalid_request`, `unsupported_response_type`, `invalid_scope`,
   `invalid_target`). Otherwise the consent page names the client, where it returns to and what
   each scope allows.
2. **Consent.** The person pastes the principal's agent key (`sa_live_…`): that is what says which
   account to connect. The grant is the requested scopes intersected with the key's; `admin` is
   never grantable; an OAuth access token cannot consent to another grant. Approve redirects with
   `code`, `state` and `iss`; deny redirects with `error=access_denied`.
3. **Token.** `POST /oauth/token` (form-encoded) with `grant_type=authorization_code`, `code`,
   `redirect_uri`, `client_id`, `code_verifier` (43–128 characters, S256 only). Answers
   `{ access_token: "sat_…", token_type: "Bearer", expires_in: 900, refresh_token: "sar_…", scope }`.
   Codes live 60 seconds and work once; a second exchange of the same code revokes everything the
   first one issued.
4. **Refresh.** `grant_type=refresh_token` with `refresh_token` and `client_id` (and optionally a
   narrower `scope`). Every refresh rotates the refresh token; presenting a rotated one again
   revokes the whole grant (reuse detection). Refresh tokens live 30 days.
5. **Revoke.** `POST /oauth/revoke` with `token` (RFC 7009): revokes the grant; always `200`.

## What an access token can do

Everywhere an `sa_` key works, with the consenting key's principal, wallet and caps, and at most
its scopes. It stops working when it expires, when its grant is revoked, and when the key behind it
is revoked or expires. It cannot revoke or re-cap keys, link a licence, or call admin routes.

Codes, access tokens and refresh tokens are stored only as SHA-256 hashes; none of them, nor the
client id or redirect URI, appears in a log line.
