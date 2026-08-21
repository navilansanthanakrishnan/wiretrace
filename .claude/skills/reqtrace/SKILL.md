---
name: reqtrace
description: Learn an app's private API by watching it, then call it. Use when you need to automate a website or desktop app that has no public API or SDK — Discord, an internal dashboard, a SaaS tool — or when the user asks to "figure out the API", "build an MCP for this app", or "automate this site".
---

# reqtrace

You watch an app being used, reqtrace tells you what API it is really calling,
and then you call that API directly. No DOM scraping, no clicking through a UI
on every run.

## When to use this

- The app has no documented API, or the docs do not cover what you need.
- You want to automate something repeatable, not click through it once.
- The user wants an MCP server for an app they already log into.

Do not use it when a public, documented API already exists. Use that instead.

## The loop

1. **`start_capture(target, mode)`** — it does not return until the capture is
   really listening, so you can drive the app immediately.
2. **Drive the app so it makes the calls you care about.** This is the step that
   matters — reqtrace can only infer what actually happened. Do the target
   action *twice with different values* if you can: two samples is what turns
   `/channels/8412/messages` into `/channels/{channel_id}/messages`.
3. **`stop_capture()`** — returns the inferred endpoints.
4. **`describe_endpoint(id)`** — parameters and field names. Add `full=True`
   only when you need the raw schemas; they are long.
5. **`call_endpoint(id, path_params=…, query=…, body=…)`** — call it for real.
   Verify here before promising anything.
6. **`export_mcp(dest)`** — writes an OpenAPI document and a standalone
   `server.py` MCP server the user can register and keep. It carries its own
   credentials, so it keeps working after the capture session is gone. Pass
   `only=[ids]` to export a chosen subset — note that `only` is a full
   override, so anything you name is exported even if it is telemetry.

## Choosing a mode

**`mode="browser"`** (default) for websites. A Chrome opens at `target` and
requests are attributed to the click that caused them.

- `navigate(url)` points that tab somewhere else.
- `evaluate(js)` runs JavaScript in it — this is how you click and type without
  a human: `document.querySelector('button[type=submit]').click()`.
- Only that one tab is captured. Anything you open another way is invisible.
- `headless=True` when nobody is watching. Everything above still works;
  computer-use does not, because there is no window.

**`mode="proxy"`** for native apps, CLIs and scripts. `target` is the host to
watch and is always intercepted; `hosts` adds more.

- By default this repoints the macOS system proxy until `stop_capture`.
- **`system_proxy=False` is the unattended path**: nothing on the machine
  changes, and you route traffic through the proxy yourself. You can drive that
  traffic — a `curl` or a script *is* the app being captured. `start_capture`
  returns the exact command to use, with the right port and certificate path
  already filled in; read it rather than guessing, because the port is not
  always 8787. `ca_path` gives you the certificate on its own.
- For a real desktop app you cannot drive it yourself; the system proxy path
  needs `reqtrace trust` to have been run once, and the user has to use the app.

## Reading the result

Endpoints are listed as `id: METHOD url (Nx)`, followed in browser captures by
`<- click on button "Send"` — the UI action that triggered it. That trigger is
usually the fastest way to find the endpoint you actually want. Proxy captures
never have one, so there go by call count and path instead.

Prefer endpoints with a high call count. Telemetry hosts and liveness probes
are captured but left out of exports, so the exported tool list is already the
useful subset.

Path parameters are only inferred for machine-looking segments — ids, UUIDs,
hashes. A path like `/repos/{owner}/{repo}` stays literal, so you may see one
endpoint per value. That is expected, not a failure.

## Credentials

Captured auth — headers *and* query parameters like `?api_key=` — is stored
privately and reattached on replay. You will not see the token, and you do not
need it. Never ask the user for a password or API key to make an endpoint work.
If a call 401s, the capture did not include an authenticated request: capture
again after the user is logged in.

## When nothing is captured

`stop_capture` tells you how many raw requests it saw, which is the thing that
distinguishes the causes:

- **0 requests** — the app never reached the capture. Wrong mode, wrong host
  filter, or you were driving a tab the capture was not attached to. This is by
  far the most common cause; check it before blaming the app.
- **Requests, but no API calls** — the app renders server-side, or the calls
  went to a host you did not include.
- Certificate pinning is a real possibility for native apps, but it is the last
  explanation to reach for, not the first.

## Being careful

- `call_endpoint` hits the real service as the real user. Read-only endpoints
  (GET, search, list) are safe to try. Confirm with the user before calling
  anything that posts, deletes, pays, or sends a message to another person.
- A `mode="proxy"` capture with `system_proxy=True` changes the machine's
  network settings until `stop_capture`. Always stop the session, including on
  failure.
- Captured sessions contain the user's real traffic, including logged-in page
  bodies and full cookies. Do not copy them out of `~/.reqtrace`, and call
  `forget_session` when a capture is no longer needed.
