# 4narek-azalea

Rust-порт орка + воркера на **Azalea** (MC 1.21.11). Своя ферма — **не** зависит от `4narek-1.12`.

## Конфиг

| Файл | Что |
|---|---|
| `orch.json` | анка, `farm_root` (по умолчанию `.`), Go WS, local_mode |
| `{farm}/bots/{an}b.json` | список ботов; прокси — поле `proxy` (`socks5://user:pass@host:port`) у каждого бота |
| `{farm}/ip.json` | запасной вариант: слоты (`502-1`…) для ботов с полем `ip` вместо `proxy`; овнеры им не пользуются |
| `{farm}/clan-owners.json` | овнер анки |
| `{farm}/owner-ip.json` | прокси овнера (для clan-setup) |

## Запуск

```bash
cd 4narek-azalea
npm install
cargo build -p bot -p orch
./target/debug/orch504
```
nohup ./target/release/bot502 > bot502.log 2>&1 &
nohup ./target/release/bot504 > bot504.log 2>&1 &

`local_mode: true` — овнер (`scripts/clan-setup.mjs` + `lib/`) поднимается сам при `clan_needed` / Go `run_clan_setup`.
Овнер читает `clan-owners.json` + `owner-ip.json` из `farm_root`.
