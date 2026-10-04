//! Store manifests (internal/pluginstore/manifest.go): what an installed plugin
//! records under `plugins.configs.<id>.store`, and the Home sync manifests.

use serde_yaml_ng::{Mapping, Value as Yaml};

use super::client::Release;
use super::registry::{
    INSTALL_TYPE_DIRECT, INSTALL_TYPE_GITHUB_RELEASE, InstallPlan, Plugin, SCHEMA_VERSION_V2, Source,
    normalize_install_plan, plugin_install_type, valid_plugin_id, valid_plugin_version, validate_install_plan,
    validate_plugin,
};
use super::url;
use super::version::normalize_version;
use crate::go_struct;

go_struct! {
    pub struct Manifest("pluginstore.Manifest") {
        "schema_version" omitempty => schema_version: isize,
        "id" omitempty => id: String,
        "name" omitempty => name: String,
        "description" omitempty => description: String,
        "author" omitempty => author: String,
        "version" omitempty => version: String,
        "release_tag" omitempty => release_tag: String,
        "repository" omitempty => repository: String,
        "logo" omitempty => logo: String,
        "homepage" omitempty => homepage: String,
        "license" omitempty => license: String,
        "tags" omitempty => tags: Vec<String>,
        "source_id" omitempty => source_id: String,
        "source_name" omitempty => source_name: String,
        "source_url" omitempty => source_url: String,
        "install" omitempty => install: InstallPlan,
    }
}

/// Go `ManifestFromRelease`.
pub fn manifest_from_release(source: &Source, plugin: &Plugin, release: &Release) -> Result<Manifest, String> {
    let version = release_version(release)?;
    Ok(manifest_from_plugin_base(
        source,
        plugin,
        Manifest {
            version,
            release_tag: release.tag_name.trim().to_owned(),
            repository: plugin.repository.trim().to_owned(),
            install: InstallPlan {
                install_type: INSTALL_TYPE_GITHUB_RELEASE.into(),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
}

/// Go `ManifestFromPlugin`: direct installs only.
pub fn manifest_from_plugin(source: &Source, plugin: &Plugin) -> Result<Manifest, String> {
    validate_plugin(plugin)?;
    match plugin_install_type(plugin).as_str() {
        INSTALL_TYPE_DIRECT => {
            let manifest = manifest_from_plugin_base(
                source,
                plugin,
                Manifest {
                    schema_version: SCHEMA_VERSION_V2,
                    version: plugin.version.trim().to_owned(),
                    install: normalize_install_plan(&plugin.install),
                    ..Default::default()
                },
            );
            manifest.validate()?;
            Ok(manifest)
        }
        INSTALL_TYPE_GITHUB_RELEASE => Err("github-release manifest requires a resolved release".into()),
        _ => Err(format!(
            "unsupported install type {}",
            cpa_common::gostr::quote(&plugin.install.install_type)
        )),
    }
}

fn manifest_from_plugin_base(source: &Source, plugin: &Plugin, mut base: Manifest) -> Manifest {
    base.id = plugin.id.trim().to_owned();
    base.name = plugin.name.trim().to_owned();
    base.description = plugin.description.trim().to_owned();
    base.author = plugin.author.trim().to_owned();
    base.logo = plugin.logo.trim().to_owned();
    base.homepage = plugin.homepage.trim().to_owned();
    base.license = plugin.license.trim().to_owned();
    base.tags = plugin.tags.clone();
    base.source_id = source.id.trim().to_owned();
    base.source_name = source.name.trim().to_owned();
    base.source_url = source.url.trim().to_owned();
    base
}

/// Go `ReleaseVersion`: the tag without its `v`, validated.
pub fn release_version(release: &Release) -> Result<String, String> {
    let version = normalize_version(&release.tag_name);
    if !valid_plugin_version(&version) {
        return Err(format!(
            "invalid release tag {}",
            cpa_common::gostr::quote(&release.tag_name)
        ));
    }
    Ok(version)
}

/// yaml.v3 decoding of a scalar into a Go `string` (null is empty); containers fail.
fn yaml_text(v: &Yaml) -> Option<String> {
    match v {
        Yaml::Null => Some(String::new()),
        Yaml::Sequence(_) | Yaml::Mapping(_) => None,
        Yaml::Tagged(t) => match &t.value {
            Yaml::Sequence(_) | Yaml::Mapping(_) => None,
            _ => Some(crate::config::yaml_string(&t.value)),
        },
        scalar => Some(crate::config::yaml_string(scalar)),
    }
}

/// yaml.v3 decoding into a Go `int`/`int64` (null is 0).
fn yaml_number(v: &Yaml) -> Option<i64> {
    match v {
        Yaml::Null => Some(0),
        other => crate::config::yaml_int(other),
    }
}

/// yaml.v3 decoding of the known `json`/`yaml` keys of a Go struct: `None` when a
/// field fails to decode; unknown keys are ignored, as in Go.
fn yaml_struct(v: &Yaml, mut field: impl FnMut(&str, &Yaml) -> Option<()>) -> Option<()> {
    match v {
        Yaml::Null => Some(()),
        Yaml::Mapping(m) => m
            .iter()
            .try_for_each(|(key, value)| field(&crate::config::yaml_string(key), value)),
        _ => None,
    }
}

fn yaml_list<T>(v: &Yaml, item: impl Fn(&Yaml) -> Option<T>) -> Option<Vec<T>> {
    match v {
        Yaml::Null => Some(Vec::new()),
        Yaml::Sequence(items) => items.iter().map(item).collect(),
        _ => None,
    }
}

fn yaml_artifact(v: &Yaml) -> Option<super::registry::Artifact> {
    let mut a = super::registry::Artifact::default();
    yaml_struct(v, |key, value| {
        match key {
            "goos" => a.goos = yaml_text(value)?,
            "goarch" => a.goarch = yaml_text(value)?,
            "url" => a.url = yaml_text(value)?,
            "sha256" => a.sha256 = yaml_text(value)?,
            "size" => a.size = yaml_number(value)?,
            _ => {}
        }
        Some(())
    })?;
    Some(a)
}

impl Manifest {
    /// Go `storeNode.Decode(&manifest)`: yaml.v3's typed decode of a saved store node
    /// (`plugins.configs.<id>.store`). `None` when Go's decode fails.
    // ponytail: merge keys and `!!binary` scalars are not interpreted.
    pub fn from_yaml(node: &Yaml) -> Option<Manifest> {
        let mut m = Manifest::default();
        yaml_struct(node, |key, value| {
            match key {
                "schema-version" => m.schema_version = isize::try_from(yaml_number(value)?).ok()?,
                "id" => m.id = yaml_text(value)?,
                "name" => m.name = yaml_text(value)?,
                "description" => m.description = yaml_text(value)?,
                "author" => m.author = yaml_text(value)?,
                "version" => m.version = yaml_text(value)?,
                "release-tag" => m.release_tag = yaml_text(value)?,
                "repository" => m.repository = yaml_text(value)?,
                "logo" => m.logo = yaml_text(value)?,
                "homepage" => m.homepage = yaml_text(value)?,
                "license" => m.license = yaml_text(value)?,
                "tags" => m.tags = yaml_list(value, yaml_text)?,
                "source-id" => m.source_id = yaml_text(value)?,
                "source-name" => m.source_name = yaml_text(value)?,
                "source-url" => m.source_url = yaml_text(value)?,
                "install" => {
                    yaml_struct(value, |key, value| {
                        match key {
                            "type" => m.install.install_type = yaml_text(value)?,
                            "artifacts" => m.install.artifacts = yaml_list(value, yaml_artifact)?,
                            _ => {}
                        }
                        Some(())
                    })?;
                }
                _ => {}
            }
            Some(())
        })?;
        Some(m)
    }
}

impl Manifest {
    /// Go `Manifest.Plugin`.
    pub fn plugin(&self) -> Plugin {
        Plugin {
            id: self.id.trim().to_owned(),
            name: self.name.trim().to_owned(),
            description: self.description.trim().to_owned(),
            author: self.author.trim().to_owned(),
            version: self.version.trim().to_owned(),
            repository: self.repository.trim().to_owned(),
            logo: self.logo.trim().to_owned(),
            homepage: self.homepage.trim().to_owned(),
            license: self.license.trim().to_owned(),
            tags: self.tags.clone(),
            install: normalize_install_plan(&self.install),
            ..Default::default()
        }
    }

    /// Go `Manifest.InstallType`.
    pub fn install_type(&self) -> String {
        match self.install.install_type.trim().to_lowercase() {
            t if t.is_empty() => INSTALL_TYPE_GITHUB_RELEASE.into(),
            t => t,
        }
    }

    /// Go `Manifest.Validate`.
    pub fn validate(&self) -> Result<(), String> {
        let version = self.version.trim();
        if version.is_empty() {
            return Err("missing required field version".into());
        }
        if !valid_plugin_version(&normalize_version(version)) {
            return Err(format!(
                "invalid plugin version {}",
                cpa_common::gostr::quote(&self.version)
            ));
        }
        match self.install_type().as_str() {
            INSTALL_TYPE_DIRECT => {
                if self.schema_version != 0 && self.schema_version != SCHEMA_VERSION_V2 {
                    return Err(format!("unsupported schema-version {}", self.schema_version));
                }
                validate_manifest_plugin_id(&self.id)?;
                let mut plan = normalize_install_plan(&self.install);
                plan.install_type = INSTALL_TYPE_DIRECT.into();
                if !plan.artifacts.is_empty() {
                    validate_install_plan(&plan)?;
                    return validate_pinned_artifact_urls(&plan);
                }
                validate_manifest_source_url(&self.source_url)
            }
            INSTALL_TYPE_GITHUB_RELEASE => {
                let release_tag = self.release_tag.trim();
                if release_tag.is_empty() {
                    return Err("missing required field release-tag".into());
                }
                let mut plugin = self.plugin();
                plugin.install = InstallPlan {
                    install_type: INSTALL_TYPE_GITHUB_RELEASE.into(),
                    ..Default::default()
                };
                validate_plugin(&plugin)?;
                let release_version = release_version(&Release {
                    tag_name: release_tag.into(),
                    ..Default::default()
                })?;
                if release_version != normalize_version(version) {
                    return Err(format!(
                        "release-tag {} resolves version {}, want {}",
                        cpa_common::gostr::quote(release_tag),
                        cpa_common::gostr::quote(&release_version),
                        cpa_common::gostr::quote(normalize_version(version))
                    ));
                }
                Ok(())
            }
            _ => Err(format!(
                "unsupported install type {}",
                cpa_common::gostr::quote(&self.install.install_type)
            )),
        }
    }

    /// yaml.v3 `Encode` with the yaml tags: Go's field order, zero values omitted.
    pub fn to_yaml(&self) -> Yaml {
        let mut m = Mapping::new();
        let mut text = |key: &str, value: &str| {
            if !value.is_empty() {
                m.insert(key.into(), value.into());
            }
        };
        text("id", &self.id);
        text("name", &self.name);
        text("description", &self.description);
        text("author", &self.author);
        text("version", &self.version);
        text("release-tag", &self.release_tag);
        text("repository", &self.repository);
        text("logo", &self.logo);
        text("homepage", &self.homepage);
        text("license", &self.license);
        let mut out = Mapping::new();
        if self.schema_version != 0 {
            out.insert("schema-version".into(), (self.schema_version as i64).into());
        }
        out.extend(m);
        if !self.tags.is_empty() {
            out.insert(
                "tags".into(),
                Yaml::Sequence(self.tags.iter().map(|t| t.clone().into()).collect()),
            );
        }
        for (key, value) in [
            ("source-id", &self.source_id),
            ("source-name", &self.source_name),
            ("source-url", &self.source_url),
        ] {
            if !value.is_empty() {
                out.insert(key.into(), value.clone().into());
            }
        }
        let mut install = Mapping::new();
        if !self.install.install_type.is_empty() {
            install.insert("type".into(), self.install.install_type.clone().into());
        }
        if !self.install.artifacts.is_empty() {
            let artifacts = self
                .install
                .artifacts
                .iter()
                .map(|a| {
                    let mut item = Mapping::new();
                    for (key, value) in [
                        ("goos", &a.goos),
                        ("goarch", &a.goarch),
                        ("url", &a.url),
                        ("sha256", &a.sha256),
                    ] {
                        if !value.is_empty() {
                            item.insert(key.into(), value.clone().into());
                        }
                    }
                    if a.size != 0 {
                        item.insert("size".into(), a.size.into());
                    }
                    Yaml::Mapping(item)
                })
                .collect();
            install.insert("artifacts".into(), Yaml::Sequence(artifacts));
        }
        if !install.is_empty() {
            out.insert("install".into(), Yaml::Mapping(install));
        }
        Yaml::Mapping(out)
    }
}

fn validate_pinned_artifact_urls(plan: &InstallPlan) -> Result<(), String> {
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        let Some(parsed) = url::parse(artifact.url.trim()) else {
            return Err(format!("artifacts[{index}]: invalid artifact url"));
        };
        if parsed.has_user {
            return Err(format!(
                "artifacts[{index}]: pinned artifact url must not contain credentials"
            ));
        }
        if !parsed.raw_query.is_empty() || !parsed.fragment.is_empty() {
            return Err(format!(
                "artifacts[{index}]: pinned artifact url must not contain query or fragment"
            ));
        }
    }
    Ok(())
}

fn validate_manifest_plugin_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("missing required field id".into());
    }
    if !valid_plugin_id(id) {
        return Err(format!("invalid plugin id {}", cpa_common::gostr::quote(id)));
    }
    Ok(())
}

fn validate_manifest_source_url(source_url: &str) -> Result<(), String> {
    let source_url = source_url.trim();
    if source_url.is_empty() {
        return Err("missing required field source-url".into());
    }
    let parsed = match url::parse(source_url) {
        Some(u) if !u.scheme.is_empty() && !u.host.is_empty() => u,
        _ => return Err("invalid source-url".into()),
    };
    if parsed.scheme != "https" && parsed.scheme != "http" {
        return Err("source-url must use http or https".into());
    }
    if url::has_sensitive_query_parameter(&parsed) {
        return Err("source-url contains sensitive query parameter".into());
    }
    Ok(())
}
