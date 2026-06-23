# Schedule Session Clone Goal

## 한 줄 목표

스케줄 작업은 등록 당시의 대화 맥락을 최대한 그대로 사용할 수 있어야 하지만, 원본 대화 세션을 직접 변경해서는 안 된다.

이번 작업의 목표는 기존 `context_summary` 기반 스케줄 실행 방식을 제거하고, 스케줄 실행 시점마다 원본 provider 세션을 복제하거나 fork한 뒤 그 복제 세션에서 작업을 수행하도록 바꾸는 것이다.

## 왜 이 작업이 필요했나

기존 스케줄 구현은 대략 다음 방식이었다.

1. 사용자가 대화 중 `--cron`, `--at` 등으로 스케줄을 등록한다.
2. 등록 직후 별도 백그라운드 프로세스가 실행된다.
3. 그 프로세스는 기존 세션을 읽고, 앞으로 실행할 스케줄 작업에 필요한 맥락을 짧은 `context_summary` 텍스트로 요약한다.
4. 요약 결과를 `~/.cokacdir/schedule/<ID>.json`의 `context_summary` 필드에 저장한다.
5. 스케줄 실행 시점이 오면 별도 workspace 또는 별도 세션에서 prompt와 `context_summary`를 함께 사용해 작업을 시작한다.
6. 반복 cron의 경우 실행 결과를 다시 요약해 다음 실행에 넘기는 구조를 일부 갖고 있었다.

이 방식은 기능적으로는 동작할 수 있지만, 스케줄의 본래 요구와 맞지 않는 문제가 있었다.

## 기존 summary 방식의 문제

### 1. 요약은 원본 세션이 아니다

스케줄이 원하는 것은 "지금 하던 일을 나중에 이어서 해줘"에 가깝다.

하지만 `context_summary`는 원본 세션 전체가 아니라 모델이 한 번 해석해서 줄인 텍스트다. 이 과정에서 다음 정보가 손실될 수 있다.

- 이전 대화의 세부 지시
- 사용자가 싫어하거나 금지한 방식
- 코드 수정 과정의 맥락
- provider가 내부적으로 가진 tool 결과
- 파일 경로, 세션 상태, 진행 중 판단
- 사용자의 말투나 의도
- "중요해 보이지 않지만 다음 행동에는 필요한" 정보

요약이 아무리 좋아도 원본 대화와 같을 수는 없다. 따라서 스케줄 실행의 기본 자료로 summary를 쓰는 것은 구조적으로 취약하다.

### 2. 요약 대상 선정이 휴리스틱해진다

요약을 하려면 "어느 정도까지 읽을 것인가"를 결정해야 한다.

세션이 길면 전체를 그대로 넘기기 어렵고, 짧게 자르면 어느 지점부터 잘라야 하는지 기준이 필요하다. 이 기준은 결국 길이, 최근 메시지 수, 문자 수 같은 휴리스틱에 의존하게 된다.

이번 작업의 방향은 이런 판단 자체를 없애는 것이다. 원본 세션을 복제할 수 있다면 요약할 범위를 판단할 필요가 없다.

### 3. 백그라운드 요약은 race condition을 만든다

기존 방식은 스케줄 등록 후 `--cron-context` 백그라운드 프로세스를 띄워 요약을 만들었다.

이 구조에서는 다음 문제가 생긴다.

- 스케줄 실행 시점이 요약 완료보다 먼저 올 수 있다.
- 요약이 실패해도 등록 자체는 성공한 상태가 된다.
- 요약 중 사용자가 스케줄을 삭제하면, 요약 프로세스가 뒤늦게 파일을 다시 쓸 위험이 있다.
- `context_summary_pending` 같은 별도 상태 플래그가 필요해진다.
- 요약을 담당하는 provider가 실제 실행 provider와 달라질 수 있다.

즉 스케줄 등록, 요약 완료, 스케줄 실행 사이에 상태 동기화 문제가 생긴다.

세션 복제 방식은 등록 시점에 요약 작업을 하지 않으므로 이 문제를 제거한다.

### 4. Claude 의존성이 생긴다

기존 흐름의 일부는 Claude를 통해 context summary를 만들었다.

하지만 스케줄 실행 provider가 Codex, OpenCode, Agy일 수 있다면 Claude가 요약을 담당하는 것은 자연스럽지 않다. provider별 세션 구조와 의미가 다르고, Claude가 다른 provider의 세션을 정확히 이해한다고 볼 수 없다.

이번 작업의 목표는 summary provider를 고르는 것이 아니라 summary 자체를 없애는 것이다.

### 5. 별도 workspace 방식은 사용자의 기대와 다르다

기존 기본 모드는 `~/.cokacdir/workspace/<schedule_id>/` 같은 별도 workspace를 만들고 그 안에서 스케줄 작업을 실행하는 방향이었다.

하지만 사용자가 기대하는 것은 보통 다음에 가깝다.

- 등록 당시 작업하던 프로젝트 경로에서 실행된다.
- 등록 당시 대화 맥락을 바탕으로 실행된다.
- 원본 대화는 망가지지 않는다.
- 결과만 현재 채팅으로 돌아온다.

별도 workspace는 `/start <schedule_id>` 같은 continuation 개념을 만들지만, 세션 복제 방식에서는 그것이 핵심이 아니다. 스케줄 실행은 원본 프로젝트 경로에서 복제된 provider 세션으로 수행되어야 한다.

## 최종 목표

이번 작업의 최종 목표는 다음이다.

### 1. 스케줄 등록은 가볍고 결정적이어야 한다

`--cron` 또는 `--at` 등록이 성공했다는 것은 다음까지만 의미해야 한다.

- 스케줄 JSON 파일이 생성되었다.
- prompt, schedule, schedule_type, current_path가 저장되었다.
- 등록 당시의 provider, model, session_id가 가능한 경우 저장되었다.
- 백그라운드 요약 작업은 시작되지 않는다.
- 등록 직후 별도 AI 호출은 일어나지 않는다.

즉 등록 명령이 stdout을 내고 exit 0으로 끝났다면, "나중에 실행할 작업 지시와 원본 세션 메타데이터를 안전하게 기록했다"는 의미다.

그 시점에 실제 스케줄 작업이 수행된 것은 아니다. provider 세션 복제도 아직 수행된 것이 아니다.

### 2. 스케줄 실행 시점에 원본 세션을 복제하거나 fork한다

스케줄 실행 시점이 오면 그때 저장된 `session_id`, `provider`, `model`, `current_path`를 기준으로 실행 준비를 한다.

비-inline 기본 모드에서는 원본 세션을 직접 resume하지 않는다. provider별로 다음 방식 중 하나를 사용한다.

- Codex: Codex 세션 파일과 state DB 정보를 복제해 새 Codex session id를 만든다.
- OpenCode: OpenCode SQLite DB의 session/message/part 관련 row를 복제하고 id 참조를 새 id로 remap한다.
- Agy: Antigravity conversation 파일을 새 conversation id로 복사한다.
- Claude: Claude CLI의 native `--fork-session`을 사용해 원본 세션을 fork한다.

그 다음 스케줄 prompt는 복제 또는 fork된 세션에 전달한다.

### 3. 원본 세션은 변경하지 않는다

스케줄 실행은 원본 대화에 영향을 주면 안 된다.

이를 위해 기본 모드는 원본 `session_id`를 직접 resume하지 않는다. 원본 세션은 스케줄 실행의 기준점으로만 사용된다.

스케줄 실행 중 생성된 대화, tool 결과, 파일 생성, provider transcript 변경은 복제된 세션 또는 fork된 세션에 남아야 한다.

사용자의 현재 채팅 세션은 실행 전 백업되고 실행 후 복원된다.

### 4. 반복 cron은 매번 원본 세션을 새로 복제한다

반복 cron 정책은 "이전 스케줄 실행 결과를 이어서 다음 실행을 한다"가 아니다.

이번 작업에서 정한 정책은 다음이다.

매 실행마다 등록 당시 저장된 원본 세션을 다시 복제해서 실행한다.

이 정책의 의미는 다음과 같다.

- 반복 실행끼리 서로의 대화 상태에 의존하지 않는다.
- 첫 번째 실행 결과가 두 번째 실행의 context가 되지 않는다.
- 실행마다 동일한 원본 기준점에서 출발한다.
- `context_summary`를 갱신하거나 이어붙일 필요가 없다.
- 이전 실행의 실패나 잘못된 추론이 다음 실행에 누적되지 않는다.

반복 실행 간의 상태 공유가 필요한 작업은 사용자가 명시적으로 파일, DB, 외부 시스템 등에 상태를 저장하게 해야 한다.

### 5. 별도 schedule workspace를 만들지 않는다

비-inline 기본 모드에서도 `~/.cokacdir/workspace/<schedule_id>/`를 새로 만들지 않는다.

스케줄은 등록 당시의 `current_path`를 working directory로 사용한다. 이는 "복제된 provider 세션이 원래 작업하던 프로젝트 경로에서 실행된다"는 뜻이다.

따라서 스케줄 완료 후 `/<schedule_id>`로 들어갈 별도 workspace도 없다. 스케줄 결과는 채팅에 표시되고, provider별 복제 세션 transcript는 provider 저장소에 남는다.

### 6. inline 모드는 별도의 정책으로 유지한다

`COKAC_SCHEDULE_INLINE=1`이 설정된 경우는 기본 모드와 다르다.

inline 모드의 목표는 "스케줄 prompt를 현재 채팅에 사용자가 직접 입력한 것처럼 처리"하는 것이다.

따라서 inline 모드에서는 다음이 맞다.

- 현재 채팅의 live session id를 사용한다.
- 현재 채팅의 current_path를 사용한다.
- prompt와 응답이 현재 대화에 누적된다.
- 원본 세션을 보호하기 위해 clone하는 기본 모드와 달리, 의도적으로 live session을 이어간다.

inline 모드는 사용자가 명시적으로 활성화하는 별도 동작이다.

## 비목표

이번 작업에서 하지 않으려는 것도 명확히 해야 한다.

### 1. 모든 provider에 같은 clone 구현을 강제하지 않는다

provider마다 세션 저장 방식이 다르다.

따라서 같은 추상 인터페이스를 억지로 맞추기보다 provider별로 올바른 방식을 쓴다.

- Codex는 rollout JSONL과 `state_5.sqlite`를 다룬다.
- OpenCode는 SQLite row를 다룬다.
- Agy는 conversation 파일을 다룬다.
- Claude는 CLI가 제공하는 `--fork-session`을 쓴다.

핵심은 구현 모양이 같은지가 아니라 원본 세션이 변경되지 않는지다.

### 2. 스케줄 실행 결과를 원본 세션에 합치지 않는다

스케줄 실행 결과를 원본 대화에 자동으로 merge하지 않는다.

이유는 간단하다. merge는 원본 세션 변경이다. 이번 작업의 핵심 목표와 충돌한다.

결과는 채팅으로 전달되지만, 원본 provider 세션 transcript에 자동 반영되면 안 된다.

### 3. 복제 세션을 즉시 삭제하지 않는다

요약용 임시 세션이라면 삭제가 맞다.

하지만 이번 방식의 복제 세션은 실제 스케줄 작업이 수행된 실행 세션이다. 이 세션은 결과의 근거와 transcript를 담는다.

따라서 Codex, OpenCode, Agy의 schedule clone은 생성 후 바로 cleanup하지 않는다. Claude도 native fork 세션을 사용한다.

### 4. `/start <schedule_id>` continuation을 유지하지 않는다

별도 schedule workspace를 만들지 않으므로, 스케줄 완료 후 `/<schedule_id>`로 들어가는 continuation 모델도 유지하지 않는다.

이는 기능 제거라기보다 실행 모델 변경에 따른 자연스러운 결과다.

### 5. 구버전 schedule JSON을 즉시 강제 migration하지 않는다

기존 파일에 `context_summary`가 남아 있을 수 있다.

이를 읽는 호환 필드는 남긴다. 하지만 실행에는 사용하지 않는다. 그리고 다음 write 경로에서 `context_summary`는 새 JSON에 다시 저장하지 않는다.

즉 "읽을 수는 있지만, 더 이상 의미 있는 실행 입력으로 쓰지 않는다"가 목표다.

## 스케줄 등록 시 저장해야 하는 정보

스케줄 등록 시 최소한 다음 정보가 저장되어야 한다.

- `id`: 스케줄 id
- `chat_id`: 채팅 id
- `bot_key_verifier`: 소유 bot 검증용 값
- `current_path`: 등록 당시 working directory
- `prompt`: 나중에 실행할 사용자 지시
- `schedule`: cron 식 또는 절대 시간
- `schedule_type`: `cron` 또는 `absolute`
- `once`: cron one-shot 여부
- `last_run`: 반복 cron의 마지막 실행 시각
- `created_at`: 생성 시각
- `session_id`: 등록 당시 provider 세션 id, 있으면 저장
- `provider`: 등록 당시 provider
- `model`: 등록 당시 model

`context_summary`는 새로 저장하지 않는다.

## 스케줄 실행 시 기대 흐름

비-inline 기본 모드에서 기대하는 흐름은 다음이다.

1. scheduler loop가 실행 대상 schedule entry를 찾는다.
2. 채팅이 busy이면 pending으로 두고 다음 cycle에서 다시 본다.
3. 실행 가능하면 현재 채팅 세션을 백업한다.
4. 취소와 accounting을 위해 임시 `ChatSession`을 state에 넣는다.
5. `execute_schedule`이 호출된다.
6. working directory는 `entry.current_path`가 된다.
7. provider와 model은 entry에 저장된 값을 우선 사용한다.
8. `entry.session_id`가 있으면 provider별 clone/fork를 수행한다.
9. clone/fork 결과로 얻은 session id를 실제 실행 함수에 넘긴다.
10. prompt는 `entry.prompt`만 사용한다.
11. legacy `context_summary`는 무시한다.
12. 실행 결과는 채팅에 stream된다.
13. 반복 cron이면 `last_run`만 갱신한다.
14. one-time schedule이면 실행 전에 schedule 파일을 삭제하고, 실행 후 되살리지 않는다.
15. 현재 채팅 세션은 실행 전 상태로 복원한다.
16. schedule history에는 결과와 working directory를 기록한다.

## provider별 목표

### Codex

Codex에서는 `cokacmux`에서 확인한 세션 복제 방식을 채택하는 것이 목표다.

핵심은 다음이다.

- 원본 rollout JSONL을 찾는다.
- 새 session UUID를 만든다.
- rollout 안의 session id, cwd 등을 새 값에 맞게 patch한다.
- 새 rollout JSONL을 Codex sessions 경로에 쓴다.
- `~/.codex/state_5.sqlite`의 `threads` row도 새 session id와 새 rollout path에 맞게 복사한다.
- 스케줄 실행은 새 session id로 `codex exec resume`한다.

이 방식은 원본 Codex 세션을 직접 resume하지 않기 위한 것이다.

### OpenCode

OpenCode에서는 SQLite DB row 복제가 목표다.

핵심은 다음이다.

- OpenCode DB 위치를 찾는다.
- `session`, `message`, `part` row를 복제한다.
- 필요한 경우 `session_message` row도 복제한다.
- 새 `ses_`, `msg_`, `prt_`, `evt_` id를 만든다.
- message parent 참조와 part/message/session 참조를 새 id로 remap한다.
- session directory/path와 message data의 cwd를 실행 working directory에 맞춘다.
- 새 session id를 normal serve/SSE 실행 경로에 넘긴다.

OpenCode의 기존 `--fork` 기반 summary 경로는 스케줄 실행 목표와 맞지 않으므로 제거 대상이다.

### Agy

Agy에서는 conversation 파일 복제가 목표다.

핵심은 다음이다.

- 원본 conversation 파일 경로를 찾는다.
- 새 conversation id를 만든다.
- 원본 `.db` 또는 conversation 파일을 새 id 이름으로 복사한다.
- SQLite sidecar인 `.db-wal`, `.db-shm`가 있으면 함께 복사한다.
- 스케줄 실행은 새 conversation id로 resume한다.

Agy도 summary 방식이 없어야 한다.

### Claude

Claude는 native `--fork-session`을 사용할 수 있다.

따라서 별도 수동 복제 구현을 하지 않고, 스케줄 실행 시 원본 session id와 `--fork-session`을 함께 사용한다.

중요한 점은 Claude에서도 context summary를 만들지 않는다는 것이다.

## `context_summary`에 대한 최종 정책

`context_summary`의 최종 정책은 다음이다.

- 새 schedule 등록에서는 만들지 않는다.
- 백그라운드 `--cron-context` 프로세스를 띄우지 않는다.
- 실행 prompt에 붙이지 않는다.
- 반복 cron 실행 후 새 summary를 만들지 않는다.
- 구버전 schedule JSON에 필드가 있으면 읽을 수는 있다.
- 다음 write에서는 다시 저장하지 않는다.
- 로그에서는 legacy field 여부 정도만 확인한다.

즉 `context_summary`는 더 이상 스케줄 실행 메커니즘의 일부가 아니다.

## `--cron-context`에 대한 최종 정책

`--cron-context`는 예전 백그라운드 요약 프로세스용 CLI entry였다.

이번 작업 후 내부에서는 더 이상 호출하지 않는다.

다만 외부에서 오래된 방식으로 호출될 가능성을 고려해 CLI 분기 자체는 남겨둘 수 있다. 이 경우 동작은 실제 요약 수행이 아니라 명시적 오류 반환이어야 한다.

기대 메시지는 다음 의미를 가져야 한다.

- `--cron-context`는 더 이상 지원하지 않는다.
- 스케줄 실행은 execution-time provider session clone/fork 방식으로 수행된다.

## current_path와 cwd 정책

스케줄 등록 당시의 `current_path`는 중요하다.

비-inline 기본 모드에서는 이 값을 실행 working directory로 사용한다.

이는 다음 의미다.

- 별도 schedule workspace로 이동하지 않는다.
- 등록 당시 프로젝트 위치에서 실행한다.
- provider 세션 복제물 내부의 cwd도 가능한 한 이 working directory와 일치하게 patch한다.

Codex/OpenCode처럼 세션 내부에 cwd가 저장되는 provider에서는 clone 과정에서 cwd 값을 조정한다. 이는 복제된 세션이 실행될 때 provider가 잘못된 이전 경로를 참조하지 않게 하기 위한 것이다.

## 성공 기준

이번 작업은 다음 조건을 만족해야 성공이다.

### 등록 단계

- `--cron` 등록 시 즉시 schedule JSON이 저장된다.
- stdout이 반환되고 프로세스가 종료된다.
- 백그라운드 요약 프로세스가 뜨지 않는다.
- `session_id`, `provider`, `model`이 가능한 경우 저장된다.
- `context_summary`는 새 파일에 저장되지 않는다.

### 실행 단계

- 비-inline 실행은 `entry.current_path`를 working directory로 사용한다.
- Codex/OpenCode/Agy는 원본 세션을 복제한 뒤 복제 세션 id로 실행한다.
- Claude는 `--fork-session`으로 실행한다.
- 원본 세션 id를 직접 변경하지 않는다.
- prompt에 summary 텍스트를 붙이지 않는다.
- 현재 채팅 세션은 실행 후 복원된다.
- 반복 cron은 매번 원본 세션을 새로 clone/fork한다.

### 호환성

- 구버전 schedule JSON의 `context_summary` 필드는 읽어도 실행에 쓰지 않는다.
- 다음 write에서 legacy `context_summary`는 사라진다.
- 구버전 `bot_key`는 기존 verifier migration 흐름을 유지한다.
- schedule history의 기존 `workspace_path` JSON key는 호환을 위해 유지할 수 있지만, 값의 의미는 현재 실행 working directory다.

### 문서

- 사용자 문서는 더 이상 "기본 모드가 별도 workspace를 만든다"고 설명하지 않아야 한다.
- 사용자 문서는 더 이상 "recurring cron이 `context_summary`를 carry forward한다"고 설명하지 않아야 한다.
- `COKAC_SCHEDULE_INLINE` 문서는 기본 모드를 cloned-session mode로 설명해야 한다.

## 의도적으로 남긴 것

### legacy `context_summary` 필드

`ScheduleEntry`와 `ScheduleEntryData`에 `context_summary` 필드가 남아 있을 수 있다.

이는 새 기능을 위한 것이 아니다. 구버전 schedule JSON을 읽기 위한 호환성이다.

실행에서 이 값을 쓰면 안 된다.

### schedule history의 `workspace_path` key

history JSON에 `workspace_path`라는 key가 남아 있을 수 있다.

이는 기존 history 소비 코드와의 호환 때문이다. 현재 스케줄 실행에서는 이 값이 "별도 workspace 폴더"가 아니라 "실행 working directory"를 의미한다.

## 이번 작업에서 특히 피해야 하는 회귀

다음 회귀가 생기면 이번 작업의 목표를 깨는 것이다.

- `--cron` 등록 후 다시 백그라운드 AI 요약을 시작하는 것
- `context_summary`를 새 schedule JSON에 다시 쓰는 것
- 스케줄 실행 prompt에 summary를 붙이는 것
- 반복 cron 실행 후 다음 실행용 summary를 만드는 것
- Codex/OpenCode/Agy에서 원본 session id를 직접 resume하는 것
- 비-inline 기본 모드에서 다시 `~/.cokacdir/workspace/<schedule_id>/`를 만드는 것
- 스케줄 결과를 원본 provider session에 자동 merge하는 것
- provider/model을 실행 시점의 현재 채팅 설정으로 무조건 덮어쓰는 것
- old `--cron-context`를 실제 요약 실행 경로로 되살리는 것

## 최종 설계 요약

이 작업의 핵심 설계는 다음 문장으로 정리된다.

스케줄은 등록 시 원본 세션의 식별자와 실행 경로를 저장하고, 실행 시마다 그 원본 세션을 provider별 방식으로 복제하거나 fork해서 복제 세션에서 prompt를 수행한다. 원본 세션은 변경하지 않고, summary는 만들지 않으며, 별도 schedule workspace도 만들지 않는다.

