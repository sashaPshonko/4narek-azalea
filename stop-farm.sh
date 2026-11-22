#!/usr/bin/env bash
# Остановка оркестраторов (боты-воркеры завершатся вместе с ними). ./stop-farm.sh [502 ...]
cd "$(dirname "$0")" || exit 1
if [ $# -eq 0 ]; then pkill -f "target/release/orch502" ; pkill -f "target/release/bot502"; else for a in "$@"; do pkill -f "orch502 $a\$"; done; fi
echo done
