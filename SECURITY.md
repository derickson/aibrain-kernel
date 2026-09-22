# Security

A record of the security review of `aibrain-kernel`: what the app is meant to
withstand, what was found, what was fixed, and what was deliberately left
alone. Written after the review of 2026-09-22, against commit `a99a700`.

## Do this now

**The Elasticsearch API key in `.env` was committed and must be rotated.**

`.env` carried a live `ELASTICSEARCH_API_KEY` and was tracked from commit
`065f567` onwards. It has been removed from the index and added to
`.gitignore`, and `.env.example` now shows the shape without the values — but
**the key is still in git history**, and history has deliberately not been
rewritten (other branches and worktrees are in flight; a rewrite would strip
them). Anyone with the repository has the key.

1. Revoke the key in Elasticsearch and issue a new one.
2. Put the new key in your local `.env` only. It is ignored now.
3. If this repository is ever published, rewrite history or treat every key
   that ever appeared in it as burned.

## Threat model

A single-user desktop app. Two servers on loopback with no authentication:

| Process | Port | Reachable by |
|---|---|---|
| `aibrain/server.py` | 127.0.0.1:8760 | the browser, the user's shell |
| `rust/aibrain-core` | 127.0.0.1:8781 | the Python server, the user's shell |

The corpus is the user's Obsidian vaults: personal notes, read-only to this
app. The data of value is the notes themselves; the capability of value is
that the app can start subprocesses (maintenance scripts, ACP agents) as the
user.

**In scope.** Anything reachable from a web page the user did not write, and
anything the *contents of a note* can do — a note is untrusted input, because
it can be anything you pasted. Concretely:

- a page on another origin reading or driving the API (CSRF, DNS rebinding),
- a note whose text becomes markup in the reader (XSS),
- a request making the servers read or write outside the vaults,
- a request choosing what subprocess runs,
- credentials reaching a log, a response or the repository.

**Out of scope.** An attacker who is already running code as the user, or who
can edit `~/.aibrain/config.json`. The config names the command an ACP agent
runs; whoever writes it has already won, and no check here would change that.
Multi-user or network deployment is not a supported configuration: the checks
below assume loopback and would need real authentication before this could be
bound anywhere else.

## Found and fixed

Severity is relative to the threat model above: "high" means a web page could
do it without the user noticing.

### High — no origin check on a state-changing API (`aibrain/server.py`)

A page on any origin could `POST /api/agent/<id>` and set the command an ACP
agent runs, or `POST /api/script/<id>/run` to start one, and read every note
through `/api/search`. A JSON `fetch` would be stopped by the preflight, but a
cross-site form post is a "simple" request and was not.

Fixed in `aibrain/server.py` `Handler.origin_is_same`, applied to every request
in `_dispatch`: `Sec-Fetch-Site` must be `same-origin` or `none`, and if
`Origin` is present it must equal the `Host`. A request with neither header —
curl, urllib, the MCP server — is allowed, because no browser omits them.

### High — DNS rebinding on both servers

`evil.com` resolving to `127.0.0.1` makes the browser treat the attacker's
page as same-origin with ours, which defeats the check above and exposes the
whole corpus. The `Host` header still says `evil.com`, and no page can change
it.

Fixed in `aibrain/server.py` `Handler.host_is_local` and in
`rust/aibrain-core/src/api.rs` `guard_host` / `host_is_loopback`: the host must
be `127.0.0.1`, `localhost`, `::1` or `0.0.0.0`. Both implementations refuse
`[::1].evil.com` and `127.0.0.1.evil.com`, which a naive split on `:` accepts.

### High — `javascript:` links in a note (`rust/.../vault/render.rs`)

`pulldown-cmark` escapes a link destination but does not judge it, so
`[click](javascript:alert(1))` in a note became a live `href`, and
`web/app.js` puts the rendered HTML into the app's own DOM with `innerHTML`.
Raw HTML in a note was already neutralised; the destination was not.

Fixed with `safe_url` / `guard_destination`: a destination with a scheme
outside `http https mailto ftp tel obsidian` becomes `#`. Whitespace and
control characters are folded out first, so `java\tscript:` is caught.
Relative destinations and fragments are untouched.

### Medium — arbitrary arguments appended to a script's command line

`POST /api/script/<id>/run` appended a free-form `args` list from the request
body to the command. No shell was involved, so this was not command injection,
but "which flags does the exporter run with" is not the caller's decision, and
the UI never sent one.

Fixed in `run_script`: only names looked up in the script's own `options`
table reach the command line. The script itself was already an allow-list —
the request names an id, and the command comes from `config.json`.

### Medium — `add_brain` could place a symlink anywhere

`POST /api/brains/add` built the link as `VAULT_LINK_DIR / payload["name"]`
with the name unchecked, so `{"name": "../../.ssh/authorized_keys"}` wrote a
symlink outside `obsidian_vaults/`.

Fixed with `link_name`, which refuses anything containing a separator, a NUL,
a newline, a leading dot, `.`/`..`, or more than 128 characters — and refuses
rather than sanitising, because quietly renaming `../evil` to `evil` links a
vault under a name nobody asked for. The resolved parent is re-checked after
the join.

### Medium — exception text returned to the caller

`_dispatch` returned `f"{type(exc).__name__}: {exc}"` with a 500, and the Rust
`ApiError` returned the whole `anyhow` chain. Both carry absolute vault paths,
failing SQL, and whatever Elasticsearch said about a request built from user
input.

Fixed in both: the detail goes to the log the user started the server in, and
the response says only that it failed. `CorpusError` and the new `BadRequest`
still carry their text, because both are written for a person to read.

### Medium — `.claude/settings.local.json` rewritten in a way that removed rules

`write_deny_rules` rebuilt the whole `Read(` block from the current symlinks,
so any `Read(...)` denial someone had written by hand disappeared on the next
`Config.load()` — and nothing in the file says which rules are ours.

Fixed: the function now only ever **adds**. A rule that outlives its vault
costs nothing; a rule that vanishes costs the protection it was there for. It
also honours `AIBRAIN_MANAGE_DENY_RULES=0`, so a test or a script can load a
config without editing the file. The existing guard — a missing
`obsidian_vaults/` means a fresh clone or a worktree, and nothing is written
— is unchanged.

Separately, the rules were originally written into `.claude/settings.json`,
which is committed — baking each checkout's absolute vault paths into shared
git history and making every machine's commits fight over the same file.
Moved to `.claude/settings.local.json`, Claude Code's untracked
local-overrides file (now gitignored), so each checkout's paths stay local to
it.

The trade-off: unlinking a vault now leaves its deny rule behind. That is the
fail-safe direction, and removing it is a one-line edit the user can make.

### Medium — unbounded and unvalidated query parameters

`limit=abc` was a traceback, `limit=-1` travelled to Postgres, and
`/api/recent?limit=1000000` was honoured. `Content-Length` was read and
allocated without a cap.

Fixed: `Handler.int_query` parses and clamps (400 on a non-number),
`_int_id` does the same for a path segment the router only promised was
`[^/]+`, bodies over 1 MiB are refused on the header before a byte is read,
a non-object JSON body becomes `{}`, and the search query and brain list are
length-capped in both Python and Rust. The Rust router gained a 256 KiB body
limit in place of axum's 2 MiB default.

### Medium — an A2A agent card could redirect our credentials

`A2AClient.endpoint` took the POST URL out of the agent card, which is written
by the remote agent, and `A2AClient.headers` carries whatever static API key
the user configured for that connection. A card naming another host sent the
key there.

Fixed with `same_origin`: a card may move the path, not the scheme, host or
port. Anything else raises rather than silently falling back, so the user
learns the remote agent tried.

Still true and not fixed: `urllib` follows redirects and keeps custom headers
across hosts, so a 302 from the configured host could do the same thing. Left
because closing it means replacing `urlopen` with a hand-rolled redirect
policy, and the configured host is one the user chose and trusts enough to
hand a key to. Worth revisiting if A2A connections ever become shareable.

### Low — agent colour interpolated into a `style` attribute

`POST /api/agent/<id>` accepted any string as `color`, and `web/universe.js`
interpolated it into a `style="…"` inside an `innerHTML` template, so
`#000" onmouseover="…` closed the attribute. Reachable only through the API,
so the origin check above already closes it; fixed anyway in both places —
the server requires `#` plus 3-8 hex digits, and the template escapes.

### Low — `fs/read_text_file` was broken, not just wrong

`ACPConnection._read_file` called `self.opened.add(...)` on what became a dict
when the second evidence tier was added, so every file an ACP agent asked us
to read came back as an error. Fixed to record through `_record`, which is
also what applies `_confine`. `line`/`limit` are now parsed defensively.

## Verified and left as they are

- **SQL is parameterised everywhere.** The only `format!` near a `sqlx::query`
  interpolates the `COLUMNS` constant (`db/todo.rs`); every value is bound.
  `to_tsquery` input is rebuilt from scratch out of alphanumerics with a
  12-term cap (`db/queries.rs::to_tsquery`), so it cannot be a syntax error
  let alone an injection.
- **Elasticsearch queries are built with `serde_json`,** never concatenated.
  Index names come from `sanitize_index_name`, and `target_indices` maps
  caller-supplied brain *ids* through the brain table to names — an unknown id
  narrows to an index that cannot exist rather than widening to `*`. A caller
  cannot name an index.
- **The markdown renderer escapes raw HTML** (`Event::Html` → `Event::Text`)
  and escapes wikilink labels. With the destination fix above, the reader's
  one `innerHTML` of server HTML is safe.
- **The browser escapes everything else.** `web/app.js` builds DOM through
  `el()` with text nodes; the one exception is `safeSnippet`, which escapes
  the whole string and then re-admits `<mark>` only — the markers both
  Postgres and Elasticsearch emit. Agent answers, note titles, wikilink
  targets and citation text all go through text nodes.
- **Static file serving is confined.** `_static` resolves the path — which
  collapses `..` and follows any symlink — and requires `web/` to be a parent.
  It now percent-decodes first, so the check sees what the filesystem will.
- **Secrets stay out of responses and logs.** `AgentConfig.headers` (A2A
  static auth) and `AgentConfig.env` are never included in `/api/status`, the
  universe payload, or any other response. Database URLs go through
  `db::redact` before they reach a log line. The Elasticsearch key is only
  ever a request header.
- **The ingester and the watcher never write into a vault.** Every filesystem
  write under `vault/` is inside a `#[cfg(test)]` module.
- **The ACP agent is confined** to the session directory and the linked vaults
  by `_confine`, which resolves before comparing, and the vault roots are
  generated from the enabled brains rather than hardcoded.
- **The MCP server validates its inputs:** unknown tool names are refused, a
  non-object `arguments` is refused, bad arguments come back as a tool result
  rather than a protocol error, and `limit` is clamped.
- **The search queue cannot spin.** `fail_queue` backs a row off by
  `2^attempts` seconds capped at an hour, and `queue_stats.failing` surfaces
  stuck rows through `/status` into the UI. Rows retry forever rather than
  being dead-lettered; at one attempt an hour that is cheap, and a row that
  gives up silently is worse than one you can see.
- **`add_brain` links any directory the user names.** This is the feature: the
  person typing the path is the person the app belongs to. What is enforced is
  where the *link* goes, and that `remove_brain` will only unlink something
  inside `obsidian_vaults/` that really is a symlink.
- **The config can name any command for an ACP agent.** Also the feature —
  that is how you point it at `claude-agent-acp`. The protection is that only
  a same-origin request can change it, which is now enforced.
- **SSE connections are one thread each** in the Python server
  (`ThreadingHTTPServer`, `daemon_threads`). A page opening hundreds would
  exhaust threads, but only a same-origin page can open any, and that page is
  ours.
- **Dead code from the SQLite era is gone from the corpus layer,** but two
  parser helpers were still re-exported with no consumer
  (`vault::split_frontmatter`, `vault::strip_markup`); the re-export was
  dropped. They are still used inside `parse.rs`.
- **`dotenvy` searches upward from the working directory** for a `.env`
  (`main.rs`). Running `aibrain-core` from inside somebody else's tree could
  therefore pick up their `.env`. Left as is: it is how the tool is meant to
  find the repo's own file, and real environment variables win over it.

## Dependencies

`cargo audit` (0.21, advisory database of 2026-09-22) over `rust/Cargo.lock`,
297 crates, reports exactly one advisory:

> `rsa` 0.9.10 — RUSTSEC-2023-0071, Marvin Attack: potential key recovery
> through timing sidechannels. Severity 5.9. No fixed upgrade available.

**Not exploitable here, and not actually built.** `rsa` reaches the lockfile
through `sqlx-mysql`, which sqlx declares as an optional dependency;
`Cargo.lock` is resolved without feature resolution, so it lists optional
crates that are never compiled. `cargo tree -e all` confirms neither
`sqlx-mysql` nor `rsa` is in the build graph — this service speaks only to
Postgres, over a loopback socket, with `tls-none`. The advisory is about RSA
key operations during MySQL authentication, which never happen.

Related cleanup while looking: `sqlx`'s `macros` feature was enabled and
nothing uses `sqlx::query!` or `sqlx::migrate!` (migrations are plain `.sql`
applied by `db::migrate`), so it was dropped.

Versions of note: axum 0.7, sqlx 0.8, reqwest 0.12 (rustls, no OpenSSL),
pulldown-cmark 0.12, tokio 1.

Python has no third-party dependencies at all — the package is standard
library only, by design, which removes the whole supply-chain surface on that
side. `web/vendor/three.module.js` is vendored and pinned; the page loads
nothing from a CDN.

## Tests

Every fix above has a test. New ones:

- `tests/test_kernel.py::ServerTests` — rebound `Host` refused, cross-site
  request refused, same-origin and non-browser allowed, four spellings of path
  traversal, oversized body, non-numeric query parameters, script arguments
  ignored, agent colour validated, internal failure carries no detail.
- `tests/test_kernel.py::LinkNameTests` — the `add_brain` name rules.
- `tests/test_kernel.py::A2AEndpointTests` — an agent card may move the path
  but not the host.
- `tests/test_kernel.py::DenyRuleTests` — no rule is ever dropped; the rewrite
  can be switched off.
- `rust/aibrain-core/src/api.rs` — `host_is_loopback` accepts what it should
  and refuses every rebinding spelling; an `ApiError` carries no detail.
- `rust/aibrain-core/src/vault/render.rs` — script URLs in markdown links and
  images are defused; ordinary links are untouched.
