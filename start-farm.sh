#!/usr/bin/env bash
# Запуск оркестраторов в nohup: логи 502.log, 503.log, ... в этой папке. Старты со сдвигом (по умолчанию 90 с).
# Использование: ./start-farm.sh            — все анархии
#                ./start-farm.sh 502 507    — только перечисленные
cd "$(dirname "$0")" || exit 1
ANS=("$@"); [ ${#ANS[@]} -eq 0 ] && ANS=(502 503 504 506 507 508 509 510)
GAP="${GAP:-90}"
for a in "${ANS[@]}"; do
  if pgrep -f "orch502 $a\$" >/dev/null; then echo "$a уже запущен"; continue; fi
  # без прокси не запускаем: бот пошёл бы напрямую с IP сервера (510 ждёт прокси)
  if grep -q '"proxy": ""' "bots/${a}b.json" 2>/dev/null; then echo "$a: у бота нет прокси — пропуск"; continue; fi
  # пустая анархия (все ники в питомнике) — оркестратор не нужен
  if python3 -c "import json,sys; sys.exit(0 if json.load(open('bots/${a}b.json')) else 1)" 2>/dev/null; then :; else echo "$a: ботов нет — пропуск"; continue; fi
  EXTRA=()
  # быструю покупку на всех анархиях включали 20.11 ~15:00 МСК и откатили в ~22:00: через 3,5 ч забанили старого
  # korapuxazag (503, 9 сут) — первый «AB» у старого бота с 14.11. Эксперимент на 508 тоже закрыт (ниже).
  # эксперимент на 508 (4 строки + быстрый темп) закрыт 21.11: после бана pexunilav 508 вернули к обычной покупке (2 строки, обычный темп)
  nohup "${EXTRA[@]}" ./target/release/orch502 "$a" >> "$a.log" 2>&1 &
  echo "$a запущен (pid $!), лог $a.log"
  sleep "$GAP"
done
