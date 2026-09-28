#!/bin/zsh
set -eu
cd -- "$(dirname -- "$0")"
base=http://127.0.0.1:18794
paper_health() {
  /usr/bin/curl -fsS --noproxy '*' --max-time 2 "$base/health" 2>/dev/null || true
}
ready() {
  [[ "$1" == *'"application":"paired-paper"'* && "$1" == *'"markets":["openai","anth"]'* && "$1" == *'"live_orders":false'* ]]
}
health=$(paper_health)
if ready "$health"; then /usr/bin/open "$base/"; exit 0; fi
if [[ -n "$health" ]]; then
  print '18794 已有其他程序。请先核对原程序，不会再启动第二个进程。'
  exit 1
fi
nohup ./Cross-Platform-Arbitrage-Paper --data-dir data --start >模拟日志.log 2>&1 </dev/null &
sim_pid=$!
for attempt in {1..30}; do
  if ! kill -0 "$sim_pid" 2>/dev/null; then
    print '模拟程序启动失败，请查看本目录的模拟日志.log。'
    exit 1
  fi
  health=$(paper_health)
  if ready "$health"; then /usr/bin/open "$base/"; exit 0; fi
  sleep 1
done
print '公开行情连接尚未就绪，请查看模拟日志.log；不要重复启动。'
