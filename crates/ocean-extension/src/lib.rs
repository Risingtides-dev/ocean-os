//! Host-owned schema-v1 validation for `ocean-extension.toml` packages.
//!
//! Parsing produces [`RawOceanExtensionManifest`]. Validation is a separate,
//! non-executing step that produces [`OceanExtensionManifest`] with parsed
//! SemVer values and canonical resource paths confined to the package root.

use semver::Version;
use serde::Deserialize;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

/// The only package schema version accepted by this checkpoint.
pub const SCHEMA_VERSION: u32 = 1;

/// The fail-closed, unvalidated schema-v1 document.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawOceanExtensionManifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub min_ocean_version: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub package: Option<PackageMetadata>,
    #[serde(default)]
    pub trust: Option<TrustMetadata>,
    #[serde(default)]
    pub plugins: Vec<RawPathResource>,
    #[serde(default)]
    pub services: Vec<RawServiceResource>,
    #[serde(default)]
    pub agents: Vec<RawPathResource>,
    #[serde(default)]
    pub skills: Vec<RawPathResource>,
    #[serde(default)]
    pub profiles: Vec<RawProfileResource>,
    #[serde(default)]
    pub external: Vec<RawExternalResource>,
}

impl RawOceanExtensionManifest {
    /// Parse TOML without touching resource paths or executing package code.
    pub fn parse(input: &str) -> Result<Self, ExtensionManifestError> {
        toml::from_str(input).map_err(|error| ExtensionManifestError::Parse {
            message: error.to_string(),
        })
    }

    /// Read and parse a manifest without validating its package resources.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ExtensionManifestError> {
        let path = path.as_ref();
        let input = fs::read_to_string(path).map_err(|error| ExtensionManifestError::Read {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        Self::parse(&input)
    }

    /// Validate schema, identity, compatibility, capability references, and
    /// lexical resource paths without touching the filesystem.
    ///
    /// Descriptor-anchored inspectors use this after reading the manifest from
    /// the exact artifact handle they hashed, then prove each relative path
    /// against that same artifact inventory. Ordinary package validation uses
    /// the same metadata pass before canonical filesystem resolution.
    pub fn validate_metadata(
        self,
        host_version: &Version,
    ) -> Result<OceanExtensionMetadata, ExtensionManifestError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(ExtensionManifestError::UnsupportedSchemaVersion {
                found: self.schema_version,
            });
        }
        validate_extension_id(&self.id)?;
        let version = parse_version("version", &self.version)?;
        let min_ocean_version = parse_version("min_ocean_version", &self.min_ocean_version)?;
        if host_version < &min_ocean_version {
            return Err(ExtensionManifestError::IncompatibleHost {
                host: host_version.clone(),
                minimum: min_ocean_version,
            });
        }

        let mut ids = HashSet::new();
        validate_metadata_path_resources("plugin", &self.plugins, &mut ids)?;
        validate_metadata_path_resources("agent", &self.agents, &mut ids)?;
        validate_metadata_path_resources("skill", &self.skills, &mut ids)?;

        let mut services = Vec::with_capacity(self.services.len());
        for service in self.services {
            claim_id("service", &service.id, &mut ids)?;
            validate_resource_path_syntax("service entry", Some(&service.id), &service.entry)?;
            validate_service_events(&service.id, &service.events)?;
            let capabilities = validate_capabilities(&service.id, service.capabilities)?;
            services.push(MetadataServiceResource {
                id: service.id,
                entry: service.entry,
                args: service.args,
                events: service.events,
                restart: service.restart,
                health: service.health,
                capabilities,
            });
        }

        for profile in &self.profiles {
            validate_resource_path_syntax("profile path", Some(&profile.surface), &profile.path)?;
        }
        for resource in &self.external {
            validate_resource_path_syntax(
                "external manifest",
                Some(resource.kind.as_str()),
                &resource.manifest,
            )?;
        }

        Ok(OceanExtensionMetadata {
            schema_version: self.schema_version,
            id: self.id,
            name: self.name,
            version,
            min_ocean_version,
            description: self.description,
            license: self.license,
            package: self.package,
            trust: self.trust,
            plugins: self.plugins,
            services,
            agents: self.agents,
            skills: self.skills,
            profiles: self.profiles,
            external: self.external,
        })
    }

    /// Validate identity, compatibility, capabilities, and every declared path.
    pub fn validate(
        self,
        package_root: impl AsRef<Path>,
        host_version: &Version,
    ) -> Result<OceanExtensionManifest, ExtensionManifestError> {
        let metadata = self.validate_metadata(host_version)?;
        let package_root = canonical_directory(package_root.as_ref(), "package root")?;
        let plugins = resolve_path_resources(&package_root, "plugin", metadata.plugins)?;
        let agents = resolve_path_resources(&package_root, "agent", metadata.agents)?;
        let skills = resolve_path_resources(&package_root, "skill", metadata.skills)?;

        let mut services = Vec::with_capacity(metadata.services.len());
        for service in metadata.services {
            let entry = canonical_resource_path(
                &package_root,
                "service entry",
                Some(&service.id),
                &service.entry,
            )?;
            services.push(ServiceResource {
                id: service.id,
                entry,
                args: service.args,
                events: service.events,
                restart: service.restart,
                health: service.health,
                capabilities: service.capabilities,
            });
        }

        let mut profiles = Vec::with_capacity(metadata.profiles.len());
        for profile in metadata.profiles {
            let path = canonical_resource_path(
                &package_root,
                "profile path",
                Some(&profile.surface),
                &profile.path,
            )?;
            profiles.push(ProfileResource {
                surface: profile.surface,
                path,
            });
        }

        let mut external = Vec::with_capacity(metadata.external.len());
        for resource in metadata.external {
            let manifest = canonical_resource_path(
                &package_root,
                "external manifest",
                Some(resource.kind.as_str()),
                &resource.manifest,
            )?;
            external.push(ExternalResource {
                kind: resource.kind,
                manifest,
            });
        }

        Ok(OceanExtensionManifest {
            schema_version: metadata.schema_version,
            id: metadata.id,
            name: metadata.name,
            version: metadata.version,
            min_ocean_version: metadata.min_ocean_version,
            description: metadata.description,
            license: metadata.license,
            package: metadata.package,
            trust: metadata.trust,
            package_root,
            plugins,
            services,
            agents,
            skills,
            profiles,
            external,
        })
    }
}

/// Informational package metadata.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageMetadata {
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Informational trust request; this never grants trust.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustMetadata {
    #[serde(default)]
    pub project_local: bool,
}

/// Unvalidated ID/path resource used by plugins, agents, and skills.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPathResource {
    pub id: String,
    pub path: String,
}

/// Unvalidated supervised-service declaration. Execution is outside this crate.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawServiceResource {
    pub id: String,
    pub entry: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub restart: Option<RestartPolicy>,
    #[serde(default)]
    pub health: Option<ServiceHealth>,
    #[serde(default)]
    pub capabilities: RawRequestedCapabilities,
}

/// Supported schema-v1 service restart policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum RestartPolicy {
    #[serde(rename = "on-failure")]
    OnFailure,
}

impl RestartPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OnFailure => "on-failure",
        }
    }
}

impl fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Supported schema-v1 service health discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceHealthKind {
    Process,
}

impl ServiceHealthKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
        }
    }
}

impl fmt::Display for ServiceHealthKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Service health metadata retained for future supervision.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceHealth {
    pub kind: ServiceHealthKind,
    #[serde(default)]
    pub startup_timeout_ms: Option<u64>,
}

/// Unvalidated requested capability names and references.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRequestedCapabilities {
    #[serde(default)]
    pub network: Vec<String>,
    #[serde(default)]
    pub filesystem: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
}

/// Validated requested capabilities. These values grant nothing themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestedCapabilities {
    pub network: Vec<String>,
    pub filesystem: Vec<String>,
    pub env: Vec<String>,
    pub secrets: Vec<SecretReference>,
}

/// A host-resolvable secret reference, never a raw credential.
///
/// Schema v1 uses `<scheme>:<key>`. The scheme is lowercase ASCII
/// alphanumeric with hyphens only between alphanumeric characters. The key is
/// nonempty and contains only ASCII alphanumeric characters, `_`, `-`, `.`,
/// and `/`; it must be relative and contain no parent traversal. This syntax
/// does not stop a malicious publisher from mislabeling a value, but it keeps
/// raw credentials outside the declared contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecretReference {
    scheme: String,
    key: String,
}

impl SecretReference {
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

impl fmt::Display for SecretReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.scheme, self.key)
    }
}

/// A value did not match the schema-v1 secret-reference grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSecretReference;

impl fmt::Display for InvalidSecretReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a host-resolvable secret reference in <scheme>:<key> form")
    }
}

impl std::error::Error for InvalidSecretReference {}

impl FromStr for SecretReference {
    type Err = InvalidSecretReference;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (scheme, key) = value.split_once(':').ok_or(InvalidSecretReference)?;
        if !valid_secret_scheme(scheme) || !valid_secret_key(key) {
            return Err(InvalidSecretReference);
        }
        Ok(Self {
            scheme: scheme.to_string(),
            key: key.to_string(),
        })
    }
}

/// Unvalidated surface profile resource.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProfileResource {
    pub surface: String,
    pub path: String,
}

/// Unvalidated external-host manifest resource.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawExternalResource {
    pub kind: ExternalKind,
    pub manifest: String,
}

/// Supported schema-v1 external host discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExternalKind {
    Herdr,
}

impl ExternalKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
        }
    }
}

impl fmt::Display for ExternalKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Filesystem-independent validated package metadata.
///
/// Resource paths remain package-relative. A caller that already owns an
/// immutable descriptor-anchored artifact inventory can compare these paths to
/// that inventory without reopening attacker-controlled pathnames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OceanExtensionMetadata {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: Version,
    pub min_ocean_version: Version,
    pub description: Option<String>,
    pub license: Option<String>,
    pub package: Option<PackageMetadata>,
    pub trust: Option<TrustMetadata>,
    pub plugins: Vec<RawPathResource>,
    pub services: Vec<MetadataServiceResource>,
    pub agents: Vec<RawPathResource>,
    pub skills: Vec<RawPathResource>,
    pub profiles: Vec<RawProfileResource>,
    pub external: Vec<RawExternalResource>,
}

/// A lexically validated service declaration whose entry remains relative to
/// its package inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataServiceResource {
    pub id: String,
    pub entry: String,
    pub args: Vec<String>,
    pub events: Vec<String>,
    pub restart: Option<RestartPolicy>,
    pub health: Option<ServiceHealth>,
    pub capabilities: RequestedCapabilities,
}

/// A validated package whose filesystem resources are canonical and confined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OceanExtensionManifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: Version,
    pub min_ocean_version: Version,
    pub description: Option<String>,
    pub license: Option<String>,
    pub package: Option<PackageMetadata>,
    pub trust: Option<TrustMetadata>,
    pub package_root: PathBuf,
    pub plugins: Vec<PathResource>,
    pub services: Vec<ServiceResource>,
    pub agents: Vec<PathResource>,
    pub skills: Vec<PathResource>,
    pub profiles: Vec<ProfileResource>,
    pub external: Vec<ExternalResource>,
}

/// A validated ID-bearing resource directory/file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathResource {
    pub id: String,
    pub path: PathBuf,
}

/// A validated service declaration with a canonical entry path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceResource {
    pub id: String,
    pub entry: PathBuf,
    pub args: Vec<String>,
    pub events: Vec<String>,
    pub restart: Option<RestartPolicy>,
    pub health: Option<ServiceHealth>,
    pub capabilities: RequestedCapabilities,
}

/// A validated surface profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileResource {
    pub surface: String,
    pub path: PathBuf,
}

/// A validated external-host manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalResource {
    pub kind: ExternalKind,
    pub manifest: PathBuf,
}

/// Typed failure from parsing or validation of untrusted package input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionManifestError {
    Read {
        path: PathBuf,
        message: String,
    },
    Parse {
        message: String,
    },
    UnsupportedSchemaVersion {
        found: u32,
    },
    InvalidExtensionId {
        value: String,
    },
    InvalidVersion {
        field: &'static str,
        value: String,
        message: String,
    },
    IncompatibleHost {
        host: Version,
        minimum: Version,
    },
    DuplicateResourceId {
        id: String,
    },
    InvalidResourceId {
        kind: &'static str,
        id: String,
    },
    InvalidResourcePath {
        kind: &'static str,
        id: Option<String>,
        path: PathBuf,
        reason: String,
    },
    InvalidCapabilityReference {
        service_id: String,
        capability: &'static str,
        value: String,
    },
    InvalidServiceEvent {
        service_id: String,
        value: String,
    },
}

impl fmt::Display for ExtensionManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => write!(f, "read `{}`: {message}", path.display()),
            Self::Parse { message } => write!(f, "parse ocean extension manifest: {message}"),
            Self::UnsupportedSchemaVersion { found } => {
                write!(
                    f,
                    "unsupported schema_version {found}; expected {SCHEMA_VERSION}"
                )
            }
            Self::InvalidExtensionId { value } => write!(f, "invalid extension id `{value}`"),
            Self::InvalidVersion {
                field,
                value,
                message,
            } => {
                write!(f, "invalid {field} `{value}`: {message}")
            }
            Self::IncompatibleHost { host, minimum } => {
                write!(f, "Ocean {host} is older than required {minimum}")
            }
            Self::DuplicateResourceId { id } => write!(f, "duplicate resource id `{id}`"),
            Self::InvalidResourceId { kind, id } => write!(f, "invalid {kind} id `{id}`"),
            Self::InvalidResourcePath {
                kind,
                id,
                path,
                reason,
            } => match id {
                Some(id) => write!(
                    f,
                    "invalid {kind} for `{id}` at `{}`: {reason}",
                    path.display()
                ),
                None => write!(f, "invalid {kind} at `{}`: {reason}", path.display()),
            },
            Self::InvalidCapabilityReference {
                service_id,
                capability,
                value,
            } => write!(
                f,
                "invalid {capability} capability reference `{value}` for service `{service_id}`"
            ),
            Self::InvalidServiceEvent { service_id, value } => write!(
                f,
                "invalid lifecycle event `{value}` for service `{service_id}`"
            ),
        }
    }
}

impl std::error::Error for ExtensionManifestError {}

fn parse_version(field: &'static str, value: &str) -> Result<Version, ExtensionManifestError> {
    Version::parse(value).map_err(|error| ExtensionManifestError::InvalidVersion {
        field,
        value: value.to_string(),
        message: error.to_string(),
    })
}

fn valid_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

/// Validate the stable reverse-domain extension identity grammar.
///
/// State/inspection readers use the same grammar as package manifests so a
/// separately persisted install record cannot acquire a second identity shape.
pub fn validate_extension_id(id: &str) -> Result<(), ExtensionManifestError> {
    let labels: Vec<_> = id.split('.').collect();
    if id.bytes().any(|byte| byte.is_ascii_uppercase())
        || labels.len() < 2
        || labels.iter().any(|label| !valid_label(label))
    {
        return Err(ExtensionManifestError::InvalidExtensionId {
            value: id.to_string(),
        });
    }
    Ok(())
}

fn claim_id(
    kind: &'static str,
    id: &str,
    ids: &mut HashSet<String>,
) -> Result<(), ExtensionManifestError> {
    if !valid_label(id) {
        return Err(ExtensionManifestError::InvalidResourceId {
            kind,
            id: id.to_string(),
        });
    }
    if !ids.insert(id.to_string()) {
        return Err(ExtensionManifestError::DuplicateResourceId { id: id.to_string() });
    }
    Ok(())
}

fn validate_metadata_path_resources(
    kind: &'static str,
    resources: &[RawPathResource],
    ids: &mut HashSet<String>,
) -> Result<(), ExtensionManifestError> {
    for resource in resources {
        claim_id(kind, &resource.id, ids)?;
        validate_resource_path_syntax(kind, Some(&resource.id), &resource.path)?;
    }
    Ok(())
}

fn resolve_path_resources(
    root: &Path,
    kind: &'static str,
    resources: Vec<RawPathResource>,
) -> Result<Vec<PathResource>, ExtensionManifestError> {
    resources
        .into_iter()
        .map(|resource| {
            let path = canonical_resource_path(root, kind, Some(&resource.id), &resource.path)?;
            Ok(PathResource {
                id: resource.id,
                path,
            })
        })
        .collect()
}

fn canonical_directory(path: &Path, kind: &'static str) -> Result<PathBuf, ExtensionManifestError> {
    let canonical =
        fs::canonicalize(path).map_err(|error| ExtensionManifestError::InvalidResourcePath {
            kind,
            id: None,
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    if !canonical.is_dir() {
        return Err(ExtensionManifestError::InvalidResourcePath {
            kind,
            id: None,
            path: path.to_path_buf(),
            reason: "not a directory".to_string(),
        });
    }
    Ok(canonical)
}

fn validate_resource_path_syntax(
    kind: &'static str,
    id: Option<&str>,
    raw: &str,
) -> Result<PathBuf, ExtensionManifestError> {
    let path = Path::new(raw);
    let invalid_lexical = raw.is_empty()
        || path.components().any(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::ParentDir
            )
        });
    if invalid_lexical {
        return Err(ExtensionManifestError::InvalidResourcePath {
            kind,
            id: id.map(str::to_string),
            path: path.to_path_buf(),
            reason: "path must be nonempty, relative, and contain no parent components".to_string(),
        });
    }
    Ok(path.to_path_buf())
}

fn canonical_resource_path(
    root: &Path,
    kind: &'static str,
    id: Option<&str>,
    raw: &str,
) -> Result<PathBuf, ExtensionManifestError> {
    let path = validate_resource_path_syntax(kind, id, raw)?;
    let joined = root.join(&path);
    let canonical =
        fs::canonicalize(&joined).map_err(|error| ExtensionManifestError::InvalidResourcePath {
            kind,
            id: id.map(str::to_string),
            path: path.to_path_buf(),
            reason: error.to_string(),
        })?;
    if !canonical.starts_with(root) {
        return Err(ExtensionManifestError::InvalidResourcePath {
            kind,
            id: id.map(str::to_string),
            path: path.to_path_buf(),
            reason: "canonical path escapes package root".to_string(),
        });
    }
    Ok(canonical)
}

fn validate_service_events(
    service_id: &str,
    events: &[String],
) -> Result<(), ExtensionManifestError> {
    const EVENTS: &[&str] = &[
        "daemon_started",
        "session_started",
        "turn_started",
        "permission_requested",
        "permission_resolved",
        "tool_started",
        "tool_finished",
        "turn_finished",
        "session_stopped",
        "daemon_stopping",
    ];
    let mut seen = HashSet::new();
    for event in events {
        if !EVENTS.contains(&event.as_str()) || !seen.insert(event) {
            return Err(ExtensionManifestError::InvalidServiceEvent {
                service_id: service_id.to_string(),
                value: event.clone(),
            });
        }
    }
    Ok(())
}

fn validate_capabilities(
    service_id: &str,
    capabilities: RawRequestedCapabilities,
) -> Result<RequestedCapabilities, ExtensionManifestError> {
    for (capability, values) in [
        ("network", &capabilities.network),
        ("filesystem", &capabilities.filesystem),
    ] {
        for value in values {
            if !valid_named_reference(value) {
                return Err(ExtensionManifestError::InvalidCapabilityReference {
                    service_id: service_id.to_string(),
                    capability,
                    value: value.clone(),
                });
            }
        }
    }
    for value in &capabilities.env {
        let mut chars = value.chars();
        let valid = chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
        if !valid {
            return Err(ExtensionManifestError::InvalidCapabilityReference {
                service_id: service_id.to_string(),
                capability: "env",
                value: value.clone(),
            });
        }
    }
    let secrets = capabilities
        .secrets
        .into_iter()
        .map(|value| {
            value.parse().map_err(|InvalidSecretReference| {
                ExtensionManifestError::InvalidCapabilityReference {
                    service_id: service_id.to_string(),
                    capability: "secret",
                    value,
                }
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(RequestedCapabilities {
        network: capabilities.network,
        filesystem: capabilities.filesystem,
        env: capabilities.env,
        secrets,
    })
}

fn valid_secret_scheme(scheme: &str) -> bool {
    !scheme.bytes().any(|byte| byte.is_ascii_uppercase()) && valid_label(scheme)
}

fn valid_secret_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/'))
        && !Path::new(key).is_absolute()
        && !Path::new(key).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn valid_named_reference(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        && !value.contains("://")
}
