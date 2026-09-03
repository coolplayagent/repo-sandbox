//! Central environment template catalog and deterministic dependency planning.

use crate::config::{Platform, TemplateSelection};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ComponentDefinition {
    pub id: String,
    pub version: String,
    pub target_platforms: Vec<Platform>,
    pub build_context: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TemplateDefinition {
    pub id: String,
    pub version: String,
    pub base_image: String,
    pub components: Vec<TemplateComponent>,
    pub target_platforms: Vec<Platform>,
    pub build_context: PathBuf,
    #[serde(default)]
    pub parameters: BTreeMap<String, ParameterDefinition>,
    pub execution: ExecutionDefinition,
}

/// Versioned central execution profile. Repository configuration can select and
/// parameterize a profile, but cannot replace commands or safety limits.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDefinition {
    pub version: u8,
    pub build: Vec<ExecutionStep>,
    pub test: Vec<ExecutionStep>,
    pub resources: ExecutionResources,
    pub timeout_seconds: u32,
    #[serde(default = "default_true")]
    pub fail_fast: bool,
    #[serde(default)]
    pub environment_allow: Vec<String>,
    #[serde(default)]
    pub secret_environment: Vec<String>,
    #[serde(default)]
    pub artifact_directories: Vec<PathBuf>,
    #[serde(default)]
    pub registry: Option<RegistryPolicy>,
}

impl Default for ExecutionDefinition {
    fn default() -> Self {
        Self {
            version: 1,
            build: vec![ExecutionStep {
                name: "build".into(),
                command: "true".into(),
            }],
            test: vec![ExecutionStep {
                name: "test".into(),
                command: "true".into(),
            }],
            resources: ExecutionResources {
                cpu: 1,
                memory_mb: 512,
                temporary_storage_mb: 1024,
            },
            timeout_seconds: 300,
            fail_fast: true,
            environment_allow: Vec::new(),
            secret_environment: Vec::new(),
            artifact_directories: Vec::new(),
            registry: None,
        }
    }
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionStep {
    pub name: String,
    pub command: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionResources {
    pub cpu: u16,
    pub memory_mb: u32,
    pub temporary_storage_mb: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryPolicy {
    pub repository: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TemplateComponent {
    pub id: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ParameterDefinition {
    pub default: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateCatalog {
    templates: Vec<TemplateDefinition>,
    components: Vec<ComponentDefinition>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TemplatePlan {
    pub template_id: String,
    pub template_version: String,
    pub base_image: String,
    pub platform: Platform,
    /// Platforms supported by the template and every selected component.
    pub target_platforms: Vec<Platform>,
    pub build_context: PathBuf,
    pub parameters: BTreeMap<String, String>,
    pub stages: Vec<PlanStage>,
    pub execution: ExecutionDefinition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlanStage {
    pub id: String,
    pub version: String,
    pub build_context: PathBuf,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanError {
    path: String,
    message: String,
}

impl PlanError {
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

impl Display for PlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl Error for PlanError {}

impl TemplateCatalog {
    pub fn from_yaml_sources(
        template_sources: &[&str],
        component_sources: &[&str],
    ) -> Result<Self, PlanError> {
        let templates = template_sources
            .iter()
            .enumerate()
            .map(|(index, source)| parse_yaml(source, &format!("$.templates[{index}]")))
            .collect::<Result<Vec<_>, _>>()?;
        let components = component_sources
            .iter()
            .enumerate()
            .map(|(index, source)| parse_yaml(source, &format!("$.components[{index}]")))
            .collect::<Result<Vec<_>, _>>()?;
        let catalog = Self {
            templates,
            components,
        };
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn builtin() -> Result<Self, PlanError> {
        Self::from_yaml_sources(
            &[include_str!("../../../templates/rust-bazel/template.yaml")],
            &[
                include_str!("../../../templates/components/base-tools/component.yaml"),
                include_str!("../../../templates/components/bazel/component.yaml"),
                include_str!("../../../templates/components/rust/component.yaml"),
            ],
        )
    }

    pub fn plan(
        &self,
        selection: &TemplateSelection,
        platform: Platform,
    ) -> Result<TemplatePlan, PlanError> {
        let (template_index, template) = self
            .templates
            .iter()
            .enumerate()
            .find(|(_, template)| template.id == selection.id)
            .ok_or_else(|| {
                PlanError::new(
                    "$.template.id",
                    format!("central template `{}` does not exist", selection.id),
                )
            })?;
        let template_path = format!("$.templates[{template_index}]");
        if !template.target_platforms.contains(&platform) {
            return Err(PlanError::new(
                format!("{template_path}.target_platforms"),
                format!("template `{}` does not support `{platform}`", template.id),
            ));
        }

        let mut parameters = template
            .parameters
            .iter()
            .map(|(name, definition)| (name.clone(), definition.default.clone()))
            .collect::<BTreeMap<_, _>>();
        for (name, value) in &selection.parameters {
            if name == "platform" {
                continue;
            }
            if !template.parameters.contains_key(name) {
                return Err(PlanError::new(
                    format!("$.template.parameters.{name}"),
                    format!("template `{}` does not declare this parameter", template.id),
                ));
            }
            parameters.insert(name.clone(), value.clone());
        }

        let component_by_id = self
            .components
            .iter()
            .map(|component| (component.id.as_str(), component))
            .collect::<BTreeMap<_, _>>();
        let selected_ids = template
            .components
            .iter()
            .map(|component| component.id.as_str())
            .collect::<BTreeSet<_>>();
        let mut indegree = BTreeMap::<&str, usize>::new();
        let mut dependents = BTreeMap::<&str, BTreeSet<&str>>::new();

        for (index, selected) in template.components.iter().enumerate() {
            let path = format!("{template_path}.components[{index}]");
            let component = component_by_id.get(selected.id.as_str()).ok_or_else(|| {
                PlanError::new(
                    format!("{path}.id"),
                    format!("central component `{}` does not exist", selected.id),
                )
            })?;
            if !component.target_platforms.contains(&platform) {
                return Err(PlanError::new(
                    format!("{path}.id"),
                    format!("component `{}` does not support `{platform}`", selected.id),
                ));
            }
            indegree.insert(&selected.id, selected.depends_on.len());
            for (dependency_index, dependency) in selected.depends_on.iter().enumerate() {
                if !selected_ids.contains(dependency.as_str()) {
                    return Err(PlanError::new(
                        format!("{path}.depends_on[{dependency_index}]"),
                        format!("component dependency `{dependency}` is not selected"),
                    ));
                }
                dependents
                    .entry(dependency)
                    .or_default()
                    .insert(&selected.id);
            }
        }

        let mut ready = indegree
            .iter()
            .filter_map(|(id, count)| (*count == 0).then_some(*id))
            .collect::<BTreeSet<_>>();
        let mut ordered = Vec::with_capacity(template.components.len());
        while let Some(id) = ready.pop_first() {
            ordered.push(id);
            if let Some(children) = dependents.get(id) {
                for child in children {
                    let count = indegree
                        .get_mut(child)
                        .expect("each dependent is a selected component");
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(child);
                    }
                }
            }
        }
        if ordered.len() != template.components.len() {
            let cyclic = indegree
                .iter()
                .filter_map(|(id, count)| (*count > 0).then_some(*id))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(PlanError::new(
                format!("{template_path}.components"),
                format!("dependency cycle contains: {cyclic}"),
            ));
        }

        let selected_by_id = template
            .components
            .iter()
            .map(|component| (component.id.as_str(), component))
            .collect::<BTreeMap<_, _>>();
        let stages = ordered
            .into_iter()
            .map(|id| {
                let definition = component_by_id[id];
                let selected = selected_by_id[id];
                PlanStage {
                    id: id.to_owned(),
                    version: definition.version.clone(),
                    build_context: definition.build_context.clone(),
                    depends_on: selected.depends_on.clone(),
                }
            })
            .collect();

        let target_platforms = template
            .target_platforms
            .iter()
            .copied()
            .filter(|target| {
                template.components.iter().all(|selected| {
                    component_by_id[selected.id.as_str()]
                        .target_platforms
                        .contains(target)
                })
            })
            .collect();
        let mut execution = template.execution.clone();
        if let Some(registry) = &mut execution.registry {
            registry.repository = interpolate(&registry.repository, &parameters);
            registry.aliases = registry
                .aliases
                .iter()
                .map(|value| interpolate(value, &parameters))
                .collect();
            if registry.repository.trim().is_empty() {
                execution.registry = None;
            }
        }
        Ok(TemplatePlan {
            template_id: template.id.clone(),
            template_version: template.version.clone(),
            base_image: interpolate(&template.base_image, &parameters),
            platform,
            target_platforms,
            build_context: template.build_context.clone(),
            parameters,
            stages,
            execution,
        })
    }

    fn validate(&self) -> Result<(), PlanError> {
        let mut ids = BTreeSet::new();
        for (index, template) in self.templates.iter().enumerate() {
            let path = format!("$.templates[{index}]");
            validate_id_and_version(&path, &template.id, &template.version)?;
            if !ids.insert(template.id.as_str()) {
                return Err(PlanError::new(
                    format!("{path}.id"),
                    format!("duplicate template ID `{}`", template.id),
                ));
            }
            require_non_empty(&format!("{path}.base_image"), &template.base_image)?;
            validate_context(&format!("{path}.build_context"), &template.build_context)?;
            validate_platforms(
                &format!("{path}.target_platforms"),
                &template.target_platforms,
            )?;
            validate_execution(&format!("{path}.execution"), &template.execution)?;
            let mut component_ids = BTreeSet::new();
            for (component_index, component) in template.components.iter().enumerate() {
                require_non_empty(
                    &format!("{path}.components[{component_index}].id"),
                    &component.id,
                )?;
                if !component_ids.insert(component.id.as_str()) {
                    return Err(PlanError::new(
                        format!("{path}.components[{component_index}].id"),
                        format!("duplicate component ID `{}` in template", component.id),
                    ));
                }
                let mut dependencies = BTreeSet::new();
                for (dependency_index, dependency) in component.depends_on.iter().enumerate() {
                    require_non_empty(
                        &format!(
                            "{path}.components[{component_index}].depends_on[{dependency_index}]"
                        ),
                        dependency,
                    )?;
                    if !dependencies.insert(dependency.as_str()) {
                        return Err(PlanError::new(
                            format!(
                                "{path}.components[{component_index}].depends_on[{dependency_index}]"
                            ),
                            format!("duplicate dependency `{dependency}`"),
                        ));
                    }
                }
            }
        }

        ids.clear();
        for (index, component) in self.components.iter().enumerate() {
            let path = format!("$.components[{index}]");
            validate_id_and_version(&path, &component.id, &component.version)?;
            if !ids.insert(component.id.as_str()) {
                return Err(PlanError::new(
                    format!("{path}.id"),
                    format!("duplicate component ID `{}`", component.id),
                ));
            }
            validate_context(&format!("{path}.build_context"), &component.build_context)?;
            validate_platforms(
                &format!("{path}.target_platforms"),
                &component.target_platforms,
            )?;
        }
        Ok(())
    }
}

fn parse_yaml<T: for<'de> Deserialize<'de>>(source: &str, root: &str) -> Result<T, PlanError> {
    let deserializer = serde_yaml::Deserializer::from_str(source);
    serde_path_to_error::deserialize(deserializer).map_err(|error| {
        let suffix = error.path().to_string();
        let path = if suffix.is_empty() || suffix == "." {
            root.to_owned()
        } else if suffix.starts_with('[') {
            format!("{root}{suffix}")
        } else {
            format!("{root}.{suffix}")
        };
        PlanError::new(path, error.into_inner().to_string())
    })
}

fn validate_id_and_version(path: &str, id: &str, version: &str) -> Result<(), PlanError> {
    require_non_empty(&format!("{path}.id"), id)?;
    require_non_empty(&format!("{path}.version"), version)
}

fn require_non_empty(path: &str, value: &str) -> Result<(), PlanError> {
    if value.trim().is_empty() {
        Err(PlanError::new(path, "must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_context(path: &str, context: &Path) -> Result<(), PlanError> {
    let safe = !context.as_os_str().is_empty()
        && !context.is_absolute()
        && context
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
    if safe {
        Ok(())
    } else {
        Err(PlanError::new(
            path,
            "must be a non-empty central-repository-relative path without `..`",
        ))
    }
}

fn validate_platforms(path: &str, platforms: &[Platform]) -> Result<(), PlanError> {
    if platforms.is_empty() {
        return Err(PlanError::new(path, "must contain at least one platform"));
    }
    let mut unique = BTreeSet::new();
    for platform in platforms {
        if !unique.insert(platform.as_str()) {
            return Err(PlanError::new(
                path,
                format!("duplicate platform `{platform}`"),
            ));
        }
    }
    Ok(())
}

fn validate_execution(path: &str, execution: &ExecutionDefinition) -> Result<(), PlanError> {
    if execution.version != 1 {
        return Err(PlanError::new(
            format!("{path}.version"),
            "expected version 1",
        ));
    }
    if execution.build.is_empty() || execution.test.is_empty() {
        return Err(PlanError::new(
            path,
            "build and test must each contain at least one step",
        ));
    }
    let mut names = BTreeSet::new();
    for (phase, steps) in [("build", &execution.build), ("test", &execution.test)] {
        for (index, step) in steps.iter().enumerate() {
            require_non_empty(&format!("{path}.{phase}[{index}].name"), &step.name)?;
            require_non_empty(&format!("{path}.{phase}[{index}].command"), &step.command)?;
            if !names.insert(step.name.as_str()) {
                return Err(PlanError::new(
                    format!("{path}.{phase}[{index}].name"),
                    "step names must be unique across build and test",
                ));
            }
        }
    }
    if execution.resources.cpu == 0
        || execution.resources.memory_mb == 0
        || execution.resources.temporary_storage_mb == 0
        || execution.timeout_seconds == 0
    {
        return Err(PlanError::new(
            path,
            "resource limits and timeout must be greater than zero",
        ));
    }
    for (index, directory) in execution.artifact_directories.iter().enumerate() {
        validate_context(&format!("{path}.artifact_directories[{index}]"), directory)?;
    }
    for (kind, values) in [
        ("environment_allow", &execution.environment_allow),
        ("secret_environment", &execution.secret_environment),
    ] {
        let mut seen = BTreeSet::new();
        for (index, value) in values.iter().enumerate() {
            let valid = value.bytes().enumerate().all(|(offset, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (offset > 0 && byte.is_ascii_digit())
            });
            if !valid || value.is_empty() || !seen.insert(value) {
                return Err(PlanError::new(
                    format!("{path}.{kind}[{index}]"),
                    "must be a unique POSIX environment name",
                ));
            }
        }
    }
    if let Some(registry) = &execution.registry {
        require_non_empty(&format!("{path}.registry.repository"), &registry.repository)?;
        for (index, alias) in registry.aliases.iter().enumerate() {
            require_non_empty(&format!("{path}.registry.aliases[{index}]"), alias)?;
        }
    }
    Ok(())
}

fn interpolate(value: &str, parameters: &BTreeMap<String, String>) -> String {
    parameters
        .iter()
        .fold(value.to_owned(), |resolved, (name, value)| {
            resolved.replace(&format!("${{{name}}}"), value)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPONENT: &str = r#"
id: base
version: "1"
target_platforms: [linux/amd64, linux/arm64]
build_context: templates/components/base
"#;

    fn selection() -> TemplateSelection {
        TemplateSelection {
            id: "test".to_owned(),
            parameters: BTreeMap::from([("platform".to_owned(), "linux/amd64".to_owned())]),
        }
    }

    #[test]
    fn builtin_rust_bazel_template_is_statically_valid() {
        let dockerfiles = [
            include_str!("../../../templates/rust-bazel/context/Dockerfile"),
            include_str!("../../../templates/components/base-tools/context/Dockerfile"),
            include_str!("../../../templates/components/bazel/context/Dockerfile"),
            include_str!("../../../templates/components/rust/context/Dockerfile"),
        ];
        assert!(dockerfiles.iter().all(|source| source.contains("FROM ")));
        let catalog = TemplateCatalog::builtin().unwrap();
        let selection = TemplateSelection {
            id: "rust-bazel".to_owned(),
            parameters: BTreeMap::from([
                ("platform".to_owned(), "linux/amd64".to_owned()),
                ("rust_version".to_owned(), "1.97.0".to_owned()),
            ]),
        };
        let plan = catalog.plan(&selection, Platform::LinuxAmd64).unwrap();
        assert_eq!(plan.base_image, "docker.io/library/rust:1.97.0-bookworm");
        assert_eq!(
            plan.stages
                .iter()
                .map(|stage| stage.id.as_str())
                .collect::<Vec<_>>(),
            ["base-tools", "bazel", "rust"]
        );
        let arm_plan = catalog.plan(&selection, Platform::LinuxArm64).unwrap();
        assert_eq!(arm_plan.platform, Platform::LinuxArm64);
    }

    #[test]
    fn execution_profile_is_mandatory_and_versioned() {
        let missing = r#"id: test
version: "1"
base_image: example:1
components: []
target_platforms: [linux/amd64]
build_context: templates/test
"#;
        let error = TemplateCatalog::from_yaml_sources(&[missing], &[]).unwrap_err();
        assert!(error.to_string().contains("missing field `execution`"));
        let invalid = missing.to_owned()
            + "execution: { version: 2, build: [{ name: b, command: \"true\" }], test: [{ name: t, command: \"true\" }], resources: { cpu: 1, memory_mb: 1, temporary_storage_mb: 1 }, timeout_seconds: 1 }\n";
        let error = TemplateCatalog::from_yaml_sources(&[&invalid], &[]).unwrap_err();
        assert_eq!(error.path(), "$.templates[0].execution.version");
    }

    #[test]
    fn stable_topological_order_uses_ids_to_break_ties() {
        let template = r#"
id: test
version: "1"
base_image: example:1
components:
  - id: zed
    depends_on: [base]
  - id: base
  - id: alpha
    depends_on: [base]
target_platforms: [linux/amd64]
build_context: templates/test
execution: { version: 1, build: [{ name: build, command: "true" }], test: [{ name: test, command: "true" }], resources: { cpu: 1, memory_mb: 128, temporary_storage_mb: 128 }, timeout_seconds: 1 }
"#;
        let zed = COMPONENT.replace("id: base", "id: zed");
        let alpha = COMPONENT.replace("id: base", "id: alpha");
        let catalog =
            TemplateCatalog::from_yaml_sources(&[template], &[COMPONENT, &zed, &alpha]).unwrap();
        let first = catalog.plan(&selection(), Platform::LinuxAmd64).unwrap();
        let second = catalog.plan(&selection(), Platform::LinuxAmd64).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first
                .stages
                .iter()
                .map(|stage| stage.id.as_str())
                .collect::<Vec<_>>(),
            ["base", "alpha", "zed"]
        );
    }

    #[test]
    fn missing_component_is_a_pathful_plan_error() {
        let template = r#"
id: test
version: "1"
base_image: example:1
components: [{ id: absent }]
target_platforms: [linux/amd64]
build_context: templates/test
execution: { version: 1, build: [{ name: build, command: "true" }], test: [{ name: test, command: "true" }], resources: { cpu: 1, memory_mb: 128, temporary_storage_mb: 128 }, timeout_seconds: 1 }
"#;
        let catalog = TemplateCatalog::from_yaml_sources(&[template], &[COMPONENT]).unwrap();
        let error = catalog
            .plan(&selection(), Platform::LinuxAmd64)
            .unwrap_err();
        assert_eq!(error.path(), "$.templates[0].components[0].id");
        assert!(error.to_string().contains("absent"));
    }

    #[test]
    fn duplicate_component_id_is_rejected_with_a_path() {
        let error = TemplateCatalog::from_yaml_sources(&[], &[COMPONENT, COMPONENT]).unwrap_err();
        assert_eq!(error.path(), "$.components[1].id");
    }

    #[test]
    fn duplicate_template_id_is_rejected_with_a_path() {
        let template = r#"
id: test
version: "1"
base_image: example:1
components: []
target_platforms: [linux/amd64]
build_context: templates/test
execution: { version: 1, build: [{ name: build, command: "true" }], test: [{ name: test, command: "true" }], resources: { cpu: 1, memory_mb: 128, temporary_storage_mb: 128 }, timeout_seconds: 1 }
"#;
        let error = TemplateCatalog::from_yaml_sources(&[template, template], &[]).unwrap_err();
        assert_eq!(error.path(), "$.templates[1].id");
    }

    #[test]
    fn dependency_cycle_is_rejected_at_plan_time() {
        let template = r#"
id: test
version: "1"
base_image: example:1
components:
  - { id: base, depends_on: [other] }
  - { id: other, depends_on: [base] }
target_platforms: [linux/amd64]
build_context: templates/test
execution: { version: 1, build: [{ name: build, command: "true" }], test: [{ name: test, command: "true" }], resources: { cpu: 1, memory_mb: 128, temporary_storage_mb: 128 }, timeout_seconds: 1 }
"#;
        let other = COMPONENT.replace("id: base", "id: other");
        let catalog =
            TemplateCatalog::from_yaml_sources(&[template], &[COMPONENT, &other]).unwrap();
        let error = catalog
            .plan(&selection(), Platform::LinuxAmd64)
            .unwrap_err();
        assert_eq!(error.path(), "$.templates[0].components");
        assert!(error.to_string().contains("cycle"));
    }

    #[test]
    fn unsupported_platform_is_rejected_at_plan_time() {
        let template = r#"
id: test
version: "1"
base_image: example:1
components: [{ id: base }]
target_platforms: [linux/arm64]
build_context: templates/test
execution: { version: 1, build: [{ name: build, command: "true" }], test: [{ name: test, command: "true" }], resources: { cpu: 1, memory_mb: 128, temporary_storage_mb: 128 }, timeout_seconds: 1 }
"#;
        let catalog = TemplateCatalog::from_yaml_sources(&[template], &[COMPONENT]).unwrap();
        let error = catalog
            .plan(&selection(), Platform::LinuxAmd64)
            .unwrap_err();
        assert_eq!(error.path(), "$.templates[0].target_platforms");
    }
}
