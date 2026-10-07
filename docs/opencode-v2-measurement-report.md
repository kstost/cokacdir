# OpenCode v2.0.24 실측 보고서

2026-10-07

## 요약

opencode 2.0.24는 1.x와 비교해 HTTP API, 이벤트 스트림, 메시지 구조, `run` CLI, DB 스키마가 모두 바뀌었습니다. 그래서 배포된 cokacdir(v0.8.30)는 opencode 2.x에서 동작하지 않습니다. 현재 작업 트리(커밋 전)의 v2 대응 코드는 빌드해서 실제 opencode로 끝까지 검증했고, 검증 중 발견한 문제 2건은 수정했습니다.

- **배포된 cokacdir:** opencode 2.x라면 최신 2.0.24에서도 `405 Method Not Allowed`와 HTML 파싱 오류가 납니다(직접 재현).
- **작업 트리:** `opencode --version`으로 2.x를 감지해 v2 전용 경로로 처리합니다. 단위 테스트 43개, 실제 DB·서버 대상 테스트, 빌드한 바이너리의 e2e 시나리오 24개와 회귀 9개가 모두 통과했습니다.
- **수정 7건:** 검증 중 2건(실패 원인 표시, `/stop` 시 세션 미정리), 실수하기 쉬운 지점 점검에서 5건(첫 프롬프트 타임아웃, `opencode models` 멈춤, 업그레이드 후 버전 재감지, `XDG_DATA_HOME`, 취소 중 프롬프트 실패). 아래 "실수하기 쉬운 지점 점검" 참고.
- **휴리스틱 재구현 8건:** 시간·추측으로 판단하던 곳을 opencode의 확정 신호(종료 코드, DB 기록, inbox, 이벤트 순서, 프로세스 종료, `opencode debug paths`)로 바꿨습니다. 아래 "휴리스틱 조사와 재구현" 참고.

## 실측 환경과 방법

모든 결과는 설치된 opencode 2.0.24를 직접 실행해 얻었습니다. 문서나 기억에 기대지 않았습니다.

| 항목 | 내용 |
| --- | --- |
| opencode | v2.0.24, `~/.opencode/bin/opencode`, 백그라운드 서비스(`serve --service`) 실행 중 |
| OS | Linux aarch64 (Ubuntu, glibc 2.43) |
| 모델 | `openai/gpt-5.5`(OpenAI 인증), `opencode/big-pickle`(무료) |
| cokacdir | 작업 트리(커밋 전, Cargo.toml 0.8.31). cargo 1.96.1로 디버그 빌드(`build.py`는 이 환경에서 `cargo-zigbuild` 권한 문제로 실행 불가) |

실측 방법은 다섯 가지입니다.

1. CLI 직접 실행: `run`, `models`, `api`, `serve`, `debug`
2. `opencode serve`에 curl과 Python으로 HTTP·SSE 요청, 이벤트 전부 기록
3. 서버의 OpenAPI 스펙(`/openapi.json`, 270KB) 분석과 바이너리 문자열 분석
4. `~/.local/share/opencode/opencode.db` 읽기 전용 조회
5. Rust v2 어댑터의 폴링·판정 로직을 그대로 옮긴 Python 시뮬레이터로 실제 서버 대상 시나리오 실행
6. cokacdir를 빌드해 단위 테스트, 실제 DB·서버 대상 임시 테스트(검증 후 제거), `--test-opencode-sse` 하네스 e2e 실행

## CLI 변화

`opencode run`은 기존 호출 방식으로는 아예 실행되지 않습니다. `--dir`가 없어졌고 프롬프트 전달 방식도 바꿔야 합니다.

| 항목 | 2.0.24 실측 결과 | 대응 |
| --- | --- | --- |
| `run --dir` | `Unrecognized flag: --dir`, exit 1 | 플래그 제거, cwd와 `$PWD`로 지정 |
| `run -- "<prompt>"` | 프롬프트가 두 번 들어감. `single` → `single single`, `two words` → `"two words" "two words"` | stdin으로만 전달 |
| `run "say hi"` (`--` 없이) | 따옴표까지 저장됨: `"say hi"` | stdin으로만 전달 |
| stdin + `-m` | 정상(1.x에서는 `-m`이 있으면 stdin 무시) | 항상 stdin |
| 실행 서버 | 기본은 공유 백그라운드 서비스에 접속. `--standalone`이면 자식 프로세스로 전용 서버 | `--standalone` (환경변수와 프로세스 kill이 적용됨) |
| 권한 | `OPENCODE_PERMISSION` 제거. `--auto` = deny가 아닌 요청을 자동 승인 | `--auto` |
| `--format json` 종료 | 마지막 단계의 `step_finish`가 나오지 않음. `--standalone`에서는 `step_start`도 생략되기도 함 | exit 0을 완료로 간주 |
| `tool_use` 이벤트 | `part.id`가 호출 ID, 파트 ID는 `part.partID`. 입력은 `path` 등 v2 필드명 | 도구 정규화 표 갱신 |
| 오류 | `{"type":"error","error":{"type","message"}}`, exit 1 | 기존 파싱 그대로 |
| 없는 세션 `-s` | 그 ID로 새 세션을 만듦(1.x는 NotFoundError) | legacy 경로만 해당, 보고만 |
| SIGINT | 세션을 interrupt하고 0.3초 안에 exit 130 | `/stop` 시 kill 전에 SIGINT |
| `opencode models` | 서비스 경유 26개. `--standalone`은 항상 0개. 첫 호출에서 1회 빈 결과 | 재시도, 빈 결과는 캐시하지 않음 |
| `opencode --version` | `opencode v2.0.24` (1.x는 `1.15.5`) | 메이저 버전으로 분기 |
| `opencode api` | 신규. `--standalone`으로 단발 API 호출. 실패 시 본문 + `HTTP 404 Not Found`, exit 1 | 세션 복제에 사용 |

## serve HTTP API 변화

모든 API가 `/api` 아래로 옮겨졌고 Basic 인증이 필수입니다. 기존 경로는 새 웹 UI가 받아서, POST는 405를, GET은 HTML을 200으로 돌려줍니다.

- **기동:** `serve --port 0`은 stdout에 `server listening on http://127.0.0.1:PORT`와 `server password <비밀번호>`를 출력합니다. `serve --stdio --port 0`은 `{"url":"http://127.0.0.1:PORT"}` 한 줄을 출력하고, stdin이 닫히면 0.07초 안에 종료합니다.
- **인증:** 사용자 `opencode`, 비밀번호는 `OPENCODE_PASSWORD` 또는 `OPENCODE_SERVER_PASSWORD`입니다. 환경변수로 주면 password 줄을 출력하지 않고, 인증이 없으면 401입니다. Bearer는 거부됩니다.
- **스펙:** `/openapi.json`(인증 필요)에 operation 150여 개가 있습니다.

1.x 방식 요청을 2.0.24 서버에 보낸 결과입니다.

| 요청 (1.x 방식, 인증 없음) | 응답 |
| --- | --- |
| `POST /session?directory=...` | 405 Method Not Allowed |
| `POST /session/{id}/prompt_async` | 405 Method Not Allowed |
| `GET /session/{id}/message` | 200, `text/html` (웹 UI) |
| `GET /global/event` | 200, `text/html` |
| `GET /session/status` | 200, `text/html` |
| `POST /api/session` (인증 없음) | 401 Unauthorized |

cokacdir가 쓰는 기능별 엔드포인트 대응입니다.

| 용도 | 1.x | 2.0.24 |
| --- | --- | --- |
| 세션 생성 | `POST /session?directory=` `{title}` → `{id}` | `POST /api/session` `{location:{directory}, model?, permissions?}` → `{data:{id}}` |
| 프롬프트 | `POST /session/{id}/prompt_async` `{parts, model}` → 204 | `POST /api/session/{id}/prompt` `{text}` → 즉시 `{data:{id}}`(사용자 메시지 ID) |
| 이벤트 | `GET /global/event` (`payload` 봉투) | `GET /api/event` (봉투 없음) |
| 실행 상태 | `GET /session/status` | `GET /api/session/active` → `{data:{ses_…:{type:"running"}}}` |
| 자식 세션 | `GET /session/{id}/children` | `GET /api/session?parentID={id}` |
| todo | `GET /session/{id}/todo` | 없음 (todowrite 도구 제거) |
| 메시지 | `GET /session/{id}/message` | `GET /api/session/{id}/message?limit=&order=&cursor=` → `{data, cursor}` |
| 모델 | 프롬프트 본문 | `POST /api/session/{id}/model` `{model}` |
| 중단 | — | `POST /api/session/{id}/interrupt` → `{interrupted:true}` |
| 권한 요청 | — | `GET /api/session/{id}/permission`, `POST …/permission/{rid}/reply` `{decision:"once"}` |
| 질문 폼 | — | `GET /api/session/{id}/form`, `DELETE …/form/{fid}?message=` |
| 복제·이동 | DB 행 복사 | `POST /api/session/{id}/fork`, `POST /api/session/{id}/move` `{directory}` |

## 이벤트 스트림 (SSE)

`/api/event`는 봉투 없이 `data: {id, created, type, location, data, durable}` 프레임을 보내고, 하트비트는 `: heartbeat` 주석입니다. 1.x의 `message.updated`, `message.part.updated`, `message.part.delta`는 없습니다.

| 이벤트 | 주요 필드 (`data`) | cokacdir 용도 |
| --- | --- | --- |
| `server.connected` | 없음 | 구독 확인 |
| `session.created` | `sessionID`, `parentID`, `location`, `model` | 자식(subagent) 세션 추적 |
| `session.execution.started` / `succeeded` | `sessionID` | 실행 경계 |
| `session.execution.failed` | `error {type, message}` | 실패 원인(기록에는 남지 않음) |
| `session.execution.interrupted` | `reason` (`user`, `shutdown`) | 취소 확인 |
| `session.step.started` / `ended` / `failed` | `assistantMessageID`, `finish`, `tokens`, `error` | 단계 경계 |
| `session.text.started` / `delta` / `ended` | `assistantMessageID`, `ordinal`, `delta` 또는 전체 `text` | 응답 텍스트 스트리밍과 보정 |
| `session.reasoning.*` | `delta`, `text` | 표시하지 않음 |
| `session.tool.input.started` | `id`, `name` | 도구 이름 |
| `session.tool.called` | `id`, `input` | 도구 입력 |
| `session.tool.success` / `failed` | `content[]` 또는 `error {type, message}` | 도구 결과 |
| `session.tool.progress` | `metadata` (예: subagent `sessionID`, `shellID`) | 참고 |
| `permission.asked` / `replied` | `id`, `action`, `resources` | 참고(승인은 폴링으로) |
| `form.created` / `cancelled` | `form {id, sessionID, metadata.kind}` | 참고(취소는 폴링으로) |
| `session.moved`, `session.renamed`, `session.inbox.*`, `session.usage.updated`, `session.instructions.updated` | — | 사용 안 함 |

도구 결과는 문자열 `output`이 아니라 `content[]` 배열로 옵니다. 실패하면 문자열이 아닌 `error` 객체로 옵니다.

## 메시지 모델과 턴 완료 판정

세션 기록은 타입이 있는 메시지 목록이고, 턴마다 끝에 `idle` 마커가 붙습니다. 단, 모델 단계 전에 실패한 턴은 기록에 원인이 남지 않습니다.

| 타입 | 내용 |
| --- | --- |
| `user` | `text`, `files` |
| `assistant` | 모델 단계 하나. `content[]`(text, reasoning, tool), `finish`, `error`, `time.completed`, `tokens` |
| `idle` | 턴 종료. `outcome`: `succeeded`, `failed`, `interrupted` |
| `synthetic` | 시스템이 넣은 입력(예: 백그라운드 subagent 보고), `metadata` |
| `model-switched` | 모델 변경 기록 |
| 기타 | `agent-switched`, `location-switched`, `system`, `skill`, `shell`, `compaction` |

- **턴 경계:** assistant에 `parentID`가 없습니다. prompt 응답의 `data.id`가 기록의 user 메시지 ID와 같아서, 그 메시지 뒤를 이번 턴으로 봅니다.
- **페이지:** 기본 순서는 최신순(desc)입니다. `limit` 최대값은 200이고, 201이면 400입니다. 더 오래된 페이지는 `cursor.next`로 받습니다.
- **실패 원인:** 없는 모델로 실행하면 기록에는 `user`와 `idle(failed)`만 남습니다. `Model unavailable: openai/nonexistent-model-xyz`라는 원인은 SSE의 `session.execution.failed`에만 있고, 세션 조회도 `outcome: failed`만 돌려줍니다.
- **중단:** 마지막 assistant에 `error {type: aborted, message: "Step interrupted"}`가 붙고 `idle(interrupted)`로 끝납니다.
- **성공:** 마지막 assistant가 `finish: stop`이고 text 파트가 최종 답입니다. 도구를 부른 중간 단계는 `finish: tool-calls`입니다.

완료는 다음 조건이 연속 2회(0.5초 간격) 관측될 때로 판정합니다.

1. 부모와 모든 자손 세션이 `/api/session/active`에 없음
2. 보고를 기다리는 백그라운드 subagent가 없음
3. 이번 턴의 마지막 메시지가 `idle`

## 권한·질문 처리

아무것도 하지 않으면 프로젝트 밖 경로를 읽는 순간 세션이 멈춥니다. 1.x에서 막아 주던 `OPENCODE_PERMISSION` 환경변수는 2.0.24 바이너리에 문자열 자체가 없습니다.

- **기본 규칙:** build 에이전트는 `external_directory`와 `*.env` 읽기가 `ask`입니다. `/etc/hostname` 읽기에서 `permission.asked`가 나오고, 세션은 active 상태로 응답을 기다렸습니다.
- **규칙 형식:** 설정은 `permissions: [{action, resource, effect}]`이고, 마지막에 맞는 규칙이 적용됩니다.

권한 요청을 막는 방법 세 가지를 실측했습니다.

| 방법 | 결과 | 문제 |
| --- | --- | --- |
| 세션 생성 시 `permissions` 지정 | 요청 없이 진행 | 세션에 저장되어 TUI에서도 남음 |
| `OPENCODE_CONFIG_CONTENT`로 `* allow` 주입 | 요청 없이 진행 | 에이전트 규칙 뒤에 붙어 explore의 `* deny`, plan의 `edit deny`, general의 `subagent deny`까지 덮어씀 |
| 대기 중인 요청에 `once` 응답 (작업 트리 채택) | 요청 직후 승인되고 진행 | 없음. deny는 유지되어 `run --auto`와 같은 의미 |

질문 도구(`question`)는 `form.created`를 만들고, 답이 없으면 영구히 기다립니다.

| 처리 | 결과 |
| --- | --- |
| 메시지 없이 폼 취소 | 도구 실패(`aborted`), 턴 전체가 `interrupted`(reason `shutdown`)로 끝남 |
| `?message=<피드백>`과 함께 취소 (작업 트리 채택) | 피드백이 도구 오류로 전달되고, 모델이 가정을 밝히며 계속 진행해 `succeeded` |
| 설정에서 `question` deny | 도구가 목록에서 빠짐(`Unknown tool 'question'`) |

## subagent

백그라운드 subagent는 부모를 먼저 idle로 만든 뒤, 끝날 때 보고 메시지로 부모를 다시 실행시킵니다. 그래서 "부모가 idle이면 완료"로 판정하면 최종 답을 놓칩니다.

- **포그라운드(기본):** 부모는 자식이 끝날 때까지 active입니다. 자식의 답은 도구 결과 `<subagent sessionID=… state="completed">`로 들어옵니다.
- **자식 세션:** `session.created`에 `parentID`가 있고, `GET /api/session?parentID=`로 조회됩니다. 자식의 이벤트는 같은 스트림에 섞여 옵니다.
- **백그라운드 표식:** 도구 결과의 `state.metadata`가 `{sessionID, status: "running"}`입니다.
- **보고 메시지:** `synthetic` 타입, `metadata = {source: "subagent", childID, state: "completed"}`.

백그라운드 subagent 턴의 실측 타임라인입니다(openai/gpt-5.5, 프롬프트 이후 경과 시간).

| 시각 (초) | 세션 | 이벤트 |
| --- | --- | --- |
| 0.47 | 부모 | 1차 실행 시작 |
| 3.76 | 부모 | subagent 도구 호출 (`background: true`) |
| 3.77 | 자식 | 세션 생성 (`parentID` = 부모) |
| 3.83 | 자식 | 실행 시작 (shell `sleep 12; echo BGDONE`) |
| 6.08 | 부모 | 1차 실행 종료, 답 `LAUNCHED`, `idle(succeeded)` |
| 22.65 | 자식 | 실행 종료, 답 `BGDONE` |
| 22.66 | 부모 | 보고 메시지(`synthetic`) 도착 |
| 22.67 | 부모 | 2차 실행 시작 |
| 25.63 | 부모 | 2차 실행 종료, 답 `FINAL: BGDONE`, `idle(succeeded)` |

```text
0초       5초        10초       15초       20초       25초
|---------|----------|----------|----------|----------|--
부모  [=====]·········· idle ··························[===]
         \  LAUNCHED                                 ^  FINAL: BGDONE
          v                                          |  (10ms 후 재실행)
자식      [=========================================]
          3.83                                   22.65
```

자식이 22.65초에 끝나자 10ms 뒤 부모가 다시 실행되므로, 부모와 자손이 모두 비활성인 상태를 연속으로 확인해야 완료입니다.

## 모델 선택

2.0.24에서는 모델이 프롬프트의 속성이 아니라 세션의 설정입니다. prompt 본문에는 model 필드가 없습니다.

- **지정 방법:** 세션 생성 시 `model`로 지정하거나, 이후 `POST /api/session/{id}/model`로 바꿉니다.
- **같은 모델 재지정:** 204를 돌려주고 기록에 아무것도 추가되지 않습니다. 다른 모델이면 `model-switched` 메시지가 남습니다.
- **없는 모델:** 지정 단계에서는 204로 받아들이고, 실행할 때 `provider.no-route`로 실패합니다.
- **variant:** CLI는 `provider/model#variant`, API는 `{providerID, id, variant}`입니다. `#high` 지정을 확인했습니다.
- **미지정 시 기본 모델:** 서버 기본값은 이 환경에서 `opencode/fledge-alpha-free`입니다. `run`을 `-m` 없이 실행해도 같은 모델을 씁니다. TUI가 기억하는 최근 모델(`~/.local/state/opencode/model.json`, 여기서는 `openai/gpt-6-luna`)과는 다릅니다.

## 오류·취소·복구

interrupt API는 0.2초 안에 턴을 정리합니다. 실행 도중 서버를 죽여도 다음 프롬프트에서 자동으로 복구되었습니다.

| 상황 | 실측 결과 |
| --- | --- |
| 없는 모델 | 즉시 `session.execution.failed` (`provider.no-route`, `Model unavailable: openai/nonexistent-model-xyz`). 기록에는 `idle(failed)`만 |
| `POST …/interrupt` | 0.01초에 `{interrupted:true}`, 0.2초 안에 `execution.interrupted`(reason `user`). 실행 중 도구는 `Tool execution interrupted` |
| 도구 실행 중 서버 kill -9 | `session_v2.time_suspended`가 기록됨. 새 서버를 띄우고 그 디렉터리를 로드해도 45초간 재실행 없음 |
| 그 세션에 다음 프롬프트 | 남은 도구를 `Tool execution interrupted: shell`로 정리하고 새 요청만 처리 |
| 응답 생성 중 kill -9 | 위와 같음. 생성 중이던 텍스트는 저장되지 않음 |
| 없는 세션에 prompt / model | 404 `SessionNotFoundError` |
| `run`에 SIGINT | 세션이 `idle(interrupted)`가 되고 `run`은 exit 130 |

바이너리에는 서버 시작 시 실행되는 `resumeSuspendedSessions`가 있습니다. 재개 대상은 백그라운드 작업(백그라운드 subagent, 백그라운드 shell)이고 일반 턴은 아닙니다. 그래서 작업 트리는 취소 시 서버를 죽이기 전에 interrupt를 보냅니다.

## 도구 이름과 입력 필드

도구 3개의 이름이 바뀌었고, 파일 도구는 `filePath` 대신 `path`를 받습니다. 입력 필드는 실제 도구 호출 이벤트에서 확인했습니다.

| 1.x 이름 | 2.0.24 이름 | 실측한 입력 필드 | cokacdir 표시 |
| --- | --- | --- | --- |
| bash | shell | `command`, `timeout` | Bash |
| read | read | `path`, `offset`, `limit` | Read |
| write | write | `path`, `content` | Write |
| edit | edit | `path`, `oldString`, `newString` | Edit |
| apply_patch | patch | `patchText` | Edit |
| glob | glob | `pattern` | Glob |
| grep | grep | `pattern`, `include` | Grep |
| task | subagent | `agent`, `description`, `prompt`, `background` | Task |
| skill | skill | `name` → `id` | Skill |
| question | question | `questions[]` | Question |
| todowrite | 제거 | — | — |
| — | execute (신규) | `code` (모델이 JS로 다른 도구를 호출) | Execute |

모델마다 받는 도구 목록이 다릅니다. `openai/gpt-5.5`는 write·edit 대신 patch를, `opencode/big-pickle`은 write·edit를 받았습니다. 두 모델 모두 webfetch·websearch를 받았고 `opencode.*` 보조 도구도 있습니다.

## DB 스키마 변화

DB 경로는 그대로(`~/.local/share/opencode/opencode.db`, `OPENCODE_DB`로 변경 가능)지만, 1.x 테이블은 마이그레이션으로 이름이 바뀌거나 사라졌습니다. 그래서 기존 쿼리는 "최신 세션을 못 찾는" 정도가 아니라 테이블이 없어 실패합니다.

| 1.x | 2.0.24 |
| --- | --- |
| `session` | `session_v2`로 이름 변경. `parent_id`, `fork_session_id`, `directory`, `model`(JSON), `idle_outcome`, `time_suspended`, `resume_attempts` 등 |
| `message`, `part` | 없음. `session_message`(id, session_id, type, seq, data JSON) 한 테이블로 통합 |
| `todo`, `session_share` | 없음 |
| — | 신규: `session_inbox`, `session_pending`, `instruction_entry`, `instruction_state`, `instruction_blob`, `event`, `event_sequence`, `credential` 등 |

영향을 받는 cokacdir 기능입니다.

- 세션 ID로 디렉터리 찾기, 디렉터리의 최신 세션 찾기 (Telegram)
- 세션 히스토리 표시 (Telegram)
- 세션 아카이브 (`session_archive.rs`)
- 스케줄용 세션 복제: 2.x는 이벤트 소싱 구조(`event_sequence`, `instruction_state`)라 행 복사로는 일관성을 보장할 수 없어, opencode의 fork API로 바꿔야 합니다.

## 기타: AGENTS.md, fork·move, verify

AGENTS.md를 이용한 시스템 프롬프트 주입과 verify용 fork는 2.0.24에서도 그대로 동작합니다.

- **AGENTS.md:** 작업 디렉터리의 AGENTS.md가 지침 항목 `core/instructions`로 들어가고, 그 내용이 응답에 반영되는 것을 확인했습니다.
- **fork:** `POST /api/session/{id}/fork`는 전체 기록을 복사한 새 루트 세션을 만들고 원본은 바뀌지 않습니다. 제목에 `(fork #1)`이 붙고, 디렉터리는 원본을 따릅니다.
- **move:** `POST /api/session/{id}/move`는 inbox에 대기했다가 다음 프롬프트 직전에 `session.moved`로 적용됩니다. 다음 턴의 `pwd`가 새 디렉터리인 것을 확인했습니다.
- **verify:** `run --standalone --session <ID> --fork --agent plan`에 프롬프트를 stdin으로 넣으면 stdout에는 답(`mission_complete`)만, stderr에는 배너만 나옵니다. 원본 세션의 메시지 수는 그대로였습니다.
- **plan 에이전트:** 2.0.24에서는 edit만 deny이고 shell은 허용됩니다. "write·bash를 막는다"는 기존 코드 주석과 다릅니다. verify는 프롬프트의 "도구를 쓰지 말라"는 지시에 기댑니다.

## cokacdir 구현 대조 결과

작업 트리의 v2 대응 코드(`opencode_v2.rs` 신규, `opencode.rs`·`telegram.rs`·`session_archive.rs`·`claude.rs` 수정)는 위 실측과 일치합니다. 검증 중 문제 2건을 찾아 수정했습니다(`opencode_v2.rs`).

1. **실패 원인 미표시:** 모델 단계 전에 실패한 턴은 사용자에게 `OpenCode turn failed`만 보였습니다. 이제 부모 세션의 `session.execution.failed` 원인을 기록해 두었다가, 기록이 `failed`로 끝났을 때 그 원인을 오류 메시지로 씁니다. 실행이 다시 시작되면 기록을 비우고, assistant 메시지에 붙은 오류를 우선합니다.
2. **`/stop` 시 세션 미정리:** `/stop`이 부르는 `cancel_now`의 pre-kill hook은 interrupt 요청이 `200 OK`를 받자마자 반환했고, 곧바로 서버가 kill되었습니다. opencode는 interrupt에 즉시 응답하고 정리는 잠시 뒤에 끝내므로, 세션이 실행 중 상태(`time_suspended`)로 남았습니다(e2e로 재현). 이제 hook은 턴의 세션들이 `/api/session/active`에서 빠질 때까지(최대 2초) 기다리고, 그 사이 다시 실행된 세션에는 interrupt를 다시 보냅니다. 수정 후 54ms 만에 정리되고 `idle(interrupted)`가 기록되는 것을 확인했습니다.

검증을 위해 내부 테스트 하네스 `--test-opencode-sse`에 옵션 두 개를 추가했습니다(`main.rs`): `--cancel-now`(실제 `/stop`과 같은 `cancel_now` 경로)와 `--system-prompt <text>`(AGENTS.md 주입 경로).

| 영역 | 작업 트리 구현 | 실측과 |
| --- | --- | --- |
| 버전 분기 | `opencode --version` 메이저 2 이상(읽기 실패 시 2로 간주) | 일치 |
| 서버 | `serve --stdio`, 무작위 `OPENCODE_PASSWORD`, Basic 인증 | 일치 |
| 턴 실행 | 세션 생성 → 모델 지정 → SSE 구독 → prompt → 폴링 | 일치 |
| 완료 판정 | 세션 트리 비활성 + 보고 대기 없음 + `idle`, 연속 2회 | 일치 |
| 막힘 방지 | 권한 `once`, 질문 폼은 피드백과 함께 취소 | 일치 |
| 취소 | 자식부터 interrupt 후 서버 종료 | 일치 |
| 도구 정규화 | shell/subagent/patch/execute, `path`, `id`, `agent` | 일치 |
| `run` 경로 | `--standalone --auto`, stdin, exit 0 = 완료, SIGINT | 일치 |
| verify / 복제 | `--fork --agent plan` + stdin / `api --standalone` fork + move | 일치 |
| DB 조회 | `session_v2`, `session_message`, 루트 세션만 | 스키마와 일치(쿼리 실행은 안 함) |
| 모델 목록 | 3회 재시도, 빈 결과 캐시 안 함 | 일치 |
| 실패 원인 | (수정 전) 일반 문구 | 수정함 |
| `/stop` hook | (수정 전) interrupt 응답 직후 반환 | 수정함 |

Rust 로직을 옮긴 Python 시뮬레이터로 실제 서버에서 돌린 결과입니다.

| 시나리오 | 결과 | 소요 |
| --- | --- | --- |
| 단순 응답 | `PING-OK`, 최종 답으로 채택 | 4.7초 |
| 프로젝트 밖 경로 읽기 | 권한 1회 자동 승인 후 `HOST=…` | 7.6초 |
| 질문 도구 | 피드백과 함께 폼 취소, 모델이 가정하고 `FINAL: blue` | 14.9초 |
| 백그라운드 subagent | 보고를 기다려 `FINAL: BGDONE` 채택 | 22.8초 |
| 없는 모델 | `Model unavailable: openai/nonexistent-model-xyz` | 1.5초 |
| 세션 재개 | 이전 턴 답을 재사용하지 않고 새 답 `PREV=PING-OK` | 4.9초 |
| 취소 | interrupt 후 `idle(interrupted)` | 8.9초 |

참고로, 백그라운드 subagent 턴에서는 중간 답과 최종 답이 구분자 없이 스트리밍됩니다(`LAUNCHEDFINAL: BGDONE`). 최종 결과는 정확하고 1.x와 같은 동작이라 고치지 않았습니다.

## 빌드·테스트·e2e 결과

cokacdir를 빌드해 세 단계로 검증했고 모두 통과했습니다. 경고 81개는 모두 기존 코드에서 나온 것이고, v2 변경으로 생긴 경고는 없습니다.

| 단계 | 내용 | 결과 |
| --- | --- | --- |
| 단위 테스트 | `cargo test opencode` (v2 테스트 13개 포함) | 43개 통과 (점검 후) |
| 실제 DB·서버 대상 임시 테스트 | Telegram 세션 조회·히스토리(5개 세션), 아카이브 파싱·저장·트랜스크립트(5개 세션), 메시지 233개 세션의 페이지 넘김, verify, 스케줄용 세션 복제, 모델 목록 | 4개 통과, 검증 후 제거 |
| e2e (`--test-opencode-sse`) | 아래 24개 시나리오 | 모두 통과 |

| # | 시나리오 | 경로 | 결과 |
| --- | --- | --- | --- |
| 1 | 단순 응답 | serve | `PING-OK`, 4.7초 |
| 2 | 세션 재개 | serve | 이전 답을 정확히 인용 |
| 3 | 프로젝트 밖 경로 읽기 | serve | 권한 자동 승인, `HOST=…` |
| 4 | 도구 6종 표시 | serve | Write/Edit/Grep/Glob/Bash/Read, `file_path`·`old_string`·`glob` 키로 정규화 |
| 5 | 질문 도구 | serve | 피드백과 함께 폼 취소, `FINAL: blue` |
| 6 | 백그라운드 subagent | serve | 보고를 기다려 `FINAL: BGDONE` |
| 7 | 없는 모델 | serve | `Model unavailable: openai/nonexistent-model-xyz` |
| 8 | 없는 세션 | serve | `OpenCode session not found: …` |
| 9 | 취소(플래그) | serve | `idle(interrupted)` |
| 10 | `/stop`(`cancel_now`) | serve | 수정 전 세션 미정리, 수정 후 `idle(interrupted)` |
| 11 | 시스템 프롬프트 + 기존 AGENTS.md | serve | 둘 다 반영, AGENTS.md 해시 그대로 복원 |
| 12 | 시스템 프롬프트 + 기존 AGENTS.md | legacy | 둘 다 반영, 해시 그대로 복원 |
| 13 | 도구·권한 | legacy | Read/Bash 표시, `HOST=…` |
| 14 | 세션 재개 | legacy | 이전 답을 정확히 인용 |
| 15 | `/stop`(SIGINT) | legacy | `idle(interrupted)` |
| 16 | 3개 턴 동시 실행 | serve 2 + legacy 1 | 각자 자기 결과, 서로 기다리지 않음 |
| 17 | 150초 걸리는 턴 | serve | 157.6초에 정상 완료 |
| 18 | 수동 compaction 후 재개 | serve | 요약에서 첫 답을 정확히 인용 |
| 19 | 수동 compaction 후 재개 | legacy | 정상 |
| 20 | 턴 도중 자동 compaction 3회 | serve | 최종 답 `LAST=SECRET-TAIL-7731` 정상 채택 |
| 21 | MCP elicitation | serve | "global" 폼 취소, 도구 반환 후 정상 완료 |
| 22 | 백그라운드 subagent 2개 동시 | serve | 둘 다 보고된 뒤 `FINAL=ONE+TWO` |
| 23 | 복제 세션을 다른 디렉터리에서 실행 | serve | `pwd`가 새 디렉터리 |
| 24 | 모델 미지정 | serve | 기본 모델로 정상 |

모든 시나리오가 끝난 뒤 남은 opencode 서버, `run`, MCP 서버 프로세스는 없었습니다.

## 실수하기 쉬운 지점 점검

구현이 실측 없이 가정한 부분을 골라 하나씩 확인했습니다.

| 지점 | 확인 방법 | 결과 |
| --- | --- | --- |
| 사용자 환경에 `OPENCODE_SERVER_PASSWORD`가 있을 때 인증 | 두 변수를 다른 값으로 주고 서버 기동 | 안전. `OPENCODE_PASSWORD`가 우선 |
| verify(`--auto` 없는 `run`)가 권한·질문에서 멈추는지 | 외부 경로 읽기·질문 도구를 유도 | 안전. CLI가 자동 거부·취소하고 피드백을 줌 |
| git 하위 디렉터리, 심볼릭 링크 경로의 세션 저장 | DB의 `directory`·`path` 확인 | 안전. 받은 경로 그대로 저장, cokacdir 조회와 일치 |
| 셸 결과의 첫 항목만 표시 | 실패 종료·큰 출력·백그라운드 명령 | 안전. 두 번째 항목은 모델용 종료 코드 문구 |
| 세션 복제 실패 시 동작 | 코드 확인 | 안전. 원본 세션으로 대신 실행하지 않고 오류 |
| AGENTS.md 주입이 CLAUDE.md를 가리는지 | CLAUDE.md만 있을 때와 둘 다 있을 때 비교 | 해당 없음. v2는 CLAUDE.md를 읽지 않음 |
| 첫 프롬프트 요청 시간 | 격리 환경에서 플러그인(oh-my-opencode) 첫 설치 | **문제.** 27.6초(다른 실행 5.1초·6.4초)로 10초 제한에 걸림 → 수정 |
| `opencode models`가 끝나지 않는 경우 | 서비스가 뜰 수 없는 격리 환경 | **문제.** 한 번에 20.6초, 3회 재시도로 약 65초 → 수정 |
| 실행 중 opencode 업그레이드 | 가짜 바이너리를 1.x→2.x로 교체 | **문제.** 버전을 프로세스 수명 동안 캐시 → 수정 |
| `XDG_DATA_HOME` 설정 환경 | `opencode debug paths`와 비교 | **문제.** opencode는 따르고 cokacdir는 무시 → 수정 |
| 프롬프트 제출 중 `/stop` | 코드 확인 | **문제.** 취소가 오류로 표시됨 → 수정 |

수정 내용입니다.

1. **설정 단계 요청 타임아웃** (`opencode_v2.rs`): v2는 디렉터리를 첫 요청 때 늦게 로드하므로, 플러그인 설치가 첫 프롬프트 요청에 걸립니다(1.x는 서버 준비 단계에서 처리). 세션 생성·에이전트·모델·프롬프트 요청에 180초 제한을 따로 두고, 폴링 요청은 10초를 유지합니다. 12초 늦게 응답하는 로컬 서버로 폴링 요청은 끊기고 설정 요청은 성공하는 것을 확인했습니다.
2. **`opencode models` 타임아웃** (`opencode.rs`): 시도마다 30초 제한을 두고, 실패나 시간 초과는 재시도하지 않습니다. 정상 종료했는데 목록이 빈 경우에만 재시도합니다. 실패 시 20.6초 1회로 줄었습니다.
3. **버전 재감지** (`opencode.rs`): 감지 결과를 바이너리의 실제 경로·크기·수정 시각과 함께 캐시하고, 바이너리가 바뀌면 다시 감지합니다. 감지 실패는 캐시하지 않습니다. 1.x→2.x 교체와 실패 후 복구를 확인했습니다.
4. **`XDG_DATA_HOME`** (`opencode.rs`): DB 경로와 상대 경로 `OPENCODE_DB`의 기준 디렉터리에 반영했습니다.
5. **취소 중 프롬프트 실패** (`opencode_v2.rs`): `/stop`으로 끊긴 제출은 오류 없이 취소로 처리하고, 시간 초과로 끊겼지만 접수됐을 수 있는 턴은 interrupt로 정리합니다.

수정 후 회귀 e2e 9개(단순 응답, 권한, 질문, 백그라운드 subagent, 없는 모델, `/stop`, 시스템 프롬프트, legacy 응답, legacy `/stop`)가 모두 통과했습니다.

## 외부 버그 리포트 검토 (405 Method Not Allowed)

리포트(cokacdir v0.8.30, opencode 2.0.20, macOS)의 증상과 원인 분석은 실측과 일치해서 신빙성이 높습니다. 다만 제안된 수정(경로에 `/api` 붙이기)만으로는 동작하지 않습니다.

| 항목 | 판정 | 근거 |
| --- | --- | --- |
| 새 세션에서 405 | 맞음 | 2.0.24에서 `POST /session` → 405 재현. 서버 준비 라인이 맞아 기동까지는 성공한 뒤 생김 |
| 기존 세션에서 HTML 파싱 오류 | 맞음 | `GET /session/{id}/message` → 200 HTML 재현. 오류 문구가 v0.8.30 코드와 같음 |
| `/api` 접두사, `{data:{…}}` 래핑, `prompt_async`·`status`·`children`·`todo` 제거 | 맞음 | 실측과 일치 |
| DB가 `session_v2`를 사용 | 부분적 | "최신 세션을 못 찾는다"가 아니라 `session` 테이블이 없어 실패. `message`·`part`·`todo`도 없어 히스토리·아카이브·복제도 깨짐 |
| 제안된 수정 | 불충분 | 인증이 빠져 401. 이벤트 형식, 프롬프트 본문과 모델 지정, 완료 판정, 권한 멈춤, `run` CLI 변화가 빠짐 |
| 출시 정보(2.0.20, 2026-10-02) | 확인 못 함 | GitHub에는 v2 태그만 있고 공개 릴리스가 없음. Homebrew core 최신은 2.0.20 |

이 문제는 opencode 버전이 아니라 cokacdir 버전에 따라 갈립니다. 배포된 v0.8.30은 최신 2.0.24에서도 같은 오류가 나고, 작업 트리를 빌드한 cokacdir에서 해결됩니다(2.0.24 기준 검증).

## 휴리스틱 조사와 재구현

opencode 연동 코드에서 시간이나 추측으로 판단하던 곳을 찾아, opencode가 실제로 제공하는 확정 신호로 바꿨습니다. 대체 신호는 먼저 실측으로 확인했습니다.

- **텍스트 이벤트의 `messageID`:** DB의 assistant 메시지 ID와 같습니다.
- **`opencode debug paths`:** 0.1~0.4초 만에 DB 경로를 알려 주고, `XDG_DATA_HOME`과 `OPENCODE_DB`를 반영합니다.
- **백그라운드 보고:** subagent가 끝나고 4ms 뒤 부모가 다시 활성화되며, 보고는 30ms 안에 기록에 들어갑니다.

| # | 이전 휴리스틱 | 바뀐 판단 근거 | 위치 |
| --- | --- | --- | --- |
| 1 | legacy: 텍스트가 조금이라도 나왔으면 오류 이벤트를 무시. 텍스트 뒤에 실패하면 "Process exited with code 1"만 보임 | v2 `run`의 종료 코드. 0이면 오류는 회복된 것, 0이 아니면 마지막 오류 메시지를 표시 | `opencode.rs` legacy |
| 2 | legacy: 정상 종료면 최종 답으로 간주. 잘리거나 필터된 답도 영구 기억에 저장될 수 있음 | DB에 기록된 그 메시지의 `finish`. 턴을 `idle(succeeded)`로 닫은 마지막 assistant가 `stop`일 때만 최종 답 | `opencode_v2_message_is_final_answer` |
| 3 | serve: 턴이 끝난 뒤 SSE 마무리 대기를 고정 500ms | 기록의 `idle` 개수만큼 부모의 실행 종료 이벤트를 받았는지. 이벤트 순서상 그 앞의 텍스트는 모두 받은 것(상한 2초, 넘으면 기록으로 보정) | `opencode_v2.rs` |
| 4 | legacy `/stop`: SIGINT 후 고정 1초 대기 | `run`의 출력이 닫히는 것(=프로세스 종료)을 신호로 대기(상한 3초) | `RunExitSignal` |
| 5 | DB 경로를 후보 목록에서 "처음 존재하는 파일"로 추측 | `opencode debug paths`가 보고하는 경로. 못 받으면 기존 후보로 대체 | `opencode_db_path` |
| 6 | 프롬프트가 30초 안에 기록에 안 나타나면 실패 | 부모 inbox에 대기 중이면 계속 기다림. inbox에도 기록에도 없으면(inbox를 먼저 읽음) 버려진 것 | `poll_until_settled` |
| 7 | 백그라운드 보고가 30초 안에 안 오면 기다리지 않음 | 보고가 inbox에 대기 중이거나 자식이 실행 중이면 대기. 아무것도 실행·대기하지 않는데 보고가 없는 상태가 연속 관측되면 오지 않는 것 | `poll_until_settled` |
| 8 | HTTP 오류 6회 연속이면 서버 사망으로 간주 | 서버 프로세스의 종료를 직접 확인(`try_wait`), 종료 상태를 오류에 표시 | `Server::exit_status` |

모든 반복은 활성 세션 → 부모 inbox → 기록 순서로 읽습니다. 끝난 subagent의 보고는 inbox에 들어갔다가 기록으로 옮겨지므로, 이 순서로 읽으면 이동 중인 작업을 놓치지 않습니다.

검증 결과입니다.

- **단위 테스트:** 45개 통과. `opencode debug paths` 파싱과 종료 신호 테스트를 추가했습니다.
- **판정 함수:** 실제 DB의 다섯 경우(도구 단계, 턴을 닫은 `stop`, 중단된 턴, 백그라운드 중간 답, 보고 뒤 최종 답)를 모두 맞게 판정했습니다.
- **serve e2e 8개:** 단순 응답, 권한, 질문, 백그라운드 subagent 1개와 2개, 없는 모델, `/stop`, 시스템 프롬프트 모두 통과했습니다. 마무리 대기는 상한에 한 번도 걸리지 않았고 정착 후 0.1초 안에 끝났습니다.
- **legacy e2e:** 최종 답 판정, 오류 메시지, `/stop`(약 0.3초 만에 정리)을 확인했습니다. 잘못된 모델 시나리오는 하네스 기준상 FAIL로 나오는데, 첫 단계가 시작되지 않아 Init 이벤트가 없기 때문입니다. 표시되는 오류 메시지는 정확하고 수정 전과 같습니다.
- **확인하지 못한 것:** 출력 한도로 잘린 답(`length`)은 OpenAI 모델에 한도 설정이 적용되지 않아 실제로 만들지 못했습니다. 판정 규칙은 위 다섯 경우로 확인했습니다.

남겨 둔 것과 이유입니다(판단 근거가 아니라 상한이거나, 대체할 확정 신호가 없음).

- **연속 2회 관측(500ms 간격):** opencode에는 "턴이 정착했다"를 한 번에 답하는 API가 없어, 여러 요청에 걸친 관측이 안정적인지 확인하는 용도로 유지했습니다.
- **HTTP 오류 연속 상한:** 프로세스는 살아 있는데 응답하지 않는 서버를 위한 것입니다. 죽은 서버는 이제 직접 감지합니다.
- **`opencode models`의 빈 목록 재시도:** opencode 쪽의 초기화 경쟁이고, "모델 준비 완료" 신호가 없습니다.
- **verify의 `mission_complete` 문자열 판정:** Claude 경로와 공유하는 설계라 바꾸지 않았습니다.
- **스트리밍 텍스트가 어긋났을 때 보정 덧붙이기:** 이미 보낸 텍스트는 되돌릴 수 없어서 하는 표시상 보정이고, 최종 결과(`Done`)는 기록 기준입니다.
- **각종 타임아웃(설정 요청 180초, 모델 목록 30초 등):** 무한 대기를 막는 상한일 뿐, 판단 근거가 아닙니다.
- **opencode 1.x 경로와 쓰이지 않는 `execute_command`:** 기존 휴리스틱을 그대로 두었습니다.

## 실측하지 않은 항목과 남은 작업

이전 보고에서 빈틈으로 남겼던 항목(바이너리 e2e, 단위 테스트, DB 쿼리 실행, 페이지 넘김, compaction, 병렬 subagent, MCP elicitation, 동시 실행, 긴 턴)은 위에서 모두 검증했습니다. 남은 항목은 다음과 같습니다.

| 항목 | 상태 | 이유 |
| --- | --- | --- |
| 보고가 끝내 오지 않는 백그라운드 subagent(30초 후 대기 중단) | 코드 검토만 | 실제로 재현할 방법이 없음 |
| 중첩 subagent | 해당 없음 | 기본 general·explore 에이전트가 `subagent`를 deny |
| 스트리밍 중 프로바이더 오류와 재시도 | 미실측 | 재현 수단 없음. 오류가 붙은 assistant 메시지 처리는 단위 테스트로 확인 |
| opencode 2.0.20 (리포트 버전) | 중단 | 최신 버전만 대상으로 하기로 함 |
| macOS·Windows | 미실측 | 해당 OS 없음. Windows용 v2는 공개 배포처(GitHub 릴리스·npm)도 확인되지 않음 |
| `opencode models` 첫 호출의 빈 결과 원인 | 원인 미상 | 재시도로 대응됨 |

그 밖에 보고만 하는 점이 있습니다(설계 판단이 필요해 고치지 않음).

- cokacdir는 opencode를 `which`와 `bash -lc which`로만 찾습니다. 설치 위치 `~/.opencode/bin`이 `.bashrc`·`.zshrc`에만 등록된 환경에서는, cokacdir 프로세스 PATH에 그 경로가 없으면 opencode를 찾지 못합니다. 1.x 때부터 있던 동작입니다.
- verify는 같은 디렉터리에 fork 세션을 남기므로, 저장된 세션 없이 "디렉터리의 최신 세션"을 고르는 경우(봇 재시작 시 자동 복원, `/start`·`/cd`) 원래 대화 대신 verify fork가 잡힐 수 있습니다. 1.x와 Claude 쪽도 같은 구조입니다.
- 저장된 세션이 opencode에서 지워지면 `/clear` 전까지 턴마다 "session not found"가 반복됩니다. 모든 제공자에 공통입니다.
- 2.x의 `opencode models`는 사용자의 opencode 백그라운드 서비스가 없으면 자동으로 시작합니다. 1.x에는 없던 부수 효과입니다.
- 플러그인 첫 설치처럼 오래 걸리는 첫 프롬프트를 결정적으로 재현할 수단은 없었습니다. 대신 타임아웃 메커니즘을 로컬 서버로 확인했습니다.

실측 중 opencode DB에 남은 테스트 세션 112개(모두 스크래치 경로)는 이후 모두 삭제했습니다. 다른 세션은 건드리지 않았습니다.
