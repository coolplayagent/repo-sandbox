# `.repo-sandbox.yaml` v1

The repository configuration is explicit and versioned. A business repository
selects a centrally maintained environment by template ID and supplies only
declared parameters. It neither copies a Dockerfile nor infers a template from
filenames. See `.repo-sandbox.yaml.example` for a complete example and
[`templates.md`](templates.md) for the catalog contract.

## Private Git authentication

Git credentials are runtime inputs to the snapshot adapter and are deliberately
not fields in `.repo-sandbox.yaml`. Callers may select one of these modes:

- SSH agent, using the host's `SSH_AUTH_SOCK`;
- an SSH private-key file reference;
- an HTTPS short-lived token referenced by an environment variable or a
  host-only file; or
- the user's configured Git credential helper.

SSH always runs with `BatchMode=yes` and `StrictHostKeyChecking=yes`. An optional
`known_hosts` file can be referenced explicitly; otherwise OpenSSH's normal
known-hosts files are used. Host-key enrollment is an operator step and is never
performed automatically.

Short-lived HTTPS tokens are passed only to a private temporary askpass helper.
Configured credential helpers are disabled for that mode so the token cannot be
stored. Credential-helper mode uses the user's existing Git configuration and
disables terminal prompting. Inline credentials in HTTP(S) repository URLs are
rejected. Temporary helpers and SSH configuration are removed on success and on
failure; secret values are absent from command arguments, errors, snapshot
metadata, and source contents.

Remote failures are reported separately as authentication, network, repository
not found, or permission denied. Raw remote diagnostics are not copied into
errors because servers frequently echo URLs or credentials.

## Schema

Every field shown in the example is required. Unknown fields are errors.

- `version` must be the integer `1`.
- `template.id` is the exact, explicit central template ID.
- `template.parameters.platform` is `linux/amd64` or `linux/arm64`.
- Remaining entries in `template.parameters` override parameters declared by
  that central template. Unknown parameters fail during planning.
- `build`, `test`, inline images, and Dockerfile paths are not allowed in the
  selection form: the central template owns environment construction.

Deserialization errors and semantic validation errors include a stable YAML
field path such as `$.template.parameters.platform`. Catalog and graph errors
also identify their source, for example `$.templates[0].components[2].id`.

### Migration from the original v1 inline shape

The original `template.name/image/resources/environment/artifacts` plus
top-level `build` and `test` shape is still parsed and validated for backward
compatibility. `plan` returns an explicit migration error because inline build
logic cannot be translated safely. Replace it with `template.id` and
`template.parameters`; do not copy the old Dockerfile or command strings. This
catalog intentionally does not implement init-dev's implicit filename ordering
or naming protocol.

## CLI override contract

The `plan`, `build`, and `verify` routes accept only these runtime options:

| Option | Meaning |
| --- | --- |
| `--repository PATH_OR_URL` | repository source |
| `--git-ref REF` | Git ref to check out |
| `--platform PLATFORM` | target platform; repeat for multi-platform output; the first platform is verified locally |
| `--oci-layout DIRECTORY` | atomically export a verified OCI layout; required for multi-platform output without `--push` |
| `--push` | publish a successful/verified task image using the central Registry policy |
| `--report-path PATH` | atomic, no-overwrite JSON report destination |
| `--keep-on-failure` | retain a failed sandbox |
| `--recurse-submodules` | recursively materialize Git submodules in the source snapshot |

Remote credentials are always explicit external references. HTTPS accepts
`--git-https-token-env NAME` with an optional `--git-https-username USER`, or
the mutually exclusive `--git-credential-helper`. SSH accepts
`--git-ssh-private-key PATH` or `--git-ssh-agent`, plus an optional strict
`--git-ssh-known-hosts PATH`. Secret values and key bytes never enter the plan,
process arguments, report, or journal; only the selected reference names/paths
enter the plan digest.

When `--platform` is absent, the configured platform parameter is used. Multiple
platforms require `--push` or `--oci-layout`; publication/export happens only
after the primary platform passes the central runner. No CLI option can replace
the central image, component graph, or build contexts.

## Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | success |
| `2` | CLI or configuration error |
| `3` | host/sandbox environment error |
| `10` | build failed |
| `11` | test failed |

Build-step failures map to `10`; test-step failures map to `11`.
For `clean`, dry-run, policy-excluded, and already-absent resources are a
successful result. An active workflow lease, operator cancellation, or an owned
image that is still referenced is unfinished work and exits `3`; inspection or
removal failures also exit `3`.

The selected central template has a mandatory versioned `execution` profile.
It owns ordered build/test commands, fail-fast, CPU/memory/temporary-storage
limits, timeout, allowlisted environment names, external Secret names, artifact
directories, and optional parameterized Registry repository/aliases. Unknown
fields fail. The complete resolved DAG, profile, parameters and finite CLI
overrides form the execution/task identity digest.

The built-in `rust-bazel-acceptance-*` profiles are fixed diagnostic fixtures
for required CLI verification of timeout, memory, temporary storage,
architecture, Secret injection, and artifact export. They are never selected
by default and execution requires the explicit process-local opt-in
`REPO_SANDBOX_ENABLE_ACCEPTANCE_PROFILES=1`. Repositories can select only the
published profile ID and platform; they cannot replace its commands or safety
limits. The architecture fixture uses a centrally fixed mismatched runtime
platform so Docker itself rejects the image/platform contract as an environment
error; it is not an ordinary test-command assertion. These profiles run through
the same unprivileged Docker runner and must
not be used as application build profiles.
