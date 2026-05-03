use crate::config::{Account, now_unix};
use crate::events::WorkerEvent;
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::json;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread;
use std::time::Duration;

const DEVICE_CODE_URL: &str = "https://login.live.com/oauth20_connect.srf";
const TOKEN_URL: &str = "https://login.live.com/oauth20_token.srf";
const MSA_SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL offline_access";
const XBL_AUTH_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_AUTH_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN_URL: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";
const MC_ENTITLEMENTS_URL: &str = "https://api.minecraftservices.com/entitlements/mcstore";

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct XboxTokenResponse {
    #[serde(rename = "Token")]
    token: String,
    #[serde(rename = "DisplayClaims")]
    display_claims: XboxDisplayClaims,
}

#[derive(Debug, Deserialize)]
struct XboxDisplayClaims {
    xui: Vec<XboxUserInfo>,
}

#[derive(Debug, Deserialize)]
struct XboxUserInfo {
    uhs: String,
}

#[derive(Debug, Deserialize)]
struct MinecraftLoginResponse {
    access_token: String,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct MinecraftProfile {
    id: String,
    name: String,
}

pub fn device_login(
    client_id: &str,
    tx: &Sender<WorkerEvent>,
    cancel_rx: Receiver<()>,
) -> Result<Account> {
    let client = Client::builder()
        .user_agent("GlideClientLauncher/0.2")
        .build()
        .context("failed to build HTTP client")?;

    let device = client
        .post(DEVICE_CODE_URL)
        .form(&[
            ("client_id", client_id),
            ("scope", MSA_SCOPE),
            ("response_type", "device_code"),
        ])
        .send()
        .context("failed to request Microsoft device code")?
        .error_for_status()
        .context("Microsoft rejected the device code request")?
        .json::<DeviceCodeResponse>()
        .context("failed to parse Microsoft device code response")?;

    let message = device.message.clone().unwrap_or_else(|| {
        format!(
            "Open {} and enter code {}.",
            device.verification_uri, device.user_code
        )
    });

    let _ = open::that(&device.verification_uri);
    let _ = tx.send(WorkerEvent::DeviceCode {
        verification_uri: device.verification_uri.clone(),
        user_code: device.user_code.clone(),
        message,
    });

    let interval = Duration::from_secs(device.interval.unwrap_or(5).max(2));
    let attempts = device.expires_in / interval.as_secs().max(1);
    let token = poll_device_token(
        &client,
        client_id,
        &device.device_code,
        interval,
        attempts,
        &cancel_rx,
    )?;
    let microsoft_access_token = token
        .access_token
        .ok_or_else(|| anyhow!("Microsoft access token was missing"))?;
    let refresh_token = token
        .refresh_token
        .ok_or_else(|| anyhow!("Microsoft refresh token was missing"))?;

    complete_minecraft_login(&client, microsoft_access_token, refresh_token, tx)
}

pub fn refresh_account(
    client_id: &str,
    account: &Account,
    tx: &Sender<WorkerEvent>,
) -> Result<Account> {
    if account.is_fresh() {
        return Ok(account.clone());
    }

    let _ = tx.send(WorkerEvent::Log(
        "Refreshing Microsoft session...".to_owned(),
    ));

    let client = Client::builder()
        .user_agent("GlideClientLauncher/0.2")
        .build()
        .context("failed to build HTTP client")?;

    let token = client
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", account.refresh_token()),
            ("scope", MSA_SCOPE),
        ])
        .send()
        .context("failed to refresh Microsoft token")?
        .json::<TokenResponse>()
        .context("failed to parse refreshed Microsoft token")?;

    if let Some(error) = token.error {
        bail!(
            "Microsoft token refresh failed: {} {}",
            error,
            token.error_description.unwrap_or_default()
        );
    }

    let microsoft_access_token = token
        .access_token
        .ok_or_else(|| anyhow!("refreshed Microsoft access token was missing"))?;
    let refresh_token = token
        .refresh_token
        .unwrap_or_else(|| account.refresh_token().to_owned());

    complete_minecraft_login(&client, microsoft_access_token, refresh_token, tx)
}

fn poll_device_token(
    client: &Client,
    client_id: &str,
    device_code: &str,
    mut interval: Duration,
    attempts: u64,
    cancel_rx: &Receiver<()>,
) -> Result<TokenResponse> {
    for _ in 0..attempts.max(1) {
        wait_with_cancel(interval, cancel_rx)?;
        let token = client
            .post(TOKEN_URL)
            .form(&[
                ("client_id", client_id),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
            ])
            .send()
            .context("failed while polling Microsoft login")?
            .json::<TokenResponse>()
            .context("failed to parse Microsoft token polling response")?;

        match token.error.as_deref() {
            None => return Ok(token),
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += Duration::from_secs(5);
                continue;
            }
            Some("authorization_declined") => bail!("Microsoft login was cancelled"),
            Some("expired_token") => bail!("Microsoft login code expired"),
            Some(error) => {
                let description = token.error_description.unwrap_or_default();
                bail!("Microsoft login failed: {error} {description}");
            }
        }
    }

    bail!("Microsoft login did not finish before the code expired")
}

fn wait_with_cancel(duration: Duration, cancel_rx: &Receiver<()>) -> Result<()> {
    let mut remaining_ms = duration.as_millis().min(u128::from(u64::MAX)) as u64;
    while remaining_ms > 0 {
        match cancel_rx.try_recv() {
            Ok(_) | Err(TryRecvError::Disconnected) => bail!("Microsoft login was cancelled"),
            Err(TryRecvError::Empty) => {}
        }

        let chunk_ms = remaining_ms.min(250);
        thread::sleep(Duration::from_millis(chunk_ms));
        remaining_ms -= chunk_ms;
    }
    Ok(())
}

fn complete_minecraft_login(
    client: &Client,
    microsoft_access_token: String,
    refresh_token: String,
    tx: &Sender<WorkerEvent>,
) -> Result<Account> {
    let xbl = client
        .post(XBL_AUTH_URL)
        .header("Accept", "application/json")
        .json(&json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": microsoft_access_token
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }))
        .send()
        .context("failed to authenticate with Xbox Live")?
        .error_for_status()
        .context("Xbox Live authentication failed")?
        .json::<XboxTokenResponse>()
        .context("failed to parse Xbox Live response")?;

    let user_hash = xbl
        .display_claims
        .xui
        .first()
        .ok_or_else(|| anyhow!("Xbox user hash was missing"))?
        .uhs
        .clone();

    let xsts = client
        .post(XSTS_AUTH_URL)
        .header("Accept", "application/json")
        .json(&json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [xbl.token]
            },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        }))
        .send()
        .context("failed to authorize with XSTS")?
        .error_for_status()
        .context("XSTS authorization failed. Xbox privacy settings or account age may block Minecraft login.")?
        .json::<XboxTokenResponse>()
        .context("failed to parse XSTS response")?;

    let mc_login = client
        .post(MC_LOGIN_URL)
        .header("Accept", "application/json")
        .json(&json!({
            "identityToken": format!("XBL3.0 x={};{}", user_hash, xsts.token)
        }))
        .send()
        .context("failed to login to Minecraft services")?
        .error_for_status()
        .context("Minecraft services rejected the Xbox token")?
        .json::<MinecraftLoginResponse>()
        .context("failed to parse Minecraft login response")?;

    let _ = client
        .get(MC_ENTITLEMENTS_URL)
        .bearer_auth(&mc_login.access_token)
        .send()
        .context("failed to check Minecraft ownership")?
        .error_for_status()
        .context("Minecraft ownership check failed")?;

    let profile = client
        .get(MC_PROFILE_URL)
        .bearer_auth(&mc_login.access_token)
        .send()
        .context("failed to request Minecraft profile")?
        .error_for_status()
        .context(
            "Minecraft profile was not available. This account may not own Minecraft Java Edition.",
        )?
        .json::<MinecraftProfile>()
        .context("failed to parse Minecraft profile")?;

    let _ = tx.send(WorkerEvent::Log(format!("Logged in as {}.", profile.name)));

    Ok(Account::Microsoft {
        username: profile.name,
        uuid: profile.id,
        access_token: mc_login.access_token,
        refresh_token,
        expires_at: now_unix() + mc_login.expires_in.saturating_sub(60),
    })
}
