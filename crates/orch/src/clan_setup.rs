//! Запуск `scripts/clan-setup.mjs` (овнер: create/invite/права) — как 502b.

use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{mpsc, Mutex},
};
use tracing::{error, info, warn};

type GoTx = mpsc::UnboundedSender<Value>;

const COOLDOWN: Duration = Duration::from_secs(90);

struct Slot {
    started_at: Instant,
    /// child ещё жив
    pid: Option<u32>,
}

pub struct ClanSetupGate {
    by_an: Mutex<HashMap<u16, Slot>>,
    farm_root: PathBuf,
    local_mode: bool,
}

impl ClanSetupGate {
    pub fn new(farm_root: PathBuf, local_mode: bool) -> Arc<Self> {
        Arc::new(Self {
            by_an: Mutex::new(HashMap::new()),
            farm_root,
            local_mode,
        })
    }

    pub fn local_mode(&self) -> bool {
        self.local_mode
    }

    /// `force` — без cooldown (пришло от Go `run_clan_setup`).
    pub async fn request(
        self: &Arc<Self>,
        go_tx: &GoTx,
        anarchy: u16,
        reason: &str,
        trigger: &str,
        force: bool,
    ) -> bool {
        if anarchy == 0 {
            warn!("clan-setup: bad anarchy");
            return false;
        }
        let script = self.farm_root.join("scripts/clan-setup.mjs");
        if !script.is_file() {
            error!("clan-setup: нет {}", script.display());
            return false;
        }

        {
            let mut map = self.by_an.lock().await;
            if let Some(slot) = map.get(&anarchy) {
                if slot.pid.is_some() {
                    info!(
                        "clan-setup an{anarchy} уже запущен (триггер {trigger}, reason={reason})"
                    );
                    return false;
                }
                if !force && slot.started_at.elapsed() < COOLDOWN {
                    let left = (COOLDOWN - slot.started_at.elapsed()).as_secs();
                    info!("clan-setup an{anarchy} cooldown ещё {left}с (триггер {trigger})");
                    return false;
                }
            }
            map.insert(
                anarchy,
                Slot {
                    started_at: Instant::now(),
                    pid: Some(0), // placeholder until spawn
                },
            );
        }

        info!("clan-setup start an{anarchy} reason={reason} trigger={trigger}");

        let mut cmd = Command::new("node");
        cmd.arg(&script)
            .arg(anarchy.to_string())
            .current_dir(&self.farm_root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // launch-gate / Go HTTP: локально как LOCAL_MODE у JS-орка
        if self.local_mode {
            cmd.env("LOCAL_MODE", "1");
            cmd.env("FLEET_LAUNCH_BYPASS", "1");
            cmd.env("GO_HTTP_URL", "http://127.0.0.1:8080");
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                error!("clan-setup spawn: {e}");
                self.by_an.lock().await.insert(
                    anarchy,
                    Slot {
                        started_at: Instant::now(),
                        pid: None,
                    },
                );
                report_result(go_tx, anarchy, false, false, &e.to_string());
                return false;
            }
        };

        let pid = child.id();
        self.by_an.lock().await.insert(
            anarchy,
            Slot {
                started_at: Instant::now(),
                pid,
            },
        );

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let gate = Arc::clone(self);
        let go = go_tx.clone();
        tokio::spawn(async move {
            let banned = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let b1 = Arc::clone(&banned);
            let b2 = Arc::clone(&banned);
            let t1 = tokio::spawn(async move {
                pipe_clan_log(stdout, anarchy, b1).await;
            });
            let t2 = tokio::spawn(async move {
                pipe_clan_log(stderr, anarchy, b2).await;
            });
            let status = child.wait().await;
            let _ = t1.await;
            let _ = t2.await;
            let code = status.as_ref().ok().and_then(|s| s.code());
            let banned_seen = banned.load(std::sync::atomic::Ordering::SeqCst) || code == Some(2);
            let ok = code == Some(0);
            info!(
                "[clan-setup an{anarchy}] exit code={code:?} ok={ok} banned={banned_seen}"
            );
            gate.by_an.lock().await.insert(
                anarchy,
                Slot {
                    started_at: Instant::now(),
                    pid: None,
                },
            );
            report_result(
                &go,
                anarchy,
                ok,
                banned_seen,
                &format!("code={code:?}"),
            );
        });

        true
    }
}

async fn pipe_clan_log<R>(
    stream: Option<R>,
    an: u16,
    banned_flag: Arc<std::sync::atomic::AtomicBool>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let Some(stream) = stream else {
        return;
    };
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let bare = line
            .strip_prefix("[clan-setup] ")
            .unwrap_or(line.as_str());
        info!("[clan-setup an{an}] {bare}");
        if bare.contains("BAN — stop")
            || bare.contains("ВЫ ЗАБАНЕНЫ")
            || bare.contains("Пункт 4.3")
        {
            banned_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn report_result(go_tx: &GoTx, anarchy: u16, ok: bool, banned: bool, detail: &str) {
    let _ = go_tx.send(json!({
        "action": "clan_setup_result",
        "anarchy": anarchy,
        "ok": ok,
        "banned": banned,
        "detail": detail.chars().take(200).collect::<String>(),
    }));
}
