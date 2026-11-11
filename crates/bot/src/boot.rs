use std::sync::Arc;

use azalea::prelude::*;
use ipc::{BotBoot, OrchCmd};
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::captcha::CaptchaMaps;
use crate::owner::PayRequest;
use crate::state::BotState;

/// **Устарело** — owner нет. Оставлено для совместимости.
#[derive(Debug, Clone)]
pub struct BalancesSnapshotReq {
    pub balances: Vec<(String, f64)>,
}

pub async fn read_boot() -> anyhow::Result<BotBoot> {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("stdin EOF — нет BotBoot"))?;
    let boot: BotBoot = serde_json::from_str(&line)?;

    let (pay_tx, pay_rx) = mpsc::unbounded_channel::<PayRequest>();
    let (my_nick_tx, my_nick_rx) = mpsc::unbounded_channel::<String>();
    let (balances_tx, balances_rx) = mpsc::unbounded_channel::<BalancesSnapshotReq>();

    let _ = GLOBAL.set(Global {
        boot: Arc::new(boot.clone()),
        state: Arc::new(Mutex::new(BotState::from_boot(&boot))),
        captcha_maps: Arc::new(Mutex::new(CaptchaMaps::default())),
        bot_client: Arc::new(Mutex::new(None)),
        pay_tx: Arc::new(Mutex::new(Some(pay_tx))),
        pay_rx: Arc::new(Mutex::new(Some(pay_rx))),
        my_nick_tx: Arc::new(Mutex::new(Some(my_nick_tx))),
        my_nick_rx: Arc::new(Mutex::new(Some(my_nick_rx))),
        balances_tx: Arc::new(Mutex::new(Some(balances_tx))),
        balances_rx: Arc::new(Mutex::new(Some(balances_rx))),
    });
    Ok(boot)
}

pub struct Global {
    pub boot: Arc<BotBoot>,
    pub state: Arc<Mutex<BotState>>,
    pub captcha_maps: Arc<Mutex<CaptchaMaps>>,

    /// `Client` бота. Заполняется в `handle(Event::Spawn)` в `main.rs`.
    /// Нужен для `flush_treasurer_loop` из тикера в `main.rs`.
    pub bot_client: Arc<Mutex<Option<Client>>>,

    /// **Устарело** — owner нет. Оставлено для совместимости.
    pub pay_tx: Arc<Mutex<Option<mpsc::UnboundedSender<PayRequest>>>>,
    pub pay_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<PayRequest>>>>,

    /// **Устарело** — owner нет. Оставлено для совместимости.
    pub my_nick_tx: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    pub my_nick_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,

    /// **Устарело** — owner нет. Снимок пишется напрямую в
    /// `state.balances_snapshot`, канал не читается.
    pub balances_tx: Arc<Mutex<Option<mpsc::UnboundedSender<BalancesSnapshotReq>>>>,
    pub balances_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<BalancesSnapshotReq>>>>,
}

static GLOBAL: std::sync::OnceLock<Global> = std::sync::OnceLock::new();

pub fn global() -> &'static Global {
    GLOBAL.get().expect("boot not read")
}

pub async fn stdin_loop(
    shared: Arc<Mutex<BotState>>,
    pay_tx: Arc<Mutex<Option<mpsc::UnboundedSender<PayRequest>>>>,
    balances_tx: Arc<Mutex<Option<mpsc::UnboundedSender<BalancesSnapshotReq>>>>,
) {
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

                // Pay → owner-боту. **Устарело** — owner нет.
                if let OrchCmd::Pay { to, amount, req_id } = &cmd {
                    let tx = pay_tx.lock();
                    if let Some(tx) = tx.as_ref() {
                        if let Err(e) = tx.send(PayRequest {
                            to: to.clone(),
                            amount: *amount,
                            req_id: *req_id,
                        }) {
                            warn!("Pay: send в канал: {e}");
                        }
                    }
                    continue;
                }

                // BalancesSnapshot: пишем **напрямую** в state
                // (`state.balances_snapshot`) — его читает `sell_pass`.
                if let OrchCmd::BalancesSnapshot { balances } = &cmd {
                    shared.lock().balances_snapshot = balances.clone();
                    // Дублируем в канал (на будущее).
                    let tx = balances_tx.lock();
                    if let Some(tx) = tx.as_ref() {
                        let _ = tx.send(BalancesSnapshotReq {
                            balances: balances.clone(),
                        });
                    }
                    continue;
                }

                // TreasurerRole: смена казначея. `apply_cmd` обновит
                // `is_treasurer` / `treasurer_nick`.
                if let OrchCmd::TreasurerRole { username } = &cmd {
                    let (treasurer_was, i_am_new) = {
                        let st = shared.lock();
                        (
                            st.treasurer_nick.clone(),
                            username.eq_ignore_ascii_case(&st.username),
                        )
                    };
                    shared.lock().apply_cmd(cmd);
                    continue;
                }

                // FlushToTreasurer: **только** ставим `treasurer_flush_to`.
                // Запуск `flush_treasurer_loop` — в `main.rs` (тикер раз в 2с).
                // Так `stdin_loop` не зависит от `Client`, который ещё
                // может быть не готов на момент прихода команды.
                if let OrchCmd::FlushToTreasurer { to } = &cmd {
                    info!("FlushToTreasurer → {to} (flush запустит main.rs тикер)");
                    shared.lock().apply_cmd(cmd);
                    continue;
                }

                // Остальные OrchCmd — напрямую в state.
                shared.lock().apply_cmd(cmd);
            }
            Err(e) => warn!("stdin parse: {e} — {line}"),
        }
    }
}