use std::sync::Arc;

use ipc::{BotBoot, OrchCmd};
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::warn;

use crate::captcha::CaptchaMaps;
use crate::state::BotState;

pub async fn read_boot() -> anyhow::Result<BotBoot> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("stdin EOF — нет BotBoot"))?;
    let boot: BotBoot = serde_json::from_str(&line)?;
    // stash for Azalea Default state
    let _ = GLOBAL.set(Global {
        boot: Arc::new(boot.clone()),
        state: Arc::new(Mutex::new(BotState::from_boot(&boot))),
        captcha_maps: Arc::new(Mutex::new(CaptchaMaps::default())),
    });
    Ok(boot)
}

pub struct Global {
    pub boot: Arc<BotBoot>,
    pub state: Arc<Mutex<BotState>>,
    pub captcha_maps: Arc<Mutex<CaptchaMaps>>,
}

static GLOBAL: std::sync::OnceLock<Global> = std::sync::OnceLock::new();

pub fn global() -> &'static Global {
    GLOBAL.get().expect("boot not read")
}

pub async fn stdin_loop(shared: Arc<Mutex<BotState>>) {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<OrchCmd>(&line) {
            Ok(cmd) => {
                tracing::debug!("orch cmd");
                shared.lock().apply_cmd(cmd);
            }
            Err(e) => warn!("stdin parse: {e} — {line}"),
        }
    }
}
