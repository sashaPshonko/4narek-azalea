//! «Майник»: когда у казначея накопилось много лишних денег, на анархию заходит аккаунт владельца (myNick),
//! ждёт перевод от казначея и выходит. Сам заход делает скрипт `bot-view/mynick.mjs` (Node), а оркестратор только
//! решает, когда его запускать. Казначей, увидев myNick в табе, сам переводит ему остаток (`my_nick_seen`
//! в `bot/src/main.rs`).
//!
//! Конфиг: `{farm_root}/mynick.json` (читается на каждом тике, перезапуск оркестратора не нужен):
//! `enabled`, `username`, `password`, `socks`, `host`, `port`, `version`, `threshold` (лишнее сверх KEEP_SUM, по
//! умолчанию 40 000 000), `cooldown_min`, `wait_min`, `after_pay_sec`, `script`, `node`.
//!
//! Один аккаунт на все анархии: перед запуском берётся общий файл-замок `{farm_root}/mynick.lock`
//! (анархия + время), поэтому оркестраторы разных анархий не запустят его одновременно.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use tracing::{info, warn};

/// Сколько фермер оставляет себе (как `KEEP_SUM` в боте): лишнее считается сверх этого.
pub const KEEP_SUM_BASE: f64 = 15_000_000.0;
pub fn keep_sum() -> f64 { ipc::balance::k(KEEP_SUM_BASE) }
/// Баланс казначея старше этого считаем устаревшим — не запускаем.
pub const BALANCE_FRESH: Duration = Duration::from_secs(5 * 60);

fn d_threshold() -> f64 { 40_000_000.0 }
fn d_cooldown() -> u64 { 20 }
fn d_wait() -> u64 { 4 }
fn d_script() -> String { "../bot-view/mynick.mjs".into() }
fn d_node() -> String { "node".into() }
fn d_em_threshold() -> f64 { 5_000_000.0 }
fn d_em_cooldown() -> u64 { 5 }
fn d_em_window() -> u64 { 45 }
fn d_engine() -> String { "rust".into() }
fn d_chat() -> String { "!всем привет".into() }

#[derive(Debug, Clone, Deserialize)]
pub struct MyNickCfg {
    #[serde(default)]
    pub enabled: bool,
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub socks: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default = "d_threshold")]
    pub threshold: f64,
    #[serde(default = "d_cooldown")]
    pub cooldown_min: u64,
    #[serde(default = "d_wait")]
    pub wait_min: u64,
    #[serde(default)]
    pub after_pay_sec: Option<u64>,
    #[serde(default = "d_script")]
    pub script: String,
    #[serde(default = "d_node")]
    pub node: String,
    /// Аварийный режим: после бана (любого бота фермы) в течение `emergency_window_min` порог лишних денег
    /// снижается до `emergency_threshold`, а пауза между заходами — до `emergency_cooldown_min`.
    #[serde(default = "d_em_threshold")]
    pub emergency_threshold: f64,
    #[serde(default = "d_em_cooldown")]
    pub emergency_cooldown_min: u64,
    #[serde(default = "d_em_window")]
    pub emergency_window_min: u64,
    /// `rust` (бинарь бота, режим MYNICK_MODE) или `node` (старый скрипт mineflayer).
    #[serde(default = "d_engine")]
    pub engine: String,
    /// Что писать в общий чат после захода (с «!»): казначеи реагируют на упоминание myNick в чате.
    #[serde(default = "d_chat")]
    pub chat: String,
}

pub fn cfg_path(farm_root: &Path) -> PathBuf {
    farm_root.join("mynick.json")
}

pub fn load(farm_root: &Path) -> Option<MyNickCfg> {
    let raw = std::fs::read_to_string(cfg_path(farm_root)).ok()?;
    match serde_json::from_str::<MyNickCfg>(&raw) {
        Ok(c) if c.enabled => Some(c),
        Ok(_) => None,
        Err(e) => {
            warn!("mynick.json: {e}");
            None
        }
    }
}

/// Пора ли звать майника: лишних денег у казначея хватает.
pub fn excess_enough(balance: f64, threshold: f64) -> bool {
    balance >= 0.0 && balance - keep_sum() >= threshold
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn lock_path(farm_root: &Path) -> PathBuf {
    farm_root.join("mynick.lock")
}

/// Замок свободен, если файла нет или он старше `cooldown_min` (после захода держим паузу, чтобы не ходить подряд).
pub fn lock_free(farm_root: &Path, cooldown_min: u64) -> bool {
    let Ok(raw) = std::fs::read_to_string(lock_path(farm_root)) else {
        return true;
    };
    let ts: u64 = raw.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    now_secs().saturating_sub(ts) >= cooldown_min * 60
}

fn ban_wave_path(farm_root: &Path) -> PathBuf {
    farm_root.join("bots").join("ban-wave.txt")
}

/// Запомнить бан (время, секунды). Файл общий для всех оркестраторов фермы.
pub fn record_ban(farm_root: &Path) {
    let _ = std::fs::write(ban_wave_path(farm_root), format!("{}\n", now_secs()));
}

/// Был ли бан в последние `window_min` минут (по любой анархии).
pub fn emergency_active(farm_root: &Path, window_min: u64) -> bool {
    let Ok(raw) = std::fs::read_to_string(ban_wave_path(farm_root)) else {
        return false;
    };
    let ts: u64 = raw.trim().parse().unwrap_or(0);
    ts > 0 && now_secs().saturating_sub(ts) < window_min * 60
}

/// Запуск скрипта. Замок пишется до запуска, чтобы другой оркестратор не успел встать рядом.
pub fn spawn(farm_root: &Path, anarchy: u16, cfg: &MyNickCfg, bot_bin: &Path) -> anyhow::Result<()> {
    std::fs::write(lock_path(farm_root), format!("{anarchy} {}\n", now_secs()))?;
    if cfg.engine == "rust" {
        return spawn_rust(farm_root, anarchy, cfg, bot_bin);
    }
    // конфиг для скрипта: тот же mynick.json (поля host/port/version необязательны, дефолты в скрипте/библиотеке)
    let script = if Path::new(&cfg.script).is_absolute() {
        PathBuf::from(&cfg.script)
    } else {
        farm_root.join(&cfg.script)
    };
    let log_path = farm_root.join("mynick.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let log_err = log.try_clone()?;
    let mut cmd = tokio::process::Command::new(&cfg.node);
    cmd.arg(&script)
        .arg(anarchy.to_string())
        .arg(cfg_path(farm_root))
        .current_dir(script.parent().unwrap_or(farm_root))
        .stdout(log)
        .stderr(log_err)
        .kill_on_drop(false);
    let mut child = cmd.spawn()?;
    info!("mynick: запущен для an{anarchy} (pid {:?}), лог {}", child.id(), log_path.display());
    // фоновое ожидание, чтобы не плодить зомби и записать итог
    tokio::spawn(async move {
        match child.wait().await {
            Ok(st) => info!("mynick: завершён, код {:?}", st.code()),
            Err(e) => warn!("mynick: wait: {e}"),
        }
    });
    Ok(())
}

/// Заход через бинарь бота (azalea): `MYNICK_MODE=1`, BotBoot отдаём в stdin, как обычному воркеру.
fn spawn_rust(farm_root: &Path, anarchy: u16, cfg: &MyNickCfg, bot_bin: &Path) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let boot = ipc::BotBoot {
        username: cfg.username.clone(),
        password: cfg.password.clone(),
        anarchy,
        item: "netherite".into(),
        go_type: "mynick".into(),
        proxy: cfg.socks.clone(),
        server: cfg.host.clone().unwrap_or_else(|| "mc.funtime.su".into()),
        my_nick: None,
        owner_nick: None,
        owner_reason: None,
        treasurer_nick: None,
        warmup: false,
    };
    let log_path = farm_root.join("mynick.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let log_err = log.try_clone()?;
    let mut cmd = tokio::process::Command::new(bot_bin);
    cmd.env("MYNICK_MODE", "1")
        .env("MYNICK_WAIT_SEC", (cfg.wait_min * 60).to_string())
        .env("MYNICK_AFTER_PAY_SEC", cfg.after_pay_sec.unwrap_or(8).to_string())
        .env("MYNICK_CHAT", &cfg.chat)
        .current_dir(farm_root)
        .stdin(std::process::Stdio::piped())
        .stdout(log)
        .stderr(log_err)
        .kill_on_drop(false);
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take();
    let line = serde_json::to_string(&boot)? + "\n";
    info!("mynick(rust): запущен для an{anarchy} (pid {:?}), лог {}", child.id(), log_path.display());
    tokio::spawn(async move {
        if let Some(si) = stdin.as_mut() {
            let _ = si.write_all(line.as_bytes()).await;
            let _ = si.flush().await;
        }
        // stdin держим открытым до конца процесса (EOF воркер может принять за команду на выход)
        match child.wait().await {
            Ok(st) => info!("mynick: завершён, код {:?}", st.code()),
            Err(e) => warn!("mynick: wait: {e}"),
        }
        drop(stdin);
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_window() {
        let dir = std::env::temp_dir().join(format!("mn-em-{}", now_secs()));
        std::fs::create_dir_all(dir.join("bots")).unwrap();
        assert!(!emergency_active(&dir, 45));
        record_ban(&dir);
        assert!(emergency_active(&dir, 45));
        assert!(!emergency_active(&dir, 0));
    }

    #[test]
    fn excess_over_threshold() {
        assert!(excess_enough(55_000_000.0, 40_000_000.0));
        assert!(!excess_enough(54_000_000.0, 40_000_000.0));
        assert!(!excess_enough(-1.0, 40_000_000.0)); // баланс неизвестен
    }
}
