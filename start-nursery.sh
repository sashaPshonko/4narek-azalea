#!/usr/bin/env bash
# Питомник: прогрев новых ников без АХ (target/release/nursery502). Лог nursery.log в этой папке.
# Ники — bots/nursery.json, сводка — nursery-status.json. Прогрев: WARMUP_DAYS (по умолчанию 3).
cd "$(dirname "$0")" || exit 1
if pgrep -f "^./target/release/nursery502$" >/dev/null; then echo "питомник уже запущен"; exit 0; fi
nohup ./target/release/nursery502 >> nursery.log 2>&1 < /dev/null &
echo "питомник запущен (pid $!), лог nursery.log"
