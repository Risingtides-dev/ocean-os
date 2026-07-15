use ocean_extension::{
    ExtensionManifestError, ExternalKind, RawOceanExtensionManifest, RestartPolicy,
    ServiceHealthKind,
};
use semver::Version;
use std::fs;
use std::path::{Path, PathBuf};

struct Package(PathBuf);

impl Package {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ocean-extension-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(&path).expect("create package fixture");
        Self(path)
    }

    fn make(&self, relative: &str) {
        let path = self.0.join(relative);
        if Path::new(relative).extension().is_some() {
            fs::create_dir_all(path.parent().expect("fixture parent")).expect("create parent");
            fs::write(path, "fixture").expect("write fixture");
        } else {
            fs::create_dir_all(path).expect("create fixture directory");
        }
    }

    fn validate(
        &self,
        toml: &str,
        host: &str,
    ) -> Result<ocean_extension::OceanExtensionManifest, ExtensionManifestError> {
        RawOceanExtensionManifest::parse(toml)?.validate(
            &self.0,
            &Version::parse(host).expect("valid test host version"),
        )
    }
}

impl Drop for Package {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const MINIMAL: &str = r#"
schema_version = 1
id = "example.valid"
name = "Valid"
version = "1.2.3"
min_ocean_version = "0.8.0"
"#;

#[test]
fn full_and_mixed_manifests_validate_to_canonical_resources() {
    let package = Package::new("full");
    for path in [
        "plugins/tools",
        "agents/researcher",
        "skills/citations",
        "profiles/TUI",
        "services/lifecycle/run.sh",
        "external/herdr-plugin.toml",
    ] {
        package.make(path);
    }
    let manifest = package
        .validate(
            r#"
schema_version = 1
id = "risingtides.ocean-herdr"
name = "Ocean for Herdr"
version = "1.2.3"
min_ocean_version = "0.8.0"
description = "mixed package"
license = "MIT"

[package]
homepage = "https://example.test"
source = "https://example.test/source"

[trust]
project_local = false

[[plugins]]
id = "tools"
path = "plugins/tools"

[[services]]
id = "lifecycle"
entry = "services/lifecycle/run.sh"
args = ["--stdio"]
events = ["session_started"]
restart = "on-failure"
[services.health]
kind = "process"
startup_timeout_ms = 5000
[services.capabilities]
network = ["api.example.test"]
filesystem = ["project-root"]
env = ["HERDR_ENV"]
secrets = ["vault:teams/herdr-token"]

[[agents]]
id = "researcher"
path = "agents/researcher"

[[skills]]
id = "citations"
path = "skills/citations"

[[profiles]]
surface = "TUI"
path = "profiles/TUI"

[[external]]
kind = "herdr"
manifest = "external/herdr-plugin.toml"
"#,
            "1.2.3",
        )
        .expect("full manifest validates");

    assert_eq!(manifest.version, Version::new(1, 2, 3));
    assert_eq!(manifest.services[0].restart, Some(RestartPolicy::OnFailure));
    assert_eq!(
        manifest.services[0]
            .health
            .as_ref()
            .map(|health| health.kind),
        Some(ServiceHealthKind::Process)
    );
    let secret = &manifest.services[0].capabilities.secrets[0];
    assert_eq!(secret.scheme(), "vault");
    assert_eq!(secret.key(), "teams/herdr-token");
    assert_eq!(secret.to_string(), "vault:teams/herdr-token");
    assert_eq!(manifest.external[0].kind, ExternalKind::Herdr);
    assert!(manifest.plugins[0].path.is_absolute());
    assert!(manifest.services[0]
        .entry
        .starts_with(&manifest.package_root));
    assert!(manifest.agents[0].path.starts_with(&manifest.package_root));
    assert!(manifest.skills[0].path.starts_with(&manifest.package_root));
    assert!(manifest.profiles[0]
        .path
        .starts_with(&manifest.package_root));
    assert!(manifest.external[0]
        .manifest
        .starts_with(&manifest.package_root));
}

#[test]
fn required_malformed_unknown_and_schema_fields_fail_closed() {
    let package = Package::new("parse");
    for input in [
        "schema_version = 1\nid = \"example.valid\"",
        "not toml = [",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\nunknown = true",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[[plugins]]\nid = \"x\"\npath = \"x\"\nunknown = true",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[package]\nunknown = true",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[trust]\nunknown = true",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[[services]]\nid = \"svc\"\nentry = \"run\"\nunknown = true",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[[services]]\nid = \"svc\"\nentry = \"run\"\n[services.health]\nkind = \"process\"\nunknown = true",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[[services]]\nid = \"svc\"\nentry = \"run\"\n[services.capabilities]\nunknown = []",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[[profiles]]\nsurface = \"TUI\"\npath = \"profile\"\nunknown = true",
        "schema_version = 1\nid = \"example.valid\"\nname = \"x\"\nversion = \"1.0.0\"\nmin_ocean_version = \"1.0.0\"\n[[external]]\nkind = \"herdr\"\nmanifest = \"external.toml\"\nunknown = true",
    ] {
        assert!(RawOceanExtensionManifest::parse(input).is_err(), "must reject: {input}");
    }
    let error = package
        .validate(
            &MINIMAL.replace("schema_version = 1", "schema_version = 2"),
            "2.0.0",
        )
        .unwrap_err();
    assert!(matches!(
        error,
        ExtensionManifestError::UnsupportedSchemaVersion { found: 2 }
    ));
}

#[test]
fn unknown_schema_v1_discriminator_kinds_fail_parsing() {
    for (field, input) in [
        (
            "external kind",
            format!(
                "{MINIMAL}\n[[external]]\nkind = \"other\"\nmanifest = \"external.toml\"\n"
            ),
        ),
        (
            "health kind",
            format!(
                "{MINIMAL}\n[[services]]\nid = \"svc\"\nentry = \"run.sh\"\n[services.health]\nkind = \"http\"\n"
            ),
        ),
        (
            "restart policy",
            format!(
                "{MINIMAL}\n[[services]]\nid = \"svc\"\nentry = \"run.sh\"\nrestart = \"always\"\n"
            ),
        ),
    ] {
        assert!(
            RawOceanExtensionManifest::parse(&input).is_err(),
            "accepted unknown {field}"
        );
    }
}

#[test]
fn extension_id_grammar_is_enforced() {
    let package = Package::new("ids");
    for id in [
        "single",
        "Example.valid",
        "example.-bad",
        "example.bad-",
        "example.bad_label",
        ".example",
    ] {
        let input = MINIMAL.replace("example.valid", id);
        assert!(
            matches!(
                package.validate(&input, "1.0.0"),
                Err(ExtensionManifestError::InvalidExtensionId { .. })
            ),
            "accepted {id}"
        );
    }
    package
        .validate(MINIMAL, "1.0.0")
        .expect("valid reverse-domain-like id");
}

#[test]
fn semver_and_host_compatibility_use_semver_ordering() {
    let package = Package::new("semver");
    package
        .validate(MINIMAL, "0.8.0")
        .expect("equal host accepted");
    package
        .validate(MINIMAL, "0.10.0")
        .expect("newer host accepted");
    assert!(matches!(
        package.validate(MINIMAL, "0.7.9"),
        Err(ExtensionManifestError::IncompatibleHost { .. })
    ));
    for (field, bad) in [("version", "one"), ("min_ocean_version", ">=0.8")] {
        let input = if field == "version" {
            MINIMAL.replace("version = \"1.2.3\"", &format!("version = \"{bad}\""))
        } else {
            MINIMAL.replace(
                "min_ocean_version = \"0.8.0\"",
                &format!("min_ocean_version = \"{bad}\""),
            )
        };
        assert!(
            matches!(package.validate(&input, "1.0.0"), Err(ExtensionManifestError::InvalidVersion { field: actual, .. }) if actual == field)
        );
    }
}

#[test]
fn duplicate_ids_are_rejected_package_wide_across_resource_kinds() {
    let package = Package::new("duplicates");
    for path in ["plugins/shared", "agents/shared", "skills/shared", "run.sh"] {
        package.make(path);
    }
    for resources in [
        "[[plugins]]\nid = \"shared\"\npath = \"plugins/shared\"\n[[agents]]\nid = \"shared\"\npath = \"agents/shared\"",
        "[[agents]]\nid = \"shared\"\npath = \"agents/shared\"\n[[skills]]\nid = \"shared\"\npath = \"skills/shared\"",
        "[[plugins]]\nid = \"shared\"\npath = \"plugins/shared\"\n[[services]]\nid = \"shared\"\nentry = \"run.sh\"",
    ] {
        let input = format!("{MINIMAL}\n{resources}\n");
        assert!(
            matches!(package.validate(&input, "1.0.0"), Err(ExtensionManifestError::DuplicateResourceId { id }) if id == "shared"),
            "accepted duplicate IDs in {resources}"
        );
    }
}

#[test]
fn path_validation_rejects_empty_absolute_parent_and_missing_paths() {
    let package = Package::new("paths");
    for path in ["", "/tmp", "../escape", "missing"] {
        let input = format!("{MINIMAL}\n[[plugins]]\nid = \"tools\"\npath = \"{path}\"\n");
        assert!(
            matches!(
                package.validate(&input, "1.0.0"),
                Err(ExtensionManifestError::InvalidResourcePath { .. })
            ),
            "accepted {path:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn canonical_validation_rejects_symlink_escape_for_every_path_resource() {
    use std::os::unix::fs::symlink;
    let package = Package::new("symlink");
    let outside = Package::new("outside");
    outside.make("resource");
    symlink(outside.0.join("resource"), package.0.join("escape")).expect("make escape symlink");
    for resource in [
        "[[plugins]]\nid = \"p\"\npath = \"escape\"",
        "[[agents]]\nid = \"a\"\npath = \"escape\"",
        "[[skills]]\nid = \"s\"\npath = \"escape\"",
        "[[profiles]]\nsurface = \"TUI\"\npath = \"escape\"",
        "[[services]]\nid = \"svc\"\nentry = \"escape\"",
        "[[external]]\nkind = \"herdr\"\nmanifest = \"escape\"",
    ] {
        let input = format!("{MINIMAL}\n{resource}\n");
        assert!(
            matches!(package.validate(&input, "1.0.0"), Err(ExtensionManifestError::InvalidResourcePath { reason, .. }) if reason.contains("escapes")),
            "accepted symlink escape: {resource}"
        );
    }
}

#[test]
fn every_path_bearing_resource_type_is_checked() {
    let package = Package::new("all-paths");
    let cases = [
        "[[plugins]]\nid = \"p\"\npath = \"missing\"",
        "[[agents]]\nid = \"a\"\npath = \"missing\"",
        "[[skills]]\nid = \"s\"\npath = \"missing\"",
        "[[profiles]]\nsurface = \"TUI\"\npath = \"missing\"",
        "[[services]]\nid = \"svc\"\nentry = \"missing\"",
        "[[external]]\nkind = \"herdr\"\nmanifest = \"missing\"",
    ];
    for resource in cases {
        let input = format!("{MINIMAL}\n{resource}\n");
        assert!(
            matches!(
                package.validate(&input, "1.0.0"),
                Err(ExtensionManifestError::InvalidResourcePath { .. })
            ),
            "path type escaped validation: {resource}"
        );
    }
}

#[test]
fn capability_assignments_whitespace_paths_and_raw_values_are_rejected() {
    let package = Package::new("capabilities");
    package.make("run.sh");
    for (field, value) in [
        ("network", "https://api.example.test"),
        ("network", "api.example.test=token"),
        ("filesystem", "/tmp/secret"),
        ("filesystem", "../project-root"),
        ("env", "TOKEN=value"),
        ("env", "BAD NAME"),
        ("env", "../TOKEN"),
        ("secrets", "ghp_abcd1234"),
        ("secrets", "raw value"),
        ("secrets", ":missing-scheme"),
        ("secrets", "vault:"),
        ("secrets", "Vault:key"),
        ("secrets", "-vault:key"),
        ("secrets", "vault-:key"),
        ("secrets", "vault:/tmp/secret"),
        ("secrets", "vault:team/../secret"),
        ("secrets", "vault:https://secret.example"),
        ("secrets", "vault:TOKEN=value"),
        ("secrets", "vault:bad name"),
        ("secrets", "vault:bad\\tname"),
    ] {
        let input = format!("{MINIMAL}\n[[services]]\nid = \"svc\"\nentry = \"run.sh\"\n[services.capabilities]\n{field} = [\"{value}\"]\n");
        assert!(
            matches!(
                package.validate(&input, "1.0.0"),
                Err(ExtensionManifestError::InvalidCapabilityReference { .. })
            ),
            "accepted {field}={value}"
        );
    }
}

#[cfg(unix)]
#[test]
fn parsing_validation_and_inspection_execute_no_code() {
    use std::os::unix::fs::PermissionsExt;
    let package = Package::new("no-exec");
    let sentinel = package.0.join("executed");
    let script = package.0.join("plugin.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("make executable");
    let input = format!("{MINIMAL}\n[[services]]\nid = \"svc\"\nentry = \"plugin.sh\"\n");
    let validated = package
        .validate(&input, "1.0.0")
        .expect("manifest validates");
    assert_eq!(validated.services.len(), 1);
    assert!(
        !sentinel.exists(),
        "schema inspection must not execute entries"
    );
}
