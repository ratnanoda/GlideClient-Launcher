use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionManifest {
    pub versions: Vec<ManifestVersion>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManifestVersion {
    pub id: String,
    pub url: String,
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionJson {
    pub id: String,
    #[serde(default, rename = "inheritsFrom")]
    pub inherits_from: Option<String>,
    #[serde(default)]
    pub jar: Option<String>,
    #[serde(default)]
    pub assets: Option<String>,
    #[serde(default)]
    pub asset_index: Option<AssetIndexInfo>,
    #[serde(default)]
    pub downloads: Option<HashMap<String, Artifact>>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default, rename = "mainClass")]
    pub main_class: Option<String>,
    #[serde(default, rename = "minecraftArguments")]
    pub minecraft_arguments: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndexInfo {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub downloads: Option<LibraryDownloads>,
    #[serde(default)]
    pub natives: Option<HashMap<String, String>>,
    #[serde(default)]
    pub rules: Option<Vec<Rule>>,
    #[serde(default)]
    pub extract: Option<ExtractRule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LibraryDownloads {
    #[serde(default)]
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<RuleOs>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuleOs {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractRule {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndexJson {
    #[serde(default)]
    pub objects: HashMap<String, AssetObject>,
    #[serde(default, rename = "virtual")]
    pub virtual_assets: bool,
    #[serde(default)]
    pub map_to_resources: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetObject {
    pub hash: String,
    #[serde(default, rename = "size")]
    pub _size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JavaRuntimeFileManifest {
    #[serde(default)]
    pub files: HashMap<String, JavaRuntimeFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JavaRuntimeFile {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub downloads: Option<JavaRuntimeDownloads>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JavaRuntimeDownloads {
    #[serde(default)]
    pub raw: Option<JavaRuntimeArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JavaRuntimeArtifact {
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedVersion {
    pub id: String,
    pub jar_id: String,
    pub assets_id: String,
    pub asset_index: AssetIndexInfo,
    pub client_download: Artifact,
    pub libraries: Vec<Library>,
    pub main_class: String,
    pub minecraft_arguments: String,
}

impl ResolvedVersion {
    pub fn from_parent_and_child(parent: VersionJson, child: VersionJson) -> anyhow::Result<Self> {
        let mut libraries = parent.libraries;
        libraries.extend(child.libraries);

        let asset_index = child
            .asset_index
            .or(parent.asset_index)
            .ok_or_else(|| anyhow::anyhow!("asset index was missing"))?;

        let client_download = parent
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.get("client"))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("client jar download was missing"))?;

        let assets_id = child
            .assets
            .or(parent.assets)
            .unwrap_or_else(|| asset_index.id.clone());

        Ok(Self {
            id: child.id.clone(),
            jar_id: child.jar.unwrap_or(parent.id),
            assets_id,
            asset_index,
            client_download,
            libraries,
            main_class: child
                .main_class
                .or(parent.main_class)
                .ok_or_else(|| anyhow::anyhow!("main class was missing"))?,
            minecraft_arguments: child
                .minecraft_arguments
                .or(parent.minecraft_arguments)
                .ok_or_else(|| anyhow::anyhow!("minecraft arguments were missing"))?,
        })
    }
}

pub fn library_allowed(library: &Library) -> bool {
    let Some(rules) = &library.rules else {
        return true;
    };

    let mut allowed = false;
    for rule in rules {
        if rule_matches(rule) {
            allowed = rule.action == "allow";
        }
    }

    allowed
}

fn rule_matches(rule: &Rule) -> bool {
    let Some(os) = &rule.os else {
        return true;
    };

    match os.name.as_deref() {
        Some("windows") => cfg!(target_os = "windows"),
        Some("linux") => cfg!(target_os = "linux"),
        Some("osx") => cfg!(target_os = "macos"),
        Some(_) => false,
        None => true,
    }
}
