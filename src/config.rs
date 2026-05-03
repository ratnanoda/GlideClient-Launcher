use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const APP_DIR_NAME: &str = ".glideclient";
pub const DEFAULT_CLIENT_ID: &str = "00000000402b5328";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherConfig {
    pub memory_mb: u32,
    #[serde(default = "default_use_bundled_java")]
    pub use_bundled_java: bool,
    pub java_path: String,
    pub offline_name: String,
    pub microsoft_client_id: String,
    pub account: Option<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Account {
    Microsoft {
        username: String,
        uuid: String,
        access_token: String,
        refresh_token: String,
        expires_at: u64,
    },
}

#[derive(Debug, Clone)]
pub struct GameSession {
    pub username: String,
    pub uuid: String,
    pub access_token: String,
    pub user_type: String,
    pub user_properties: String,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            memory_mb: 2048,
            use_bundled_java: true,
            java_path: find_default_java(),
            offline_name: "Player".to_owned(),
            microsoft_client_id: DEFAULT_CLIENT_ID.to_owned(),
            account: None,
        }
    }
}

impl LauncherConfig {
    pub fn load() -> Self {
        let path = config_path();
        let mut config = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        config.memory_mb = config.memory_mb.max(512);
        config
    }

    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(app_dir()).context("failed to create launcher data directory")?;
        let text = serde_json::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(config_path(), text).context("failed to write launcher config")
    }

    pub fn active_session(&self) -> GameSession {
        if let Some(Account::Microsoft {
            username,
            uuid,
            access_token,
            ..
        }) = &self.account
        {
            return GameSession {
                username: username.clone(),
                uuid: uuid.clone(),
                access_token: access_token.clone(),
                user_type: "msa".to_owned(),
                user_properties: "{}".to_owned(),
            };
        }

        let username = clean_player_name(&self.offline_name);
        GameSession {
            uuid: offline_uuid(&username),
            username,
            access_token: "0".to_owned(),
            user_type: "legacy".to_owned(),
            user_properties: "{}".to_owned(),
        }
    }
}

impl Account {
    pub fn username(&self) -> &str {
        match self {
            Self::Microsoft { username, .. } => username,
        }
    }

    pub fn expires_at(&self) -> u64 {
        match self {
            Self::Microsoft { expires_at, .. } => *expires_at,
        }
    }

    pub fn refresh_token(&self) -> &str {
        match self {
            Self::Microsoft { refresh_token, .. } => refresh_token,
        }
    }

    pub fn is_fresh(&self) -> bool {
        self.expires_at() > now_unix() + 300
    }
}

pub fn app_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(APP_DIR_NAME)
}

pub fn minecraft_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(".minecraft")
}

pub fn config_path() -> PathBuf {
    app_dir().join("launcher_config.json")
}

pub fn bundled_javaw_path() -> PathBuf {
    app_dir()
        .join("runtime")
        .join("jre-legacy")
        .join("windows-x64")
        .join("jre-legacy")
        .join("bin")
        .join("javaw.exe")
}

pub fn bundled_java_path() -> PathBuf {
    bundled_javaw_path().with_file_name("java.exe")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

pub fn clean_player_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(16)
        .collect();

    if cleaned.is_empty() {
        "Player".to_owned()
    } else {
        cleaned
    }
}

fn offline_uuid(name: &str) -> String {
    let digest = md5::compute(format!("OfflinePlayer:{name}").as_bytes());
    let mut bytes = digest.0;
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes).simple().to_string()
}

fn find_default_java() -> String {
    let mut candidates = Vec::new();

    candidates.push(bundled_javaw_path());

    if let Some(appdata) = dirs::data_dir() {
        candidates.push(
            appdata
                .join(".minecraft")
                .join("runtime")
                .join("jre-legacy")
                .join("windows-x64")
                .join("jre-legacy")
                .join("bin")
                .join("javaw.exe"),
        );
    }

    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        let java_dir = PathBuf::from(program_files).join("Java");
        collect_java8_candidates(&java_dir, &mut candidates);
    }

    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        let java_dir = PathBuf::from(program_files_x86).join("Java");
        collect_java8_candidates(&java_dir, &mut candidates);
    }

    for candidate in candidates {
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }
    }

    "javaw".to_owned()
}

fn default_use_bundled_java() -> bool {
    true
}

fn collect_java8_candidates(root: &PathBuf, candidates: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.contains("1.8") || name.contains("8") {
            candidates.push(entry.path().join("bin").join("javaw.exe"));
        }
    }
}
