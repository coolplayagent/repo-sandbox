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
| `--platform PLATFORM` | target platform; overrides `template.parameters.platform` |
| `--push` | request a future image push |
| `--report-path PATH` | future report destination |
| `--keep-on-failure` | retain a failed sandbox |
| `--recurse-submodules` | recursively materialize Git submodules in the source snapshot |

When `--platform` is absent, the configured platform parameter is used. No CLI
option can replace the central image, component graph, or build contexts.

## Exit codes

| Code | Meaning |
| ---: | --- |
| `0` | success |
| `2` | CLI or configuration error |
| `3` | host/sandbox environment error |
| `10` | build failed |
| `11` | test failed |

Build and test execution is outside v1's implementation scope; codes `10` and
`11` are reserved now so later execution work does not change the public API.
