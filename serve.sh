#!/usr/bin/env bash

set -euo pipefail

PORT=3000
if ! command -v lsof >/dev/null 2>&1; then
  echo "포트 사용 여부 확인에 필요한 lsof를 찾을 수 없습니다." >&2
  exit 1
fi
PID=$(lsof -t -i:"$PORT" 2>/dev/null || true)
if [ -n "$PID" ]; then
  echo "포트 $PORT 이(가) 이미 사용 중입니다(PID $PID). 기존 프로세스는 종료하지 않습니다." >&2
  exit 1
fi
echo "http://localhost:$PORT 서버 시작"
npx -y serve -s "$(dirname "$0")" -l "$PORT"
