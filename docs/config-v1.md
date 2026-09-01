# `.repo-sandbox.yaml` v1

The repository configuration is explicit and versioned. v1 does not inspect
language files or Dockerfiles to infer any of these values. See
`.repo-sandbox.yaml.example` for a complete example.

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
- `template.name` is a non-empty, user-facing template identifier.
- `template.platform` is `linux/amd64` or `linux/arm64`.
- `template.image` is a non-empty OCI image reference. Resolution and pulling
  belong to a later issue.
- `template.timeout_seconds`, `template.resources.cpu`, and
  `template.resources.memory_mb` are positive integers.
- `template.environment.allow` is the complete list of host environment
  variable names that a future sandbox may inherit. Names match
  `[A-Z_][A-Z0-9_]*` and may not be repeated.
- `template.environment.secrets` maps an environment variable name to an opaque
  secret identifier using `environment` and `secret`. The configuration never
  contains the secret value. Target environment variables may not be repeated.
- `template.artifacts.directories` contains one or more repository-relative
  directories. Absolute paths and parent traversal (`..`) are rejected.
- `build` and `test` each contain one or more ordered `{ name, run }` steps.
  Both strings must be non-empty. v1 treats `run` as an explicit command string;
  this contract does not execute it.

Deserialization errors and semantic validation errors include a stable YAML
field path such as `$.template.resources.cpu` or `$.build[0].run`.

## CLI override contract

The `plan`, `build`, and `verify` routes accept only these runtime options:

| Option | Meaning |
| --- | --- |
| `--repository PATH_OR_URL` | repository source |
| `--git-ref REF` | Git ref to check out |
| `--platform PLATFORM` | target platform; overrides `template.platform` |
| `--push` | request a future image push |
| `--report-path PATH` | future report destination |
| `--keep-on-failure` | retain a failed sandbox |
| `--recurse-submodules` | recursively materialize Git submodules in the source snapshot |

When `--platform` is absent, the configured platform is used. No CLI option can
replace the image, timeout, resources, environment policy, secrets, artifacts,
or build/test steps.

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
