use crate::config::Account;

#[derive(Debug, Clone)]
pub enum WorkerEvent {
    Log(String),
    Progress {
        label: String,
        current: u64,
        total: u64,
    },
    DeviceCode {
        verification_uri: String,
        user_code: String,
        message: String,
    },
    Authenticated(Account),
    AccountUpdated(Account),
    LaunchStarted(u32),
    Finished(String),
    Failed(String),
}
