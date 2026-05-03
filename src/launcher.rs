use crate::auth;
use crate::config::{
    Account, GameSession, LauncherConfig, app_dir, bundled_java_path, minecraft_dir,
};
use crate::events::WorkerEvent;
use crate::minecraft::{
    Artifact, AssetIndexJson, JavaRuntimeFileManifest, Library, ResolvedVersion, VersionJson,
    VersionManifest, library_allowed,
};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use sha1::{Digest, Sha1};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
use zip::ZipArchive;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const GLIDE_VERSION_JSON: &str = include_str!("../GlideClient.json");
const VERSION_MANIFEST_URL: &str =
    "https://launchermeta.mojang.com/mc/game/version_manifest_v2.json";
const LIBRARIES_BASE_URL: &str = "https://libraries.minecraft.net/";
const ASSET_BASE_URL: &str = "https://resources.download.minecraft.net/";
const JAVA_RUNTIME_ALL_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const CREATE_NO_WINDOW: u32 = 0x08000000;

pub fn prepare_and_launch(
    mut config: LauncherConfig,
    tx: Sender<WorkerEvent>,
) -> Result<Option<Account>> {
    fs::create_dir_all(app_dir()).context("failed to create .glideclient")?;
    ensure_resourcepacks_link(&tx)?;

    let updated_account = if let Some(account) = &config.account {
        let refreshed = auth::refresh_account(&config.microsoft_client_id, account, &tx)?;
        if refreshed.expires_at() != account.expires_at() {
            config.account = Some(refreshed.clone());
            let _ = tx.send(WorkerEvent::AccountUpdated(refreshed.clone()));
            Some(refreshed)
        } else {
            None
        }
    } else {
        None
    };

    let client = Client::builder()
        .user_agent("GlideClientLauncher/0.2")
        .build()
        .context("failed to build HTTP client")?;

    let resolved = prepare_distribution(&client, &tx)?;
    if config.use_bundled_java {
        let java = ensure_bundled_java(&client, &tx)?;
        config.java_path = java.to_string_lossy().to_string();
    }

    let session = config.active_session();
    let pid = launch_game(&resolved, &session, &config, &tx)?;
    let _ = tx.send(WorkerEvent::LaunchStarted(pid));

    Ok(updated_account)
}

pub fn prepare_only(mut config: LauncherConfig, tx: Sender<WorkerEvent>) -> Result<()> {
    fs::create_dir_all(app_dir()).context("failed to create .glideclient")?;
    ensure_resourcepacks_link(&tx)?;
    let client = Client::builder()
        .user_agent("GlideClientLauncher/0.2")
        .build()
        .context("failed to build HTTP client")?;

    let _ = prepare_distribution(&client, &tx)?;
    if config.use_bundled_java {
        let java = ensure_bundled_java(&client, &tx)?;
        config.java_path = java.to_string_lossy().to_string();
    }
    let _ = tx.send(WorkerEvent::Finished("Prepare complete.".to_owned()));
    Ok(())
}

fn prepare_distribution(client: &Client, tx: &Sender<WorkerEvent>) -> Result<ResolvedVersion> {
    let versions_dir = app_dir().join("versions");
    fs::create_dir_all(versions_dir.join("GlideClient"))?;
    fs::write(
        versions_dir.join("GlideClient").join("GlideClient.json"),
        GLIDE_VERSION_JSON,
    )
    .context("failed to write GlideClient version json")?;

    let glide_json: VersionJson =
        serde_json::from_str(GLIDE_VERSION_JSON).context("failed to parse GlideClient.json")?;
    let parent_id = glide_json
        .inherits_from
        .clone()
        .unwrap_or_else(|| "1.8.9".to_owned());
    let parent_json = load_or_download_version_json(client, &parent_id, tx)?;
    let resolved = ResolvedVersion::from_parent_and_child(parent_json, glide_json)?;

    ensure_client_jar(client, &resolved, tx)?;
    ensure_libraries(client, &resolved, tx)?;
    ensure_assets(client, &resolved, tx)?;

    Ok(resolved)
}

fn load_or_download_version_json(
    client: &Client,
    version_id: &str,
    tx: &Sender<WorkerEvent>,
) -> Result<VersionJson> {
    let version_dir = app_dir().join("versions").join(version_id);
    let json_path = version_dir.join(format!("{version_id}.json"));
    if json_path.exists() {
        let text = fs::read_to_string(&json_path)
            .with_context(|| format!("failed to read {}", json_path.display()))?;
        return serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", json_path.display()));
    }

    let _ = tx.send(WorkerEvent::Log(format!(
        "Downloading metadata for {version_id}..."
    )));

    let manifest = client
        .get(VERSION_MANIFEST_URL)
        .send()
        .context("failed to download Minecraft version manifest")?
        .error_for_status()
        .context("Minecraft version manifest request failed")?
        .json::<VersionManifest>()
        .context("failed to parse Minecraft version manifest")?;

    let entry = manifest
        .versions
        .into_iter()
        .find(|entry| entry.id == version_id)
        .ok_or_else(|| anyhow!("Minecraft version {version_id} was not found"))?;

    fs::create_dir_all(&version_dir)?;
    let text = client
        .get(&entry.url)
        .send()
        .with_context(|| format!("failed to download {version_id} metadata"))?
        .error_for_status()
        .with_context(|| format!("metadata request failed for {version_id}"))?
        .text()
        .context("failed to read version metadata")?;

    if let Some(expected) = entry.sha1 {
        verify_text_sha1(&text, &expected)
            .with_context(|| format!("sha1 check failed for {version_id}.json"))?;
    }

    fs::write(&json_path, &text)
        .with_context(|| format!("failed to save {}", json_path.display()))?;
    serde_json::from_str(&text).context("failed to parse downloaded version json")
}

fn ensure_client_jar(
    client: &Client,
    resolved: &ResolvedVersion,
    tx: &Sender<WorkerEvent>,
) -> Result<()> {
    let jar_path = app_dir()
        .join("versions")
        .join(&resolved.jar_id)
        .join(format!("{}.jar", resolved.jar_id));
    let artifact = resolved.client_download.clone();
    let url = artifact
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("client jar url was missing"))?;
    download_if_needed(
        client,
        url,
        &jar_path,
        artifact.sha1.as_deref(),
        tx,
        "Minecraft client jar",
    )
}

fn ensure_libraries(
    client: &Client,
    resolved: &ResolvedVersion,
    tx: &Sender<WorkerEvent>,
) -> Result<()> {
    let mut seen = HashSet::new();
    let libraries: Vec<&Library> = resolved
        .libraries
        .iter()
        .filter(|library| library_allowed(library))
        .collect();
    let total = libraries.len() as u64;

    for (index, library) in libraries.into_iter().enumerate() {
        let _ = tx.send(WorkerEvent::Progress {
            label: format!("Library: {}", library.name),
            current: index as u64 + 1,
            total,
        });

        if let Some(artifact) = normal_artifact(library)? {
            let path = library_path(&artifact, library)?;
            if seen.insert(path.clone()) {
                let url = artifact_url(&artifact, library)?;
                download_if_needed(
                    client,
                    &url,
                    &app_dir().join("libraries").join(&path),
                    artifact.sha1.as_deref(),
                    tx,
                    &library.name,
                )?;
            }
        }

        if let Some((classifier, artifact)) = native_artifact(library)? {
            let path = library_path(&artifact, library)?;
            let url = artifact_url(&artifact, library)?;
            let dest = app_dir().join("libraries").join(&path);
            download_if_needed(
                client,
                &url,
                &dest,
                artifact.sha1.as_deref(),
                tx,
                &format!("{} ({classifier})", library.name),
            )?;
            extract_natives(&dest, library, tx)?;
        }
    }

    Ok(())
}

fn ensure_assets(
    client: &Client,
    resolved: &ResolvedVersion,
    tx: &Sender<WorkerEvent>,
) -> Result<()> {
    let assets_dir = app_dir().join("assets");
    let indexes_dir = assets_dir.join("indexes");
    fs::create_dir_all(&indexes_dir)?;

    let index_path = indexes_dir.join(format!("{}.json", resolved.asset_index.id));
    download_if_needed(
        client,
        &resolved.asset_index.url,
        &index_path,
        resolved.asset_index.sha1.as_deref(),
        tx,
        "Asset index",
    )?;

    let text = fs::read_to_string(&index_path).context("failed to read asset index")?;
    let index: AssetIndexJson =
        serde_json::from_str(&text).context("failed to parse asset index")?;
    let total = index.objects.len() as u64;

    for (done, (name, object)) in index.objects.iter().enumerate() {
        if done % 25 == 0 || done + 1 == index.objects.len() {
            let _ = tx.send(WorkerEvent::Progress {
                label: "Syncing assets".to_owned(),
                current: done as u64 + 1,
                total,
            });
        }

        let prefix = object
            .hash
            .get(0..2)
            .ok_or_else(|| anyhow!("invalid asset hash"))?;
        let object_path = assets_dir.join("objects").join(prefix).join(&object.hash);
        let url = format!("{ASSET_BASE_URL}{prefix}/{}", object.hash);
        download_if_needed(
            client,
            &url,
            &object_path,
            Some(&object.hash),
            tx,
            "Asset object",
        )?;

        if index.virtual_assets {
            copy_asset_to_named_path(
                &object_path,
                &assets_dir.join("virtual").join("legacy"),
                name,
            )?;
        }

        if index.map_to_resources {
            copy_asset_to_named_path(&object_path, &app_dir().join("resources"), name)?;
        }
    }

    Ok(())
}

fn ensure_bundled_java(client: &Client, tx: &Sender<WorkerEvent>) -> Result<PathBuf> {
    let java = bundled_java_path();
    if java.exists() {
        return Ok(java);
    }

    #[cfg(not(windows))]
    bail!("bundled Java runtime is only implemented for Windows");

    #[cfg(windows)]
    {
        let _ = tx.send(WorkerEvent::Log(
            "Downloading Mojang Java 8 runtime...".to_owned(),
        ));
        let all = client
            .get(JAVA_RUNTIME_ALL_URL)
            .send()
            .context("failed to download Java runtime index")?
            .error_for_status()
            .context("Java runtime index request failed")?
            .json::<serde_json::Value>()
            .context("failed to parse Java runtime index")?;

        let manifest = &all["windows-x64"]["jre-legacy"][0]["manifest"];
        let manifest_url = manifest["url"]
            .as_str()
            .ok_or_else(|| anyhow!("Java runtime manifest URL was missing"))?;
        let manifest_sha1 = manifest["sha1"].as_str();
        let manifest_text = client
            .get(manifest_url)
            .send()
            .context("failed to download Java runtime manifest")?
            .error_for_status()
            .context("Java runtime manifest request failed")?
            .text()
            .context("failed to read Java runtime manifest")?;
        if let Some(expected) = manifest_sha1 {
            verify_text_sha1(&manifest_text, expected)
                .context("Java runtime manifest sha1 check failed")?;
        }

        let manifest: JavaRuntimeFileManifest = serde_json::from_str(&manifest_text)
            .context("failed to parse Java runtime manifest")?;
        let root = app_dir()
            .join("runtime")
            .join("jre-legacy")
            .join("windows-x64")
            .join("jre-legacy");
        let total = manifest.files.len() as u64;

        for (index, (relative, file)) in manifest.files.iter().enumerate() {
            let dest = root.join(relative);
            match file.kind.as_str() {
                "directory" => {
                    fs::create_dir_all(&dest)?;
                }
                "file" => {
                    let artifact = file
                        .downloads
                        .as_ref()
                        .and_then(|downloads| downloads.raw.as_ref())
                        .ok_or_else(|| {
                            anyhow!("Java runtime file has no raw download: {relative}")
                        })?;
                    if index % 8 == 0 || index + 1 == manifest.files.len() {
                        let _ = tx.send(WorkerEvent::Progress {
                            label: "Installing Java 8".to_owned(),
                            current: index as u64 + 1,
                            total,
                        });
                    }
                    download_if_needed(
                        client,
                        &artifact.url,
                        &dest,
                        artifact.sha1.as_deref(),
                        tx,
                        "Java runtime file",
                    )?;
                }
                other => {
                    let _ = tx.send(WorkerEvent::Log(format!(
                        "Skipping Java runtime entry {relative} ({other})."
                    )));
                }
            }
        }

        if !java.exists() {
            bail!(
                "bundled Java was downloaded, but {} was not found",
                java.display()
            );
        }
        Ok(java)
    }
}

fn copy_asset_to_named_path(source: &Path, root: &Path, name: &str) -> Result<()> {
    let dest = root.join(name);
    if dest.exists() {
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, dest)?;
    Ok(())
}

fn launch_game(
    resolved: &ResolvedVersion,
    session: &GameSession,
    config: &LauncherConfig,
    tx: &Sender<WorkerEvent>,
) -> Result<u32> {
    let game_dir = app_dir();
    let assets_dir = game_dir.join("assets");
    let natives_dir = game_dir
        .join("versions")
        .join(&resolved.jar_id)
        .join("natives");
    let logs_dir = game_dir.join("logs");
    fs::create_dir_all(&logs_dir)?;
    let launch_log = logs_dir.join("latest-launch.log");

    let classpath = build_classpath(resolved)?;
    let mut args = vec![
        format!("-Xms{}M", config.memory_mb.min(1024)),
        format!("-Xmx{}M", config.memory_mb),
        format!("-Djava.library.path={}", natives_dir.to_string_lossy()),
        "-Dminecraft.launcher.brand=GlideLauncher".to_owned(),
        "-Dminecraft.launcher.version=0.2.0".to_owned(),
        "-cp".to_owned(),
        classpath,
        resolved.main_class.clone(),
    ];

    args.extend(game_arguments(resolved, session, &game_dir, &assets_dir));

    let java = config.java_path.trim();
    if java.is_empty() {
        bail!("Java path is empty");
    }

    let stdout = File::create(&launch_log)
        .with_context(|| format!("failed to create {}", launch_log.display()))?;
    let stderr = stdout
        .try_clone()
        .context("failed to clone launch log handle")?;

    let _ = tx.send(WorkerEvent::Log(format!(
        "Starting GlideClient with {} MB memory.",
        config.memory_mb
    )));
    let _ = tx.send(WorkerEvent::Log(format!(
        "Java: {}",
        Path::new(java).display()
    )));

    let mut command = Command::new(java);
    command
        .args(args)
        .current_dir(&game_dir)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start Java: {java}"))?;

    thread::sleep(Duration::from_secs(3));
    if let Some(status) = child
        .try_wait()
        .context("failed to inspect Minecraft process")?
    {
        let tail = read_log_tail(&launch_log, 5000).unwrap_or_default();
        bail!(
            "Minecraft closed immediately with status {status}. Log: {}\n{}",
            launch_log.display(),
            tail
        );
    }

    Ok(child.id())
}

fn build_classpath(resolved: &ResolvedVersion) -> Result<String> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();

    for library in resolved
        .libraries
        .iter()
        .filter(|library| library_allowed(library))
    {
        if let Some(artifact) = normal_artifact(library)? {
            let path = app_dir()
                .join("libraries")
                .join(library_path(&artifact, library)?);
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
    }

    paths.push(
        app_dir()
            .join("versions")
            .join(&resolved.jar_id)
            .join(format!("{}.jar", resolved.jar_id)),
    );

    let separator = if cfg!(windows) { ";" } else { ":" };
    Ok(paths
        .iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join(separator))
}

fn game_arguments(
    resolved: &ResolvedVersion,
    session: &GameSession,
    game_dir: &Path,
    assets_dir: &Path,
) -> Vec<String> {
    resolved
        .minecraft_arguments
        .split_whitespace()
        .map(|token| {
            token
                .replace("${auth_player_name}", &session.username)
                .replace("${version_name}", &resolved.id)
                .replace("${game_directory}", &game_dir.to_string_lossy())
                .replace("${assets_root}", &assets_dir.to_string_lossy())
                .replace("${asset_index_name}", &resolved.assets_id)
                .replace("${assets_index_name}", &resolved.assets_id)
                .replace("${auth_uuid}", &session.uuid)
                .replace("${auth_access_token}", &session.access_token)
                .replace("${user_properties}", &session.user_properties)
                .replace("${user_type}", &session.user_type)
        })
        .collect()
}

fn normal_artifact(library: &Library) -> Result<Option<Artifact>> {
    if let Some(downloads) = &library.downloads {
        if let Some(artifact) = &downloads.artifact {
            return Ok(Some(artifact.clone()));
        }
    }

    if library.natives.is_some() {
        return Ok(None);
    }

    Ok(Some(Artifact {
        path: Some(maven_path(&library.name, None)?),
        sha1: library.sha1.clone(),
        url: Some(format!(
            "{}{}",
            library.url.as_deref().unwrap_or(LIBRARIES_BASE_URL),
            maven_path(&library.name, None)?
        )),
    }))
}

fn native_artifact(library: &Library) -> Result<Option<(String, Artifact)>> {
    let Some(natives) = &library.natives else {
        return Ok(None);
    };

    let Some(classifier_template) = natives.get("windows") else {
        return Ok(None);
    };

    let classifier = classifier_template.replace(
        "${arch}",
        if cfg!(target_pointer_width = "64") {
            "64"
        } else {
            "32"
        },
    );

    if let Some(downloads) = &library.downloads {
        if let Some(classifiers) = &downloads.classifiers {
            if let Some(artifact) = classifiers.get(&classifier) {
                return Ok(Some((classifier, artifact.clone())));
            }
        }
    }

    let path = maven_path(&library.name, Some(&classifier))?;
    Ok(Some((
        classifier,
        Artifact {
            path: Some(path.clone()),
            sha1: None,
            url: Some(format!(
                "{}{}",
                library.url.as_deref().unwrap_or(LIBRARIES_BASE_URL),
                path
            )),
        },
    )))
}

fn library_path(artifact: &Artifact, library: &Library) -> Result<String> {
    if let Some(path) = &artifact.path {
        return Ok(path.replace('\\', "/"));
    }

    maven_path(&library.name, None)
}

fn artifact_url(artifact: &Artifact, library: &Library) -> Result<String> {
    if let Some(url) = &artifact.url {
        if !url.is_empty() {
            return Ok(url.clone());
        }
    }

    Ok(format!(
        "{}{}",
        library.url.as_deref().unwrap_or(LIBRARIES_BASE_URL),
        library_path(artifact, library)?
    ))
}

fn maven_path(name: &str, classifier: Option<&str>) -> Result<String> {
    let parts: Vec<&str> = name.split(':').collect();
    if parts.len() < 3 {
        bail!("invalid Maven coordinate: {name}");
    }

    let group = parts[0].replace('.', "/");
    let artifact = parts[1];
    let version = parts[2];
    let classifier = classifier
        .or_else(|| parts.get(3).copied())
        .map(|value| format!("-{value}"))
        .unwrap_or_default();

    Ok(format!(
        "{group}/{artifact}/{version}/{artifact}-{version}{classifier}.jar"
    ))
}

fn download_if_needed(
    client: &Client,
    url: &str,
    dest: &Path,
    sha1: Option<&str>,
    tx: &Sender<WorkerEvent>,
    label: &str,
) -> Result<()> {
    if dest.exists() {
        if let Some(expected) = sha1 {
            if sha1_file(dest)
                .map(|actual| actual == expected)
                .unwrap_or(false)
            {
                return Ok(());
            }
        } else {
            return Ok(());
        }
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let _ = tx.send(WorkerEvent::Log(format!("Downloading {label}...")));
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to download {url}"))?
        .error_for_status()
        .with_context(|| format!("download request failed: {url}"))?;

    let temp = dest.with_extension("download");
    {
        let mut file =
            File::create(&temp).with_context(|| format!("failed to create {}", temp.display()))?;
        io::copy(&mut response, &mut file).context("failed to write downloaded file")?;
        file.flush()?;
    }

    if let Some(expected) = sha1 {
        let actual = sha1_file(&temp)?;
        if actual != expected {
            let _ = fs::remove_file(&temp);
            bail!("sha1 mismatch for {label}: expected {expected}, got {actual}");
        }
    }

    if dest.exists() {
        fs::remove_file(dest).with_context(|| format!("failed to replace {}", dest.display()))?;
    }
    fs::rename(&temp, dest).with_context(|| format!("failed to move {}", dest.display()))?;
    Ok(())
}

fn sha1_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()))
}

fn verify_text_sha1(text: &str, expected: &str) -> Result<()> {
    let mut hasher = Sha1::new();
    hasher.update(text.as_bytes());
    let actual = hex::encode(hasher.finalize());
    if actual == expected {
        Ok(())
    } else {
        bail!("expected {expected}, got {actual}")
    }
}

fn read_log_tail(path: &Path, max_chars: usize) -> Result<String> {
    let text =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if text.len() <= max_chars {
        return Ok(text);
    }

    let start = text
        .char_indices()
        .rev()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(0);
    Ok(format!("...{}", &text[start..]))
}

fn extract_natives(jar_path: &Path, library: &Library, tx: &Sender<WorkerEvent>) -> Result<()> {
    let natives_dir = app_dir().join("versions").join("1.8.9").join("natives");
    fs::create_dir_all(&natives_dir)?;

    let file = File::open(jar_path)
        .with_context(|| format!("failed to open native jar {}", jar_path.display()))?;
    let mut archive = ZipArchive::new(file).context("failed to read native jar")?;
    let excludes = library
        .extract
        .as_ref()
        .map(|extract| extract.exclude.clone())
        .unwrap_or_else(|| vec!["META-INF/".to_owned()]);

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let name = entry.name().replace('\\', "/");
        if entry.is_dir() || excludes.iter().any(|exclude| name.starts_with(exclude)) {
            continue;
        }

        let Some(enclosed) = entry.enclosed_name().map(|path| path.to_owned()) else {
            continue;
        };
        let dest = natives_dir.join(enclosed);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&dest)?;
        io::copy(&mut entry, &mut out)?;
    }

    let _ = tx.send(WorkerEvent::Log(format!(
        "Extracted natives for {}.",
        library.name
    )));
    Ok(())
}

fn ensure_resourcepacks_link(tx: &Sender<WorkerEvent>) -> Result<()> {
    let game_dir = app_dir();
    fs::create_dir_all(&game_dir)?;

    let target = minecraft_dir().join("resourcepacks");
    fs::create_dir_all(&target)
        .with_context(|| format!("failed to create {}", target.display()))?;

    let link = game_dir.join("resourcepacks");
    if fs::symlink_metadata(&link).is_ok() {
        let _ = tx.send(WorkerEvent::Log(format!(
            "Resource packs link: {}",
            link.display()
        )));
        return Ok(());
    }

    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_dir(&target, &link).is_ok() {
            let _ = tx.send(WorkerEvent::Log(
                "Created resource pack symlink.".to_owned(),
            ));
            return Ok(());
        }

        let status = Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(&link)
            .arg(&target)
            .status();

        if matches!(status, Ok(status) if status.success()) {
            let _ = tx.send(WorkerEvent::Log(
                "Created resource pack junction.".to_owned(),
            ));
            return Ok(());
        }
    }

    #[cfg(not(windows))]
    {
        if std::os::unix::fs::symlink(&target, &link).is_ok() {
            return Ok(());
        }
    }

    fs::create_dir_all(&link)?;
    let _ = tx.send(WorkerEvent::Log(format!(
        "Could not create a resource pack link; using {}.",
        link.display()
    )));
    Ok(())
}
