---
name: wiretrace
description: Reverse-engineer an application into a callable API. Use when the user asks to "turn this into an API", "reverse engineer this app", "build an MCP for this site", "automate this portal", or needs to integrate with a service that has no public API or SDK — internal dashboards, ERP and back-office portals, Discord, SaaS tools.
---

# wiretrace

Watch an application make its own HTTP calls, infer the API behind them, then
call it directly. This replaces browser automation with the requests the browser
was going to make anyway: no selectors, no DOM, no re-driving the UI every run.

## Choosing an entry point

| Situation | Do this |
|---|---|
| User can export a HAR from DevTools | `import_har` — no proxy, no certificate, nothing installed |
| Website, user is logged in or will log in | `start_capture(url)` — browser mode |
| Native app, CLI, or script | `start_capture(host, mode="proxy")` |
| You want to drive it yourself unattended | `mode="proxy", system_proxy=False`, then route your own `curl` through it |

Prefer HAR when it is available. It is the fastest path and the only one that
touches nothing on the machine.

## The loop

1. **`start_capture`** (or `import_har`). It blocks until capture is live, so
   the first request cannot race it.
2. **Exercise the workflow.** wiretrace does not click — use the `computer-use`
   skill against the window, or ask the user to perform the action. Run the
   target action **twice with different values**: two samples is what turns
   `/channels/8412/messages` into `/channels/{channel_id}/messages`.
3. **`stop_capture`** → the inferred endpoints.
4. **`describe_endpoint`** → parameters, field names, dependencies.
5. **`call_endpoint`** → verify against the real service before promising
   anything. Every argument you omit falls back to what was captured.
6. **`verify_session`** → confirm the credentials still authenticate.
7. **`export_mcp(dest)`** → OpenAPI plus a standalone MCP server, one typed
   tool per endpoint, carrying its own credentials.

## Reading the inference

- `id: METHOD url (Nx)` — call count. High counts are the real endpoints.
- `<- click on button "Send"` — the UI action that caused it. Browser captures
  only. Usually the fastest way to find the endpoint you actually want.
- `needs first: channel_id <- get_guilds.guilds[].id` — **a dependency**. That
  value came out of another call's response, so it is not a constant you can
  hardcode. Call the producer first and thread the value through.
- Telemetry hosts and liveness probes are captured but excluded from exports.

Path parameters are only inferred for machine-looking segments — ids, UUIDs,
hashes. `/repos/{owner}/{repo}` stays literal and you get one endpoint per
value. That is deliberate: merging two calls that are not the same call breaks
replay, so the inference errs toward splitting.

## Credentials

Auth material — headers *and* query parameters like `?api_key=` — is moved to a
0600 store and reattached on replay. You never see the token and do not need it.
**Never ask the user for a password or API key to make an endpoint work.** A 401
means the capture contained no authenticated request; capture again while the
user is logged in.

## When nothing is captured

`stop_capture` reports the raw request count, which is what distinguishes the
causes:

- **0 requests** — traffic never reached the capture. Wrong mode, wrong host
  filter, or a tab the capture is not attached to. Check this first; it is by
  far the most common cause.
- **Requests but no API calls** — the app renders server-side, or the calls went
  to a host you did not include.
- Certificate pinning is real but rare. It is the last explanation, not the
  first.

## Being careful

- `call_endpoint` hits the real service as the real user. GETs and searches are
  safe to try. Confirm before anything that posts, deletes, pays, or messages
  another person.
- `mode="proxy"` with `system_proxy=True` changes the machine's network settings
  until `stop_capture`. Always stop the session, including on failure.
- Captures hold real cookies and page bodies. Call `forget_session` when done.
- Check the target's terms of service before automating it. Reverse-engineering
  an API you are authorized to use is ordinary integration work; doing it to a
  service you have no account on is not.
