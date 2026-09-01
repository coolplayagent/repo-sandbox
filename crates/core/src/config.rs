use serde::Deserialize;
use serde::de::IntoDeserializer;
use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

/// The versioned, validated `.repo-sandbox.yaml` domain model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub version: ConfigVersion,
    pub template: TemplateSelection,
    /// The pre-template-catalog v1 shape is accepted so existing repositories
    /// get a plan-time migration error instead of a YAML deserialization error.
    pub legacy: Option<LegacyTemplate>,
    pub build: Vec<Step>,
    pub test: Vec<Step>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigVersion {
    V1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyTemplate {
    pub name: String,
    pub platform: Platform,
    pub image: String,
    pub timeout_seconds: u32,
    pub resources: Resources,
    pub environment: Environment,
    pub artifacts: Artifacts,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateSelection {
    pub id: String,
    pub parameters: BTreeMap<String, String>,
}

impl TemplateSelection {
    pub fn platform(&self) -> Result<Platform, ConfigError> {
        self.parameters
            .get("platform")
            .ok_or_else(|| ConfigError::new("$.template.parameters.platform", "is required"))?
            .parse()
            .map_err(|message| ConfigError::new("$.template.parameters.platform", message))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum Platform {
    #[serde(rename = "linux/amd64")]
    LinuxAmd64,
    #[serde(rename = "linux/arm64")]
    LinuxArm64,
}

impl Platform {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxAmd64 => "linux/amd64",
            Self::LinuxArm64 => "linux/arm64",
        }
    }
}

impl Display for Platform {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "linux/amd64" => Ok(Self::LinuxAmd64),
            "linux/arm64" => Ok(Self::LinuxArm64),
            _ => Err(format!(
                "unsupported platform `{value}`; expected linux/amd64 or linux/arm64"
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Resources {
    pub cpu: u16,
    pub memory_mb: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Environment {
    pub allow: Vec<String>,
    pub secrets: Vec<SecretRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretRef {
    pub environment: String,
    pub secret: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Artifacts {
    pub directories: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Step {
    pub name: String,
    pub run: String,
}

/// CLI-controlled run inputs. Build and test commands deliberately cannot be overridden.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CliOverrides {
    pub repository: Option<String>,
    pub git_ref: Option<String>,
    pub platform: Option<Platform>,
    pub push: bool,
    pub report: Option<PathBuf>,
    pub keep_on_failure: bool,
    pub recurse_submodules: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionRequest {
    pub repository: Option<String>,
    pub git_ref: Option<String>,
    pub platform: Platform,
    pub push: bool,
    pub report: Option<PathBuf>,
    pub keep_on_failure: bool,
    pub recurse_submodules: bool,
}

impl ExecutionRequest {
    /// Resolve the finite set of runtime overrides. Repository build logic always
    /// remains in `Config` and is not copied into the override type.
    pub fn resolve(config: &Config, cli: CliOverrides) -> Self {
        Self {
            repository: cli.repository,
            git_ref: cli.git_ref,
            platform: cli
                .platform
                .or_else(|| config.legacy.as_ref().map(|template| template.platform))
                .or_else(|| config.template.platform().ok())
                .expect("validated configurations always define a platform"),
            push: cli.push,
            report: cli.report,
            keep_on_failure: cli.keep_on_failure,
            recurse_submodules: cli.recurse_submodules,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ConfigError {
    path: String,
    message: String,
}

impl ConfigError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl Error for ConfigError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    version: u8,
    template: serde_yaml::Value,
    #[serde(default)]
    build: Vec<RawStep>,
    #[serde(default)]
    test: Vec<RawStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTemplateSelection {
    id: String,
    #[serde(default)]
    parameters: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLegacyTemplate {
    name: String,
    platform: Platform,
    image: String,
    timeout_seconds: u32,
    resources: RawResources,
    environment: RawEnvironment,
    artifacts: RawArtifacts,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResources {
    cpu: u16,
    memory_mb: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEnvironment {
    allow: Vec<String>,
    secrets: Vec<RawSecretRef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecretRef {
    environment: String,
    secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawArtifacts {
    directories: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    name: String,
    run: String,
}

impl Config {
    pub fn parse_yaml(source: &str) -> Result<Self, ConfigError> {
        let deserializer = serde_yaml::Deserializer::from_str(source);
        let raw: RawConfig = serde_path_to_error::deserialize(deserializer).map_err(|error| {
            let path = error.path().to_string();
            ConfigError::new(yaml_path(&path), error.into_inner().to_string())
        })?;
        raw.validate()
    }
}

fn yaml_path(path: &str) -> String {
    if path.is_empty() || path == "." {
        "$".to_owned()
    } else if path.starts_with('[') {
        format!("${path}")
    } else {
        format!("$.{path}")
    }
}

impl RawConfig {
    fn validate(self) -> Result<Config, ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::new("$.version", "expected version 1"));
        }
        let RawConfig {
            template,
            build,
            test,
            ..
        } = self;
        let is_selection = template
            .as_mapping()
            .is_some_and(|mapping| mapping.contains_key("id"));
        if is_selection {
            Self::validate_selection(parse_template_value(template)?, build, test)
        } else {
            Self::validate_legacy(parse_template_value(template)?, build, test)
        }
    }

    fn validate_selection(
        template: RawTemplateSelection,
        build: Vec<RawStep>,
        test: Vec<RawStep>,
    ) -> Result<Config, ConfigError> {
        require_non_empty("$.template.id", &template.id)?;
        for name in template.parameters.keys() {
            require_non_empty("$.template.parameters", name)?;
        }
        let selection = TemplateSelection {
            id: template.id,
            parameters: template.parameters,
        };
        selection.platform()?;
        if !build.is_empty() {
            return Err(ConfigError::new(
                "$.build",
                "central templates own build steps; remove this field",
            ));
        }
        if !test.is_empty() {
            return Err(ConfigError::new(
                "$.test",
                "central templates own test steps; remove this field",
            ));
        }
        Ok(Config {
            version: ConfigVersion::V1,
            template: selection,
            legacy: None,
            build: Vec::new(),
            test: Vec::new(),
        })
    }

    fn validate_legacy(
        template: RawLegacyTemplate,
        build: Vec<RawStep>,
        test: Vec<RawStep>,
    ) -> Result<Config, ConfigError> {
        require_non_empty("$.template.name", &template.name)?;
        require_non_empty("$.template.image", &template.image)?;
        if template.timeout_seconds == 0 {
            return Err(ConfigError::new(
                "$.template.timeout_seconds",
                "must be greater than zero",
            ));
        }
        if template.resources.cpu == 0 {
            return Err(ConfigError::new(
                "$.template.resources.cpu",
                "must be greater than zero",
            ));
        }
        if template.resources.memory_mb == 0 {
            return Err(ConfigError::new(
                "$.template.resources.memory_mb",
                "must be greater than zero",
            ));
        }
        validate_steps("build", &build)?;
        validate_steps("test", &test)?;

        let mut environment_names = HashSet::new();
        for (index, name) in template.environment.allow.iter().enumerate() {
            validate_environment_name(&format!("$.template.environment.allow[{index}]"), name)?;
            if !environment_names.insert(name.as_str()) {
                return Err(ConfigError::new(
                    format!("$.template.environment.allow[{index}]"),
                    "duplicate environment variable",
                ));
            }
        }

        let mut secret_environments = HashSet::new();
        for (index, secret) in template.environment.secrets.iter().enumerate() {
            validate_environment_name(
                &format!("$.template.environment.secrets[{index}].environment"),
                &secret.environment,
            )?;
            require_non_empty(
                &format!("$.template.environment.secrets[{index}].secret"),
                &secret.secret,
            )?;
            if !secret_environments.insert(secret.environment.as_str()) {
                return Err(ConfigError::new(
                    format!("$.template.environment.secrets[{index}].environment"),
                    "duplicate secret target environment variable",
                ));
            }
        }

        if template.artifacts.directories.is_empty() {
            return Err(ConfigError::new(
                "$.template.artifacts.directories",
                "must contain at least one directory",
            ));
        }
        for (index, directory) in template.artifacts.directories.iter().enumerate() {
            if !safe_relative_directory(directory) {
                return Err(ConfigError::new(
                    format!("$.template.artifacts.directories[{index}]"),
                    "must be a non-empty relative path without `..`",
                ));
            }
        }

        Ok(Config {
            version: ConfigVersion::V1,
            template: TemplateSelection {
                id: template.name.clone(),
                parameters: BTreeMap::from([(
                    "platform".to_owned(),
                    template.platform.to_string(),
                )]),
            },
            legacy: Some(LegacyTemplate {
                name: template.name,
                platform: template.platform,
                image: template.image,
                timeout_seconds: template.timeout_seconds,
                resources: Resources {
                    cpu: template.resources.cpu,
                    memory_mb: template.resources.memory_mb,
                },
                environment: Environment {
                    allow: template.environment.allow,
                    secrets: template
                        .environment
                        .secrets
                        .into_iter()
                        .map(|secret| SecretRef {
                            environment: secret.environment,
                            secret: secret.secret,
                        })
                        .collect(),
                },
                artifacts: Artifacts {
                    directories: template.artifacts.directories,
                },
            }),
            build: build
                .into_iter()
                .map(|step| Step {
                    name: step.name,
                    run: step.run,
                })
                .collect(),
            test: test
                .into_iter()
                .map(|step| Step {
                    name: step.name,
                    run: step.run,
                })
                .collect(),
        })
    }
}

fn parse_template_value<T: for<'de> Deserialize<'de>>(
    value: serde_yaml::Value,
) -> Result<T, ConfigError> {
    serde_path_to_error::deserialize(value.into_deserializer()).map_err(|error| {
        let suffix = error.path().to_string();
        let path = if suffix.is_empty() || suffix == "." {
            "$.template".to_owned()
        } else if suffix.starts_with('[') {
            format!("$.template{suffix}")
        } else {
            format!("$.template.{suffix}")
        };
        ConfigError::new(path, error.into_inner().to_string())
    })
}

fn require_non_empty(path: &str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        Err(ConfigError::new(path, "must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_steps(kind: &str, steps: &[RawStep]) -> Result<(), ConfigError> {
    if steps.is_empty() {
        return Err(ConfigError::new(
            format!("$.{kind}"),
            "must contain at least one step",
        ));
    }
    for (index, step) in steps.iter().enumerate() {
        require_non_empty(&format!("$.{kind}[{index}].name"), &step.name)?;
        require_non_empty(&format!("$.{kind}[{index}].run"), &step.run)?;
    }
    Ok(())
}

fn validate_environment_name(path: &str, name: &str) -> Result<(), ConfigError> {
    let valid = !name.is_empty()
        && name.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_uppercase()
                || (index > 0 && character.is_ascii_digit())
        });
    if valid {
        Ok(())
    } else {
        Err(ConfigError::new(path, "must match [A-Z_][A-Z0-9_]*"))
    }
}

fn safe_relative_directory(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version: 1
template:
  name: rust
  platform: linux/amd64
  image: rust:1.97
  timeout_seconds: 900
  resources:
    cpu: 2
    memory_mb: 4096
  environment:
    allow: [CI, RUST_LOG]
    secrets:
      - environment: CARGO_TOKEN
        secret: crates-io-token
  artifacts:
    directories: [target/release, reports]
build:
  - name: compile
    run: cargo build --locked
test:
  - name: unit
    run: cargo test --locked
"#;

    const SELECTION: &str = r#"
version: 1
template:
  id: rust-bazel
  parameters:
    platform: linux/amd64
    rust_version: "1.97.0"
"#;

    #[test]
    fn valid_v1_parses_to_the_domain_model() {
        let config = Config::parse_yaml(VALID).unwrap();
        assert_eq!(config.version, ConfigVersion::V1);
        let legacy = config.legacy.as_ref().unwrap();
        assert_eq!(legacy.platform, Platform::LinuxAmd64);
        assert_eq!(legacy.resources.memory_mb, 4096);
        assert_eq!(config.build[0].run, "cargo build --locked");
        assert_eq!(legacy.environment.secrets[0].secret, "crates-io-token");
    }

    #[test]
    fn central_template_selection_contains_only_id_and_parameters() {
        let config = Config::parse_yaml(SELECTION).unwrap();
        assert_eq!(config.template.id, "rust-bazel");
        assert_eq!(config.template.platform().unwrap(), Platform::LinuxAmd64);
        assert!(config.legacy.is_none());
        assert!(config.build.is_empty());
        assert!(config.test.is_empty());
    }

    #[test]
    fn missing_required_field_includes_its_parent_path() {
        let error = Config::parse_yaml(&VALID.replace("    memory_mb: 4096\n", "")).unwrap_err();
        assert_eq!(error.path(), "$.template.resources");
        assert!(error.to_string().contains("missing field `memory_mb`"));
    }

    #[test]
    fn unknown_field_is_rejected_with_a_path() {
        let error = Config::parse_yaml(
            &VALID.replace("  name: rust\n", "  name: rust\n  dockerfile: Dockerfile\n"),
        )
        .unwrap_err();
        assert_eq!(error.path(), "$.template.dockerfile");
        assert!(error.to_string().contains("unknown field `dockerfile`"));
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let error = Config::parse_yaml(&VALID.replacen("version: 1", "version: 2", 1)).unwrap_err();
        assert_eq!(error.path(), "$.version");
    }

    #[test]
    fn invalid_platform_is_rejected_with_a_path() {
        let error = Config::parse_yaml(&VALID.replace("linux/amd64", "windows/amd64")).unwrap_err();
        assert_eq!(error.path(), "$.template.platform");
    }

    #[test]
    fn zero_resources_are_rejected_with_exact_paths() {
        for (needle, replacement, path) in [
            ("cpu: 2", "cpu: 0", "$.template.resources.cpu"),
            (
                "memory_mb: 4096",
                "memory_mb: 0",
                "$.template.resources.memory_mb",
            ),
        ] {
            let error = Config::parse_yaml(&VALID.replace(needle, replacement)).unwrap_err();
            assert_eq!(error.path(), path);
        }
    }

    #[test]
    fn unsafe_artifact_directory_is_rejected() {
        let error = Config::parse_yaml(&VALID.replace("target/release, reports", "../outside"))
            .unwrap_err();
        assert_eq!(error.path(), "$.template.artifacts.directories[0]");
    }

    #[test]
    fn cli_platform_overrides_config_without_changing_build_logic() {
        let config = Config::parse_yaml(VALID).unwrap();
        let build = config.build.clone();
        let request = ExecutionRequest::resolve(
            &config,
            CliOverrides {
                platform: Some(Platform::LinuxArm64),
                push: true,
                ..CliOverrides::default()
            },
        );
        assert_eq!(request.platform, Platform::LinuxArm64);
        assert!(request.push);
        assert_eq!(config.build, build);
    }

    #[test]
    fn config_platform_is_used_without_cli_override() {
        let config = Config::parse_yaml(VALID).unwrap();
        let request = ExecutionRequest::resolve(&config, CliOverrides::default());
        assert_eq!(request.platform, Platform::LinuxAmd64);
    }
}
