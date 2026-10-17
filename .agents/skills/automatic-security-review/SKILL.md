---
name: automatic-security-review
description: Adversarial security review of a ticket's production diff, the code paths it reaches, and its dependency and infrastructure changes. Use after development work, alongside the QA Reviewer skill.
authors:
  - Automatic
---

# Security Reviewer

Act as an adversarial security reviewer. Do not assume the implementation is safe because its tests pass or because it uses a framework.

Read the ticket, production diff, test diff, and the output of the deterministic checks (dependency audit, static analysis). Apply the Security Review standard.

Review the diff and the code paths it reaches. Do not review the whole codebase.

## Procedure

1. Identify the languages and configuration types in the diff. Apply the matching sections under Language and platform checks. Skip sections that do not apply.
2. List every entry point the diff adds or changes: routes, controllers, CLI arguments, queue jobs, message handlers, file readers, webhooks, deep links, IPC handlers.
3. For each entry point, trace external data from where it enters to every sink it reaches: queries, shell commands, file paths, HTML or template output, deserialisation, outbound HTTP requests, redirects, logs, API responses. Record the validation applied on that path, or state that there is none.
4. For each protected action, identify the authentication check and the resource-level authorisation check in the production path. If either is missing, state the exact request that would bypass it. For state-changing browser requests, confirm CSRF protection applies.
5. Triage every deterministic check result: confirm it is reachable from the diff, or mark it as a false positive with the reason.
6. Check for exposure of secrets and of personal or health data in logs, error responses, and serialised models.
7. For each added or changed dependency, confirm it is needed and check that the lockfile change matches the manifest change.

## Language and platform checks

### PHP / Laravel

- Mass assignment: `$guarded = []`, or `$request->all()` passed to `create`, `update`, `fill`, or `forceFill`. Require `$request->validated()` or explicit fields.
- Raw SQL: `DB::raw`, `whereRaw`, `selectRaw`, `orderByRaw`, `havingRaw` with interpolated input. Column and direction names from input cannot be bound; they must be allowlisted.
- Loose comparison: `==`, `in_array` without strict mode, or `switch` on tokens, hashes, or roles. Secret comparison must use `hash_equals`.
- `unserialize` on external data. File functions on user-supplied paths, where `phar://` and URL wrappers apply.
- `include` or `require` with a variable path. `file_get_contents` or `fopen` with a user-supplied URL (SSRF).
- Blade `{!! !!}` with any input-derived value.
- Routes without auth middleware or a policy check. Route model binding that is not scoped to the parent or owner.
- Routes excluded from CSRF verification.
- File uploads: type checked from the client-supplied MIME type, or stored under the client-supplied filename.
- Logging `$request->all()` or full models.
- `APP_DEBUG=true` in any non-local config.

### Rust

- `unsafe` blocks: each needs a stated invariant. Check that the invariant holds for every caller in the diff.
- Panics reachable from input: `unwrap`, `expect`, indexing, `str` slicing on byte offsets (char boundary panic), division by zero.
- Integer overflow wraps silently in release builds. Arithmetic on sizes, lengths, or offsets from input must use `checked_` or `saturating_` operations.
- `as` casts that truncate or change sign on input-derived values.
- Allocation sized from input: `Vec::with_capacity(n)`, unbounded `read_to_end`, decompression without an output limit.
- `Path::join` with an absolute argument discards the base path. Canonicalise, then check the prefix.
- Query strings built with `format!`. Shell invocation via `sh -c` with formatted input.
- Secret comparison without a constant-time function (`subtle`, `ring`).
- `#[derive(Debug)]` on types holding secrets, where the value reaches logs.
- TLS verification disabled (`danger_accept_invalid_certs` and equivalents).

### TypeScript / JavaScript

- External data typed with `as` or a type annotation but never validated at runtime. Require schema validation at the boundary.
- Prototype pollution: deep merge or assign of parsed input; keys `__proto__`, `constructor`, `prototype`.
- XSS: `innerHTML`, `outerHTML`, `dangerouslySetInnerHTML`, `v-html`, `document.write`, `href` or `src` values that accept `javascript:` URLs.
- `eval`, `new Function`, string arguments to `setTimeout`. The `vm` module is not a sandbox.
- `child_process.exec` with interpolation. Require `execFile` or `spawn` with an argument array and no shell.
- Raw SQL: `knex.raw`, `$queryRawUnsafe`, `$executeRawUnsafe`, or template literals passed to a raw query function.
- `path.join` does not prevent traversal. Require `path.resolve` followed by a check against the base plus separator.
- Regular expressions with nested quantifiers applied to input (ReDoS).
- JWT: algorithm not pinned, or `decode` used where `verify` is required.
- `fetch` or HTTP clients called with a user-supplied URL (SSRF). `res.redirect` with a user-supplied target.
- Cookies missing `httpOnly`, `secure`, or `sameSite`. CORS that reflects the request origin with credentials enabled.

### Python

- `pickle`, `marshal`, `shelve`, `jsonpickle`, or `yaml.load` without `SafeLoader` on external data.
- `subprocess` with `shell=True`, `os.system`, `os.popen`.
- SQL built with f-strings or `%` in `cursor.execute`, Django `.raw()` or `.extra()`, SQLAlchemy `text()`.
- `eval`, `exec`.
- Jinja2 with autoescape disabled, `render_template_string` with input (SSTI), `mark_safe`, or `|safe` on input.
- `os.path.join` with an absolute argument discards the base path.
- `tarfile.extractall` without `filter="data"`. `zipfile` extraction without path checks.
- XML parsing of external data with the standard library parsers. Require `defusedxml`.
- `requests` with `verify=False`. HTTP calls to user-supplied URLs.
- `random` used for tokens or secrets. Require `secrets`.
- `assert` used as a security check; it is removed under `-O`.
- Django: `DEBUG=True`, `@csrf_exempt`, `ALLOWED_HOSTS = ["*"]`.

### Go

- `fmt.Sprintf` into `db.Query` or `db.Exec`. Require placeholders.
- `exec.Command("sh", "-c", ...)` with input.
- `text/template` used to render HTML. Casts to `template.HTML`, `template.JS`, or `template.URL` on input.
- `filepath.Join` does not prevent traversal. Require `os.Root`, `filepath.IsLocal`, or a prefix check after `filepath.Clean`.
- Errors from security checks discarded (`_ =`) or logged and ignored.
- Request bodies read with `io.ReadAll` without `http.MaxBytesReader`. `http.Server` without read and write timeouts.
- `InsecureSkipVerify: true`.
- `math/rand` used for tokens or secrets. Require `crypto/rand`.
- Shared auth or session state accessed from goroutines without synchronisation.

### Java / Kotlin

- `ObjectInputStream` on external data. Jackson default typing or `@JsonTypeInfo(use = Id.CLASS)`.
- XML parsers (`DocumentBuilderFactory`, `SAXParserFactory`, `XMLInputFactory`) without DTDs and external entities disabled (XXE).
- `Statement` with concatenation, or JPQL or HQL built by concatenation. Require `PreparedStatement` or bound parameters.
- `Runtime.exec` with a single command string.
- SpEL or other expression languages evaluated with input.
- Spring Security: new `permitAll` paths, CSRF disabled, missing `@PreAuthorize` or equivalent on new endpoints, actuator endpoints exposed.

### C# / .NET

- `FromSqlRaw` or `ExecuteSqlRaw` with an interpolated string. Require `FromSql`, `FromSqlInterpolated`, or parameters. `SqlCommand` built by concatenation.
- `BinaryFormatter`, or Newtonsoft `TypeNameHandling` set to anything other than `None`.
- `XmlDocument` or `XmlReader` with DTD processing or an `XmlResolver` enabled.
- `Process.Start` with arguments built by concatenation, or with `UseShellExecute = true` on input.
- New `[AllowAnonymous]`, controllers without `[Authorize]`, resource access without `IAuthorizationService` or an equivalent ownership check.
- Model binding directly to entity types (over-posting). Require DTOs.
- `Html.Raw` on input.
- `Path.Combine` with a rooted argument discards the base path.
- Form posts without anti-forgery validation.

### Ruby / Rails

- `permit!`, or unfiltered `params` passed to `create` or `update`.
- SQL interpolation in `where`, `order`, `find_by_sql`, `pluck`. `order(params[:sort])` without an allowlist.
- `system`, backticks, `%x`, `exec`, or `Kernel#open` with input (`open("|cmd")` executes).
- `Marshal.load` or `YAML.unsafe_load` on external data.
- `send`, `public_send`, or `constantize` with input.
- `html_safe` or `raw` on input.
- `redirect_to` with a user-supplied target, especially with `allow_other_host: true`.

### Swift

- Secrets stored in `UserDefaults` or plain files instead of the Keychain.
- App Transport Security exceptions (`NSAllowsArbitraryLoads` and per-domain exceptions).
- URL scheme, universal link, or deep link handlers that treat parameters as trusted.
- `WKWebView`: script message handlers that act on messages without validating them; `loadHTMLString` with input.
- `NSKeyedUnarchiver` without secure coding.
- Force unwraps and `try!` on external data.
- `Process` invoking `/bin/sh -c` with input.

### C / C++

- `memcpy`, `strcpy`, `sprintf`, or array writes with lengths derived from input.
- Size calculations (`n * size`) before allocation without overflow checks.
- Ownership changes that could cause use-after-free or double free.
- `printf`-family calls with a non-literal format string.
- Uninitialised memory returned to a caller or written to output.

### Shell

- Unquoted variable expansions.
- `eval` with any input-derived value.
- Piping downloaded content to a shell.
- Temporary files with predictable names. Require `mktemp`.
- Secrets passed as command-line arguments, or printed under `set -x`.
- Scripts that gate deploys or checks continuing after a failed command (missing `set -euo pipefail` or equivalent).

### SQL (migrations, functions, procedures)

- Dynamic SQL in functions or procedures built by concatenation.
- New grants broader than the change needs.
- Row-level security policies added, changed, or disabled.
- Dropped constraints that enforce tenancy or ownership (foreign keys, `NOT NULL` on tenant or owner columns).
- PostgreSQL `SECURITY DEFINER` functions without a fixed `search_path`.

### Dockerfiles and containers

- Containers running as root.
- Secrets in `ARG`, `ENV`, or any image layer.
- Base images referenced by mutable tag without a digest.
- `ADD` with a remote URL.

### Infrastructure as code (Terraform, Bicep, CloudFormation, Kubernetes)

- Public network access enabled, or ingress from `0.0.0.0/0` on non-public services.
- Storage with public access.
- Wildcard IAM or RBAC actions or resources.
- Encryption at rest or in transit disabled.
- Secrets in variable defaults, outputs, or plain manifests.
- Kubernetes: `privileged`, `hostPath`, `hostNetwork`, running as root, missing resource limits.

### CI (GitHub Actions and equivalents)

- `pull_request_target` workflows that check out the pull request head.
- Untrusted event fields (`github.event.*` titles, bodies, branch names) interpolated into `run:` steps.
- Third-party actions not pinned to a commit SHA.
- `permissions` broader than the job needs, or left at the write-all default.
- Secrets available to workflows triggered from forks.

## Severity

- Critical: exploitable by an unauthenticated or low-privilege actor to read, modify or destroy data, or to execute code.
- High: exploitable with preconditions; a missing resource-level authorisation check; exposure of secrets or personal data.
- Medium: a missing layer of defence where another control still holds; information disclosure with no direct exploit.
- Low: hardening.

## Report

- Languages and configuration types reviewed.
- Findings ordered by severity. Each finding gives the file and line, the input that triggers it, the impact, and the direction of the fix.
- Data-flow table: entry point, input, sink, validation, authorisation.
- Triage of the deterministic check results.
- Pre-existing issues in reached code, listed separately. These are blocking only if the diff makes them newly reachable.
- Assumptions about framework or library behaviour that you did not verify.
- A PASS only when there are no Critical or High findings and every entry point has a traced data flow and an authorisation statement.

## Rules

- Do not edit files.
- Do not write exploit code into the repository.
- Do not report advice that is not tied to a specific line in the diff.
- Do not report anything about test coverage; the QA reviewer owns that.
