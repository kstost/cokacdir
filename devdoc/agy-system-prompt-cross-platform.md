# Agy 시스템 프롬프트의 교차 플랫폼 전송 구현 기록

이 문서는 cokacdir의 Agy(Antigravity CLI) 프로바이더가 시스템 프롬프트를
Linux, macOS, Windows에서 같은 방식으로 전달하도록 바꾼 조사·설계·구현
과정을 기록한다.

가장 중요한 질문은 하나였다.

> Agy 세션을 이어서 사용할 때, 같은 cokacdir 시스템 프롬프트가 실제 모델에
> 전달되는 context 안에 계속 누적되어 중복되는가?

결론부터 말하면 **그렇지 않다**. Agy의 SQLite 대화 파일에는 과거
`ephemeralMessage` step이 이력으로 여러 개 남을 수 있지만, 그것은 모델에
매번 다시 전달되는 유효 context와 동일하지 않다. cokacdir는 현재 사용자
입력만 stdin으로 보내고, 현재 시스템 프롬프트는 모델 호출마다 Agy의
`PreInvocation` 훅을 통해 transient system message로 한 번 주입한다.

이번 변경으로 이 전송 방식은 Linux뿐 아니라 macOS와 Windows에서도 동일하게
사용된다. 세 운영체제에는 시스템 프롬프트를 사용자 stdin에 합치는 fallback이
없다.

---

## 1. 작업 메타데이터와 범위

| 항목 | 값 |
|---|---|
| 조사 및 구현 기준일 | 2026-07-13 |
| 기준 커밋 | `e178b2624a46ea279e61c33fe03249a98730110b` |
| 기준 Agy 버전 | `1.1.1` |
| 주 구현 파일 | `src/services/agy.rs` |
| 사용자 문서 | `docs/how-to-use-agy-antigravity.md` |
| 웹 문서 | `website/src/components/docs/sections/AgyProvider.tsx` |
| 대상 운영체제 | Linux, macOS, Windows |
| 명시적 검증 제한 | 사용자 요청에 따라 `cargo check`, `cargo test`, `cargo build` 생략 |

작업 시작 시 기준 커밋 이후의 worktree는 깨끗했다. 이 문서에 설명된 변경은
현재 커밋되지 않은 작업으로 남아 있으며, 기존 사용자 변경과 섞인 상태에서
시작한 것이 아니다.

이번 작업의 범위는 다음과 같다.

- 시스템 프롬프트와 사용자 요청의 전송 채널을 분리한다.
- Linux에서 이미 사용하던 Agy `PreInvocation` 경로를 macOS와 Windows에도
  적용한다.
- 세션을 resume하더라도 시스템 프롬프트가 유효 모델 context에 누적되지 않는
  구조를 유지한다.
- Agy의 fail-open 훅 동작으로 인해 검증되지 않은 응답이 사용자에게 노출되지
  않도록 한다.
- Windows 파일 잠금 의미에 맞는 임시 파일 수명주기와 crash 정리를 구현한다.
- 실제 저장 DB와 실제 모델 context를 혼동하지 않도록 문서화한다.

다음 항목은 범위 밖이다.

- Agy 자체의 hook dispatcher 또는 serializer 수정
- Agy가 provider로 보내는 원시 HTTP 요청 본문의 packet capture
- macOS/Windows 실기기에서의 authenticated live model 호출
- Agy의 plaintext conversation DB를 비밀 저장소로 바꾸는 작업
- 같은 사용자 권한으로 실행되는 Agy 도구 프로세스에 대한 보안 경계 제공

---

## 2. 최종 불변식

구현이 유지해야 하는 조건을 먼저 고정했다.

### 2.1 입력 분리

Linux, macOS, Windows에서는 다음 두 입력이 분리되어야 한다.

```text
현재 사용자 요청 ────────────────────────────────> Agy non-TTY stdin

현재 전체 시스템 프롬프트
  └─> ~/.cokacdir/tmp/agy_system_prompt_<random>
       └─> PreInvocation helper
            └─> {"injectSteps":[{"ephemeralMessage":"..."}]}
                 └─> 현재 모델 호출의 transient system message
```

stdin에는 시스템 프롬프트의 복사본이 들어가면 안 된다.

### 2.2 호출당 한 번의 현재 프롬프트

`ephemeralMessage`는 한 번의 모델 호출에 필요한 transient step이다. Agy가
도구 호출 뒤 모델을 다시 부르면 `PreInvocation`도 다시 실행되므로, helper는
그때마다 **현재 전체 시스템 프롬프트 하나**를 반환한다.

과거 호출에서 저장된 ephemeral DB row를 cokacdir가 다시 읽어 stdin이나 새
hook 응답에 합치지 않는다.

### 2.3 검증되지 않은 출력 차단

Agy 1.1.1은 hook 오류를 fatal로 처리하지 않는 fail-open 동작을 보인다. 따라서
cokacdir는 다음 조건을 모두 확인하기 전까지 Agy stdout을 사용자에게 전달하지
않는다.

- 적어도 한 번의 훅 실행이 시작되었다.
- helper가 JSON 응답을 stdout에 쓰고 flush한 뒤 acknowledgement를 남겼다.
- ledger의 모든 `start`에 대응하는 `ok`가 있다.
- `fail`, 손상된 line, 완료되지 않은 `start`가 없다.
- Agy 자식 프로세스가 종료했다.

### 2.4 활성 실행 파일 보호

다른 cokacdir 프로세스가 시작되거나 이전 프로세스가 crash했더라도 다음이
보장되어야 한다.

- 살아 있는 실행의 prompt/state/ack 파일은 삭제하지 않는다.
- crash로 남은 파일은 다음 실행이 정리할 수 있다.
- 같은 pathname이 다른 파일로 바뀌면 그 대체 파일을 삭제하지 않는다.
- symlink 또는 Windows reparse point를 정상 임시 파일로 취급하지 않는다.

### 2.5 일반 Agy 실행에 대한 무해성

전역 plugin은 cokacdir 전용 환경변수가 없는 평범한 Agy 실행에서는 hook stdin을
소비하고 `{}`만 반환해야 한다. 이 경우 시스템 메시지를 주입하지 않는다.

---

## 3. 조사 과정

### 3.1 대상 세션 식별

사용자가 Agy에 보낸 다음 질문이 조사 기준점이었다.

```text
너는 누구인지 한문장으로 응답해줘,
```

`~/.cokacdir/debug/ai_trace.log`, `agy.log`, `msg.log`를 교차 검색해 해당 요청과
연결된 Agy conversation을 찾았다. 저장 위치는 다음 형식이었다.

```text
~/.gemini/antigravity-cli/conversations/<conversation-id>.db
~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript.jsonl
```

문서에는 불필요한 채팅 식별자, bot key, 사용자별 절대 경로를 남기지 않는다.

### 3.2 cokacdir 로그에서 확인한 입력 길이

동일 세션의 세 번의 사용자 요청에서 다음 값이 기록되어 있었다.

| 시각 | 실행 형태 | stdin에 쓴 사용자 prompt | 별도 system prompt | 관측 결과 |
|---|---:|---:|---:|---|
| 18:04:56 | 새 conversation | 49 bytes | 7,362 bytes | stdin 기록은 49 bytes |
| 18:05:23 | resume | 68 bytes | 7,863 bytes | stdin 기록은 68 bytes |
| 18:14:39 | resume | 45 bytes | 7,863 bytes | stdin 기록은 45 bytes |

각 실행 로그에는 `hook_plugin=Some(...)`도 기록되어 있었다. 이는 Linux 실측
경로에서 다음을 직접 보여준다.

1. 시스템 프롬프트 길이는 별도로 계산되었다.
2. 실제 Agy stdin에는 현재 사용자 prompt 길이만큼만 기록되었다.
3. 시스템 프롬프트는 stdin 뒤에 붙지 않았다.
4. resume에서도 같은 분리 방식이 유지되었다.

즉 Linux에서 이미 “사용자 stdin + 별도 hook system step” 구조가 실제 실행되고
있었다.

### 3.3 SQLite conversation DB에서 확인한 저장 형태

대상 DB의 `steps` 테이블에는 총 16개 row가 있었다. 전체 개요는 다음과
같았다.

| `idx` | `step_type` | payload bytes | 조사상 의미 |
|---:|---:|---:|---|
| 0 | 14 | 450 | 사용자 입력 계열 |
| 1 | 90 | 7,530 | 첫 번째 hook system step |
| 2 | 98 | 225 | 첫 generation 관련 step |
| 3 | 15 | 658 | 모델 응답 계열 |
| 4 | 23 | 588 | checkpoint/history 계열 |
| 5 | 14 | 675 | 다음 사용자 입력 |
| 6 | 90 | 8,035 | 두 번째 hook system step |
| 7 | 101 | 1,144 | generation 관련 step |
| 8 | 15 | 5,256 | 모델 응답 계열 |
| 9 | 14 | 627 | 다음 사용자 입력 |
| 10 | 90 | 8,033 | 도구 뒤 추가 모델 호출용 hook system step |
| 11 | 101 | 1,141 | generation 관련 step |
| 12 | 15 | 1,776 | 모델 응답 계열 |
| 13 | 8 | 8,610 | 도구 결과 계열 |
| 14 | 90 | 8,033 | 다음 모델 호출용 hook system step |
| 15 | 15 | 1,029 | 모델 응답 계열 |

`step_type=90`인 1, 6, 10, 14번 payload를 protobuf raw decode했을 때 각 row 안에
그 시점의 전체 cokacdir 시스템 프롬프트가 들어 있었다.

따라서 “DB 파일 안에 같은 종류의 시스템 프롬프트가 여러 번 존재하는가?”라는
질문의 답은 **예**다. 하지만 이 row들은 호출 이력이며, 그 사실만으로 “다음
provider 요청 context에 네 개가 모두 들어갔다”고 결론 내릴 수는 없다.

### 3.4 generation metadata와 transcript의 차이

`gen_metadata`에는 네 개의 generation 기록이 있었고, decode된
`last_step_index`는 다음과 같았다.

| generation metadata index | `last_step_index` |
|---:|---:|
| 0 | 2 |
| 1 | 7 |
| 2 | 11 |
| 3 | 14 |

각 generation 경계에는 새 `step_type=90` ephemeral row가 하나씩 연결된다. 도구
호출로 한 사용자 턴 안에서 모델이 다시 호출될 때도 새 ephemeral step 하나가
생겼다.

반면 같은 conversation의 일반 `transcript.jsonl`에는 DB의 ephemeral row 인덱스
1, 6, 10, 14가 나타나지 않았다. transcript에는 사용자 입력, checkpoint,
모델 응답, tool call/result 등 대화에 보이는 step만 기록되었다.

이 차이는 다음 두 저장층을 구분해야 함을 보여준다.

- SQLite `steps`: 내부 실행 이력을 포함하는 보존 저장소
- 일반 transcript 및 generation 입력: 현재 호출에 필요한 대화 표현

### 3.5 Agy 공식 hook 계약

[Agy Hooks 공식 문서](https://antigravity.google/docs/hooks#preinvocation)는
`PreInvocation`을 모델 호출 전에 실행되는 event로 정의한다.

입력의 핵심 필드는 다음과 같다.

- `invocationNum`: 현재 모델 invocation의 0-based 번호
- `initialNumSteps`: hook 주입 전 trajectory step 수
- `conversationId`: 현재 conversation 식별자

출력의 `injectSteps`에는 다음 종류를 넣을 수 있다.

- `toolCall`
- `userMessage`
- `ephemeralMessage`

공식 문서는 `ephemeralMessage`를 **transient system message**로 명시한다.
cokacdir가 선택한 전송은 이 공개 계약을 그대로 사용한다.

### 3.6 Linux, macOS, Windows Agy 1.1.1 바이너리 대조

[Agy 1.1.1 공식 release](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.1.1)의
플랫폼 바이너리와 로컬 Linux 바이너리를 정적으로 대조했다.

| 플랫폼 | 형식/아키텍처 | SHA-256 |
|---|---|---|
| Linux | ELF, ARM64 | `a22a4937afd882e33b5edf2c722f5d3050166ff5c464237bd130383866ef3c80` |
| macOS | Mach-O, ARM64 | `f3b54437e56d81bb5c47715ed9bfb9a924c584f37524e87790e562399e23d7e0` |
| Windows | PE32+, x86-64 | `a78d9d170133a993d055604ee4bba1a0cb1cec84989bdbf6da5477779df9ff37` |

세 바이너리에서 공통으로 다음 계약과 serializer 표면을 확인했다.

- `PreInvocationHookArgs`
- `PreInvocationHookResult`
- `injectSteps`
- `DefaultSerializer.ephemeralMessageToMessage`
- hook command 실행 경로

Agy 1.1.1의 platform guide는 Unix에서 hook command를 `sh -c`, Windows에서
`cmd /c`로 실행하는 구조를 포함한다. 따라서 cokacdir plugin의 `command` 필드는
macOS/Linux용 POSIX shell 문법과 Windows용 cmd 문법을 각각 생성해야 한다.

### 3.7 실제 모델 context 중복 여부에 대한 증거 수준

확정할 수 있는 사실은 다음과 같다.

1. 공식 계약은 `ephemeralMessage`를 transient system message로 정의한다.
2. Linux 실측에서 stdin에는 사용자 요청만 들어갔다.
3. 각 model invocation에는 새 ephemeral step 하나가 연결됐다.
4. SQLite에는 과거 ephemeral row가 보존됐다.
5. 일반 transcript에서는 그 row들이 제외됐다.
6. Agy의 세 플랫폼 바이너리가 같은 hook/serializer 계약을 포함한다.

이 증거를 종합하면 SQLite의 과거 row는 다음 invocation에 추가 system message
복사본으로 재생되는 것이 아니며, 유효 모델 context는 현재 hook이 반환한 시스템
프롬프트를 한 번 받는다고 판단할 수 있다.

다만 Agy는 최종 provider HTTP 요청 본문을 공개하지 않는다. 따라서 이 결론은
공식 transient 계약과 session/generation trace에 근거하며, TLS를 해제한 원시
network payload capture에 근거한 것은 아니다. 문서와 사용자 응답에서 이 증거
범위를 넘는 표현은 사용하지 않는다.

---

## 4. 변경 전 구현과 발견된 공백

기준 커밋의 Linux 경로에는 이미 상당한 안전장치가 있었다.

- per-run prompt 파일
- 전역 namespaced plugin
- `PreInvocation` helper
- random acknowledgement token
- `start`/`ok`/`fail` ledger
- 30초 hook timeout
- Agy 전체 stdout 보류
- child kill/reap wrapper
- stable file identity 검사

문제는 이 구현이 Linux에만 선택되어 있었다는 점이다.

```rust
#[cfg(target_os = "linux")]
let agy_hook_prompt = prepare_agy_hook_prompt(...);

#[cfg(not(target_os = "linux"))]
let stdin_prompt = build_legacy_agy_stdin_prompt(prompt, system_prompt);
```

비-Linux fallback은 다음 텍스트를 하나의 사용자 stdin prompt로 만들었다.

```text
SYSTEM INSTRUCTIONS:
<full system prompt>

USER REQUEST:
<current user request>
```

이 방식에는 세 가지 문제가 있었다.

1. 시스템 지시가 system role이 아니라 사용자 메시지의 일부가 된다.
2. resume할 때마다 전체 시스템 프롬프트가 새 사용자 turn에 다시 저장될 수 있다.
3. Linux와 macOS/Windows의 session 의미가 달라진다.

또한 기존 Windows wrapper는 helper만 실행했으며 Unix wrapper와 같은
`start`/`ok`/`fail` ledger를 기록하지 않았다.

마지막으로 기존 임시 파일 보호는 prompt와 state 파일 자체에 exclusive advisory
lock을 걸었다. Unix advisory lock은 다른 프로세스의 일반 read/write를 막지 않지만,
Windows `LockFileEx` exclusive lock은 다른 handle의 접근을 실제로 방해한다. 이
구조를 그대로 Windows에서 활성화하면 hook child가 prompt를 읽거나 ledger에 쓰지
못할 수 있었다.

따라서 단순히 `cfg(target_os = "linux")`를 넓히는 것만으로는 충분하지 않았다.

---

## 5. 대안 검토와 결정

| 대안 | 장점 | 문제 | 결정 |
|---|---|---|---|
| macOS/Windows에서 combined stdin 유지 | 구현 변경이 작음 | system/user 역할 혼합, resume마다 복사본 저장 가능, 플랫폼 의미 불일치 | 기각 |
| 첫 model call에만 hook prompt 주입 | step 수가 적음 | ephemeral은 호출 단위이므로 tool loop의 다음 model call에 지시가 없음 | 기각 |
| `AGENTS.md` 또는 workspace rule 파일에 기록 | Agy가 읽을 가능성 높음 | 사용자 프로젝트 변경, 기존 규칙과 충돌, 정리 실패 시 지속 | 기각 |
| 시스템 프롬프트를 CLI 인자로 전달 | 구현이 단순해 보임 | Agy 1.1.1에 측정된 별도 system-role 인자 없음, argv 노출 | 기각 |
| prompt/state 파일을 계속 exclusive lock | 기존 Linux 코드 재사용 | Windows child 접근 차단 | 기각 |
| 파일 잠금을 전부 제거 | child 접근이 쉬움 | crash residue와 활성 실행을 구분할 수 없음 | 기각 |
| 별도 lease 파일을 shared lock | child가 prompt/state 사용 가능, cleanup은 live/stale 판별 가능 | cleanup mapping 로직 필요 | 채택 |
| Agy 출력을 즉시 streaming | 응답 지연이 적음 | 나중 model call의 hook 실패 전에 부분 답변 노출 | 기각 |
| hook이 있는 실행의 stdout 전체 보류 | fail-open 결과 노출 방지 | 응답은 프로세스 종료 뒤 한꺼번에 보임 | 채택 |

최종 결정은 다음 한 문장으로 요약된다.

> Linux, macOS, Windows에서 사용자 stdin과 transient system hook을 동일하게
> 사용하고, hook 보장은 별도 lease·ledger·acknowledgement·stdout gating으로
> fail-closed에 가깝게 감싼다.

---

## 6. 최종 실행 흐름

시스템 프롬프트가 있는 Agy 호출은 다음 순서로 진행된다.

1. `~/.cokacdir/tmp/`의 directory-wide cleanup lock을 잡는다.
2. 이전 crash에서 남은 stale Agy hook 파일을 정리한다.
3. 현재 시스템 프롬프트를 random private prompt 파일에 쓴다.
4. 빈 ledger state 파일을 만든다.
5. prompt/state basename을 담은 lease 파일을 만든다.
6. lease 파일에 shared lock을 잡고 실행이 끝날 때까지 유지한다.
7. random 128-bit token과 ack pathname을 준비한다.
8. namespaced global Agy plugin을 확인하거나 원자적으로 갱신한다.
9. Agy child environment에 prompt/state/executable/token을 전달한다.
10. stdin에는 현재 사용자 요청만 쓰고 pipe를 닫는다.
11. Agy가 `PreInvocation`을 실행한다.
12. shell wrapper가 ledger에 `start <token>`을 append한다.
13. wrapper가 같은 cokacdir executable의 private hook entry point를 실행한다.
14. helper가 hook JSON과 prompt 파일을 검증한다.
15. helper가 `injectSteps/ephemeralMessage` JSON을 stdout에 쓰고 flush한다.
16. helper가 ack 파일을 `create_new`로 만들고 token을 sync한다.
17. wrapper가 성공 시 `ok <token>`, 실패 시 `fail <token>`을 append한다.
18. parent는 ack와 complete ledger를 확인하고, 출력 전달을 보류한 채 Agy를 계속 감시한다.
19. Agy가 추가 model call을 하면 11~17이 다시 수행된다.
20. 모든 stdout은 메모리에 보류된다.
21. Agy child 종료 뒤 최종 ledger와 ack를 다시 검사한다.
22. 완전한 경우에만 Text/Done을 전달한다.
23. child를 reap한 다음 ack, lease, prompt, state 파일을 제거한다.

시스템 프롬프트가 없거나 whitespace뿐이면 hook 파일과 plugin 환경을 만들지 않고
기존 Agy streaming 동작을 유지한다.

---

## 7. 코드 변경 상세

### 7.1 platform gate 통일

다음 세 영역의 gate를 `target_os = "linux"`에서 `any(unix, windows)`로 바꿨다.

- `prepare_agy_hook_prompt`
- 사용자 전용 `stdin_prompt`
- plugin 및 hook executable 준비

그 결과:

- Linux: Unix hook 사용
- macOS: Unix hook 사용
- Windows: Windows hook 사용
- 그 밖의 비-Unix·비-Windows target: 기존 combined-stdin compatibility 경로 유지

이번 요구의 대상인 세 운영체제에는 combined-stdin fallback이 없다.

### 7.2 namespaced global plugin

plugin 위치는 다음과 같다.

```text
~/.gemini/config/plugins/cokacdir-runtime-system-prompt/plugin.json
~/.gemini/config/plugins/cokacdir-runtime-system-prompt/hooks.json
~/.gemini/config/plugins/cokacdir-runtime-system-prompt/.cokacdir-owned
```

설치 과정은 다음을 검사한다.

- plugin parent가 real directory인지
- lock file이 stable regular file인지
- Windows reparse point가 아닌지
- 기존 plugin directory가 cokacdir marker로 소유 확인됐는지
- `plugin.json.disabled`가 존재해 사용자가 명시적으로 끈 상태가 아닌지

plugin JSON은 private atomic write로 갱신한다. 평범한 Agy 실행에는 cokacdir 전용
환경변수가 없으므로 wrapper가 `{}`를 반환하고 아무 step도 주입하지 않는다.

### 7.3 per-run 파일

| 파일 | 내용 | Unix mode | 수명 |
|---|---|---:|---|
| `agy_system_prompt_<random>` | 현재 전체 시스템 프롬프트 | `0600` | Agy child 종료까지 |
| `agy_hook_state_<random>` | `start`/`ok`/`fail` ledger | `0600` | Agy child 종료까지 |
| `agy_hook_lease_<random>` | prompt/state basename 두 줄 | `0600` | Agy child 종료까지 |
| `agy_system_prompt_<random>.ack` | 현재 random token | `0600` on Unix | helper flush 이후부터 종료까지 |
| `.agy-hook-cleanup.lock` | process 간 cleanup 직렬화 | `0600` on Unix | 준비 구간 동안 |

random suffix는 128-bit 난수의 32자리 hex다. 시스템 프롬프트 최대 크기는
16 MiB로 제한한다.

### 7.4 child 전용 환경

| 환경변수 | 값 |
|---|---|
| `COKACDIR_AGY_SYSTEM_PROMPT_FILE` | prompt 파일 경로 |
| `COKACDIR_AGY_SYSTEM_PROMPT_TOKEN` | 32자리 random token |
| `COKACDIR_AGY_HOOK_EXECUTABLE` | 현재 cokacdir executable 경로 |
| `COKACDIR_AGY_HOOK_STATE_FILE` | ledger 파일 경로 |

Agy child를 만들기 전에 parent 환경에서 같은 이름을 먼저 제거하고, 현재 실행에
필요한 값만 다시 설정한다. 이렇게 하면 cokacdir 자체가 상위 shell에서 상속한
낡은 hook 환경을 새 실행에 전달하지 않는다.

### 7.5 Unix wrapper

Unix wrapper는 POSIX `sh` 문법을 사용한다.

```text
환경 없음:
  hook stdin을 소비하고 {} 출력

환경 있음:
  state/token 필수 확인
  umask 077
  state에 start token append
  cokacdir --internal-agy-pre-invocation-hook 실행
  성공이면 ok token, 실패면 fail token append
  helper exit status 반환
```

macOS와 Linux가 이 경로를 공유한다.

### 7.6 Windows wrapper

Windows wrapper는 Agy의 `cmd /c` 실행 방식에 맞춘 cmd 문법을 사용한다.

```text
if not defined EXEC (
  more >nul & echo {}
) else if not defined STATE (
  exit /b 125
) else if not defined TOKEN (
  exit /b 125
) else (
  append "start TOKEN"
  && run "EXEC" --internal-agy-pre-invocation-hook
  && append "ok TOKEN"
  || append "fail TOKEN" and exit /b 125
)
```

기존 Windows wrapper에 없던 `start`/`ok`/`fail` protocol을 추가해 Unix와 같은
검증 의미를 갖게 했다. path 환경변수는 quote해서 공백이 있는 Windows 경로를
보호한다.

### 7.7 private helper entry point

`main`은 일반 초기화보다 먼저 private argument를 검사한다.

```text
--internal-agy-pre-invocation-hook
```

이 경로는 TUI, bot state, 사용자 env override, 배포 문서를 초기화하지 않고 바로
hook helper로 들어간다.

helper는 다음을 검증한다.

- prompt path와 token이 함께 존재하는지
- token이 32자리 ASCII hex인지
- hook stdin이 유효한 JSON인지
- `invocationNum`이 unsigned integer인지
- `initialNumSteps`가 unsigned integer인지
- `conversationId`가 비어 있지 않은 string인지
- prompt 파일이 정확히 cokacdir private temp directory 아래에 있는지
- filename이 소유한 random prefix 형식인지
- prompt가 stable regular file인지
- prompt가 16 MiB 이하인지
- prompt가 UTF-8인지

성공 응답은 다음 형태다.

```json
{"injectSteps":[{"ephemeralMessage":"<complete current system prompt>"}]}
```

helper는 JSON과 newline을 stdout에 쓰고 flush한 **뒤에만** ack를 기록한다.

### 7.8 acknowledgement 동시성

Agy는 한 process 안에서 여러 model invocation이나 nested agent invocation을
겹쳐 실행할 수 있다. 여러 helper가 같은 token으로 같은 ack 파일을 만들려고 할
수 있으므로 ack write는 idempotent해야 한다.

구현은 다음 순서를 사용한다.

1. `create_new(true)`로 한 writer만 생성에 성공한다.
2. winner가 token을 쓰고 `sync_all`한다.
3. loser는 이미 파일이 있으면 최대 약 1초 동안 완전한 token을 기다린다.
4. 같은 token이면 성공으로 인정한다.
5. 다른 token, 과도하게 긴 내용, read 오류는 실패한다.

parent가 ack를 읽거나 제거할 때는 pathname의 filesystem identity를 읽기 전후로
확인한다. 제거는 identity-bound helper를 사용하므로 검사 뒤 pathname이 다른
파일로 바뀌면 대체 파일을 삭제하지 않는다.

### 7.9 ledger 상태 머신

허용되는 line은 현재 token에 대한 다음 세 종류뿐이다.

```text
start <token>
ok <token>
fail <token>
```

상태 판정은 다음과 같다.

| 조건 | 판정 |
|---|---|
| 파일이 비었거나 마지막 newline이 아직 없음 | `Pending` |
| 알 수 없는 line 또는 다른 token | `Failed` |
| `ok`가 대응하는 `start`보다 먼저/많이 등장 | `Failed` |
| `fail` 등장 | `Failed` |
| `start`가 하나도 없음 | `Failed` |
| 모든 `start`에 `ok`가 대응 | `Complete` |
| 일부 `start`에 아직 `ok`가 없음 | `Pending` |

“전체 line 수만 세기”가 아니라 순서대로 aggregate count를 검사하므로
`ok → start`처럼 만들어진 잘못된 ledger를 성공으로 오인하지 않는다.

### 7.10 stdout gating과 later invocation

첫 hook handshake가 성공했다고 즉시 stdout을 전달하면 충분하지 않다. Agy가
도구를 호출한 뒤 두 번째 `PreInvocation`에서 실패할 수 있기 때문이다.

따라서 시스템 프롬프트가 있는 실행에서는 다음 정책을 사용한다.

- 첫 ack + complete ledger를 최대 30초 기다린다.
- stdout reader는 Agy 출력을 내부 buffer로 모은다.
- 실행 중 ledger를 반복 검사한다.
- 새 `start`가 생기면 `Pending`으로 돌아가고 별도 30초 completion timeout을 둔다.
- 어느 시점이든 `fail` 또는 손상이 보이면 process tree를 종료한다.
- Agy 종료 뒤 ack와 ledger를 다시 확인한다.
- 모두 정상일 때만 누적된 Text와 Done을 보낸다.

시스템 프롬프트가 없는 실행은 기존처럼 line streaming을 유지한다.

### 7.11 child reaping과 drop 순서

`ReapingAgyChild`는 모든 return path와 unwind에서 Agy child를 kill/reap한다.

hook prompt guard보다 child 변수가 나중에 생성되므로 Rust local drop 역순에 따라
child가 먼저 drop된다. 이 순서가 중요한 이유는 helper 또는 Agy descendant가
아직 prompt/state를 사용할 때 파일 guard가 먼저 사라지면 안 되기 때문이다.

`AgyHookPrompt` 내부 field 순서도 Windows를 위해 의도적으로 정했다.

```text
lease lock handle
lease PrivateTempFile guard
prompt guard
state guard
...
```

Rust struct field drop 순서에 따라 lease lock handle이 먼저 닫힌 다음 lease 파일을
삭제한다. Windows에서는 잠금 handle을 보유한 상태로 같은 파일에 delete
disposition을 설정하면 실패할 수 있으므로 이 순서가 필요하다.

---

## 8. Windows 호환을 위한 lease 설계

### 8.1 prompt/state를 직접 잠그면 안 되는 이유

기존 Linux 구현은 prompt와 state 각각에 exclusive advisory lock을 보유했다.
Unix `flock` 계열에서는 lock을 존중하지 않는 일반 read/write가 가능했기 때문에
hook child가 파일을 사용할 수 있었다.

Windows의 `LockFileEx`는 다르다. exclusive byte-range lock이 걸린 파일은 다른
handle의 read/write가 충돌할 수 있다. 따라서 동일 설계를 활성화하면 다음
deadlock과 유사한 실패가 생긴다.

```text
parent: prompt/state를 exclusive lock
child: prompt read 또는 state append 시도
child: lock 때문에 실패 또는 대기
parent: child hook completion 대기
```

### 8.2 별도 shared lease

해결책은 실제 payload 파일을 잠그지 않고 별도 lease 파일만 잠그는 것이다.

lease 내용은 path 전체가 아니라 검증 가능한 basename 두 줄이다.

```text
agy_system_prompt_<32 hex>
agy_hook_state_<32 hex>
```

parent는 lease에 shared lock을 보유한다. cleanup process는 같은 lease에
`try_lock_exclusive`를 시도한다.

- exclusive lock 실패/contended: live run
- exclusive lock 성공: stale lease

prompt는 child가 자유롭게 읽을 수 있고 state는 wrapper가 자유롭게 append할 수
있다.

### 8.3 platform별 contention error

단순히 `io::ErrorKind::WouldBlock`만 비교하면 Windows raw error를 놓칠 수 있다.
구현은 `fs2::lock_contended_error()`의 raw OS error와 실제 error를 우선 비교하고,
raw code가 없는 경우 error kind를 비교한다.

이는 Unix의 `EWOULDBLOCK` 계열과 Windows의 `ERROR_LOCK_VIOLATION`을 같은
“live lock” 의미로 처리한다.

### 8.4 cleanup의 두 단계 live mapping 수집

cleanup은 filename 순서에 따라 active 파일을 잘못 지우면 안 된다. 예를 들어
오래된 stale lease가 현재 live prompt/state 이름을 가리키면서 active lease보다
먼저 정렬될 수 있다.

그래서 알고리즘을 두 단계로 나눴다.

#### 8.4.1 모든 live mapping 수집

1. temp directory entry를 정렬한다.
2. 모든 `agy_hook_lease_<random>`을 검사한다.
3. shared lock 때문에 exclusive lock을 얻지 못한 lease를 live로 본다.
4. live lease의 검증된 prompt/state basename을 `live_files` set에 넣는다.
5. 이 단계에서는 아무 파일도 삭제하지 않는다.

#### 8.4.2 stale lease와 orphan 정리

1. stale lease에 exclusive lock을 얻는다.
2. lease 내용이 유효하면 매핑된 prompt/state를 본다.
3. `live_files`에 없는 파일만 identity-bound 방식으로 삭제한다.
4. stale lease 자체를 삭제한다.
5. lease 없이 남은 legacy prompt/state도 별도 lock 상태를 확인해 삭제한다.
6. prompt가 사라지고 ack만 남은 orphan ack를 정리한다.

이렇게 하면 stale lease가 live 파일을 가리켜도 첫 단계에서 수집한 전체 live set이
삭제를 막는다.

### 8.5 구버전 실행과의 공존

업데이트 순간에 이전 cokacdir process가 실행 중일 수 있다. 구버전은 lease가
없고 prompt/state 파일 자체를 exclusive lock한다.

새 cleanup은 lease만 보는 것이 아니라 prompt/state 개별 파일에도
`try_lock_exclusive`를 수행한다. lock이 contended면 legacy live run으로 보고
보존한다. 구버전 process가 끝난 다음 실행에서 정리된다.

### 8.6 Windows handle-bound deletion

stale 파일 삭제는 다음 순서다.

1. 현재 열린 file handle의 stable identity를 얻는다.
2. 같은 identity에 묶인 deletion handle을 준비한다.
3. byte-range lock을 가진 검사 handle을 닫는다.
4. 준비한 deletion handle로 삭제를 commit한다.

이 순서는 pathname 재검사와 실제 삭제 사이의 replacement race를 줄이고,
Windows에서 현재 process가 가진 lock 때문에 자기 삭제가 막히는 문제도 피한다.

---

## 9. 시스템 프롬프트 중복에 대한 최종 해석

### 9.1 저장 DB 안에서는 중복 row가 보인다

동일 conversation에서 model invocation이 네 번 일어나면 SQLite에는 hook이
주입한 system step row도 네 개 남을 수 있다. 각 row가 전체 프롬프트 문자열을
담고 있으므로 DB를 `strings`, SQLite query, protobuf raw decode로 보면 같은 내용이
여러 번 보이는 것이 정상이다.

이것은 “실행 이력이 여러 번 저장됐다”는 의미다.

### 9.2 유효 모델 context에는 과거 row가 누적되지 않는다

Agy가 문서화한 `ephemeralMessage`는 현재 invocation에 대한 transient system
message다. cokacdir helper도 현재 prompt 파일 하나만 읽어 `injectSteps` 한 개를
반환한다.

따라서 다음과 같은 모양이 아니다.

```text
invocation 1: system A
invocation 2: system A + system A
invocation 3: system A + system A + system A
```

의도하고 관측된 의미는 다음과 같다.

```text
invocation 1 effective context: current system A once
invocation 2 effective context: current system B once
invocation 3 effective context: current system B once
```

시스템 프롬프트가 turn 사이에서 바뀌면 새 실행은 새 private file과 새 token을
만들고 그 시점의 전체 프롬프트를 주입한다. 이전 프롬프트 파일을 재사용하지
않는다.

### 9.3 세 운영체제의 의미

- Linux: 위 구조를 Agy 1.1.1 실제 session으로 관측했다.
- macOS: Linux와 같은 Unix wrapper 및 Agy hook/serializer core를 사용한다.
- Windows: cmd wrapper만 다르고 동일한 `injectSteps/ephemeralMessage` core를
  사용한다.

운영체제별 차이는 hook command를 실행하는 shell과 filesystem lock/delete
구현이다. 모델 context를 구성하는 Agy protocol은 동일하다.

### 9.4 hook 미실행 시에는 “중복”이 아니라 “미주입” 위험이다

combined-stdin fallback을 제거했으므로 hook이 아예 실행되지 않은 경우 시스템
프롬프트의 두 번째 복사본이 stdin으로 들어가지는 않는다.

대신 Agy fail-open 특성상 prompt가 없는 model request가 detection 전에 시작될
가능성이 있다. cokacdir는 ack/ledger가 없으면 그 process를 종료하고 출력을
폐기한다. 즉 사용자에게 잘못된 답을 전달하지 않지만, 이미 시작된 provider
request나 tool side effect를 되돌릴 수는 없다.

---

## 10. 실패 시나리오와 처리

| 시나리오 | 감지 수단 | cokacdir 처리 |
|---|---|---|
| plugin 설치 실패/disabled | plugin 준비 오류 | Agy spawn 전 실패 |
| system prompt 16 MiB 초과 | prompt 생성 검증 | Agy spawn 전 실패 |
| hook이 전혀 dispatch되지 않음 | ack 없음, ledger 없음 | child 종료/30초 timeout 뒤 출력 폐기 |
| hook input JSON 손상 | helper parse 오류 | wrapper `fail`, parent kill/discard |
| 필수 PreInvocation field 누락 | schema 검증 | wrapper `fail`, parent kill/discard |
| prompt path가 temp 밖을 가리킴 | parent directory/name 검증 | helper 실패 |
| prompt/state pathname 교체 | stable identity 불일치 | hook state 실패 또는 read 거부 |
| helper가 JSON을 쓰기 전 종료 | ack 없음 | 응답 폐기 |
| helper 성공 뒤 wrapper `ok` 기록 실패 | incomplete/fail ledger | 응답 폐기 |
| 두 번째 model invocation hook 실패 | 추가 `start/fail` ledger | 이미 모은 stdout까지 전부 폐기 |
| ledger에 `ok`가 `start`보다 먼저 등장 | 순차 count 검사 | `Failed` |
| ledger line이 write 중 | 마지막 newline 없음 | 잠시 `Pending` |
| hook이 30초 이상 미완료 | pending timer | process tree 종료, 출력 폐기 |
| 여러 helper가 동시에 ack 생성 | create-new + same-token wait | 같은 token이면 idempotent 성공 |
| parent crash | lease lock 자동 해제 | 다음 실행이 stale residue 정리 |
| stale lease가 live 파일을 가리킴 | 전체 live mapping 선수집 | live 파일 보존, stale lease만 삭제 |
| 구버전 live process 존재 | legacy prompt/state lock contention | 현재 실행 파일 보존 |
| receiver가 사라짐/cancel | `ReapingAgyChild` | child tree kill/reap 후 파일 정리 |

---

## 11. 추가·변경된 회귀 테스트

테스트 코드는 작성했지만, 사용자 지시에 따라 이번 작업 중 `cargo test`로 실행하지
않았다.

| 테스트 | 검증 목적 |
|---|---|
| `hook_response_preserves_the_complete_utf8_system_prompt` | 큰 UTF-8 prompt가 한 개의 complete `ephemeralMessage`로 보존되는지 |
| `hook_response_requires_the_complete_pre_invocation_schema` | `invocationNum`, `initialNumSteps`, non-empty `conversationId` 필수 여부 |
| `private_hook_prompt_and_ack_are_removed_on_drop` | prompt/state/lease/ack 수명과 제거 |
| `hook_acknowledgement_is_idempotent_under_concurrent_writers` | 16개 동시 same-token writer 처리 |
| `private_hook_prompt_uses_owner_only_permissions` | Unix prompt/state/lease mode `0600` |
| `hook_ledger_detects_a_later_invocation_failure` | 앞선 성공 뒤 나중 hook 실패를 숨기지 않는지 |
| `hook_ledger_rejects_path_replacement` | ledger pathname 교체 감지 |
| `hook_ledger_rejects_success_before_start` | 잘못된 `ok → start` 순서 거부 |
| `stale_hook_cleanup_preserves_live_leases_and_removes_crash_residue` | live run 보존과 stale 파일 정리 |
| `stale_lease_cannot_remove_files_mapped_by_a_live_lease` | 정렬 순서/악성 stale mapping 방어 |
| `stale_cleanup_honors_legacy_prompt_and_state_locks` | 구버전 live process와 upgrade 공존 |
| `platform_lock_contention_error_is_recognized` | fs2 platform contention error 판정 |
| `windows_hook_wrapper_contains_complete_ledger_protocol` | Windows command의 start/ok/fail/125 구조 |
| `namespaced_hook_plugin_is_created_idempotently` | plugin 설치 재실행 안전성 |
| `global_hook_is_a_shell_level_noop_without_cokacdir_environment` | 일반 Agy 실행에서 `{}` no-op |
| `live_agy_streaming_round_trip` | 새 conversation + resume의 실제 hook 동작; ignored live test |

---

## 12. 이번 작업에서 실제 수행한 비-cargo 검증

사용자 요청대로 cargo 기반 검증과 빌드는 전혀 실행하지 않았다.

수행한 검증은 다음과 같다.

### 12.1 diff와 변경 범위

- `git diff --check`: 통과
- 최종 변경 파일 목록 확인
- 의도하지 않은 generated file 없음
- 작업 시작 전 기준 worktree가 clean이었음을 확인
- 이 개발 문서의 trailing whitespace, code-fence 짝, 제목 계층 검사 통과
- 사용자 문서에서 이 개발 문서로 연결한 상대 경로가 실제 파일을 가리키는지 확인

### 12.2 웹 문서 타입 검사

`website` directory에서 다음 검사가 통과했다.

```text
node_modules/.bin/tsc --noEmit
```

### 12.3 Unix wrapper 직접 shell probe

실제 POSIX shell에서 no-op, 성공, 실패 세 경로를 임시 파일로 실행했다.

관측 결과:

```text
noop={}
success=start <token>|ok <token>|
failure=start <token>|fail <token>|
failure_status=1
```

### 12.4 Rust formatting 도구

direct `rustfmt --check`를 시도했지만 현재 환경에는 `rustfmt` executable이
설치되어 있지 않았다. cargo를 통해 설치하거나 실행하지 않았으며, Rust source는
`git diff --check`와 수동 review로만 확인했다.

### 12.5 수행하지 않은 검증

- `cargo check`
- `cargo test`
- `cargo build`
- Windows target compile
- macOS target compile
- Windows 실기기 authenticated Agy invocation
- macOS 실기기 authenticated Agy invocation

따라서 “코드와 계약 수준 구현 완료”와 “세 플랫폼 binary를 실제 빌드하고 live
실행 완료”를 혼동하면 안 된다.

---

## 13. 플랫폼별 현재 검증 상태

| 항목 | Linux | macOS | Windows |
|---|---|---|---|
| cokacdir transport 선택 | hook | hook | hook |
| stdin 내용 | 현재 사용자 요청만 | 현재 사용자 요청만 | 현재 사용자 요청만 |
| wrapper | POSIX `sh` | POSIX `sh` | `cmd` |
| ledger | start/ok/fail | start/ok/fail | start/ok/fail |
| Agy 1.1.1 hook symbols 확인 | 예 | 예 | 예 |
| Agy 1.1.1 session DB 실측 | 예 | 아니오 | 아니오 |
| wrapper runtime probe | 예 | Linux POSIX shell로 문법/의미 대조 | 정적 command 구조만 확인 |
| authenticated live model probe | 기존 Linux trace | 미수행 | 미수행 |
| raw provider request body | Agy가 미노출 | Agy가 미노출 | Agy가 미노출 |

Windows/macOS hook dispatch와 관련해서는 [upstream issue
#222](https://github.com/google-antigravity/antigravity-cli/issues/222)가 열려 있다.
보고 내용은 오래된/다른 hook 유형과 PowerShell 또는 inline command를 포함하며,
이번 1.1.1 `PreInvocation` + compiled helper 경로와 정확히 같은 재현은 아니다.
그래도 플랫폼 live coverage가 필요하다는 근거로 계속 추적해야 한다.

---

## 14. 남아 있는 한계

### 14.1 Agy가 유효 응답을 실제 적용했는지는 외부에서 증명할 수 없다

helper가 올바른 JSON을 쓰고 Agy가 이를 읽었더라도, Agy가 내부적으로 그 응답을
무시하는 버그까지 ledger로 검출할 수는 없다. acknowledgement는 “helper가 JSON을
완전히 출력했다”는 사실을 증명할 뿐이다.

### 14.2 첫 성공 뒤 특정 later invocation만 조용히 skip될 수 있다

첫 hook은 성공했지만 Agy가 나중 model invocation에서 hook 자체를 전혀 dispatch하지
않는다면 새로운 `start`가 생기지 않는다. parent는 Agy 내부의 예상 invocation 수를
별도 채널로 알 수 없으므로 이 sporadic skip은 검출하지 못할 수 있다.

### 14.3 이미 시작된 side effect를 되돌릴 수 없다

Agy는 hook failure를 fail-open으로 처리할 수 있다. cokacdir가 30초 안에 process를
종료하더라도 그 전에 시작된 model request, tool call, 외부 API 요청, 파일 변경을
rollback할 수는 없다.

### 14.4 임시 파일은 같은 사용자에 대한 보안 경계가 아니다

Agy tool subprocess는 child environment를 상속할 수 있으므로 prompt path, state
path, token을 볼 수 있다. Agy에 full permissions를 주는 현재 모드에서 같은 사용자
권한의 코드를 이 transport로부터 격리한다고 주장하지 않는다.

### 14.5 conversation DB는 plaintext다

`ephemeralMessage`는 대화 UI와 정상 transcript에서 transient하지만, Agy 1.1.1은
그 payload를 SQLite conversation DB에 plaintext로 보존한다. 민감한 시스템
프롬프트를 암호화하는 기능이 아니다.

### 14.6 global no-op hook 비용

plugin이 한 번 설치되면 cokacdir 밖의 일반 Agy session에서도 model invocation마다
작은 no-op hook process가 실행될 수 있다. 환경이 없으면 메시지를 주입하지 않지만
process startup 비용은 남는다.

---

## 15. 후속 live 검증 체크리스트

macOS와 Windows에서 실제 배포 전 다음 항목을 각각 확인해야 한다.

1. Agy 1.1.1 또는 배포 대상 버전을 설치한다.
2. 해당 플랫폼용 cokacdir binary를 준비한다.
3. private live test override가 그 binary를 가리키게 한다.
4. 새 conversation에서 system prompt가 요구한 파일/응답이 생성되는지 확인한다.
5. 같은 conversation을 resume해 새 system prompt가 적용되는지 확인한다.
6. Agy stdin byte 수가 사용자 prompt byte 수와 같은지 debug log로 확인한다.
7. ledger에 호출별 `start`/`ok` pair가 남는지 확인한다.
8. 종료 후 prompt/state/lease/ack 파일이 제거되는지 확인한다.
9. 강제 종료 뒤 다음 실행이 stale residue를 정리하는지 확인한다.
10. session DB에서 과거 ephemeral row가 보존되는지 확인한다.
11. 일반 transcript에서 ephemeral row가 제외되는지 확인한다.
12. generation metadata가 호출별 현재 ephemeral step과 연결되는지 확인한다.
13. hook helper를 실패시키고 응답이 사용자에게 노출되지 않는지 확인한다.
14. tool call 뒤 두 번째 PreInvocation 실패도 전체 응답을 폐기하는지 확인한다.
15. 일반 Agy 실행에서 plugin이 `{}` no-op인지 확인한다.

Windows에서는 추가로 다음을 확인해야 한다.

- path에 공백과 비ASCII 문자가 있는 사용자 profile
- cmd wrapper의 quote 처리
- `LockFileEx` contention 판정
- shared lease가 있는 동안 prompt read/state append 가능 여부
- lock handle을 닫은 뒤 handle-bound deletion 성공 여부

---

## 16. 변경 파일별 역할

### `src/services/agy.rs`

- `any(unix, windows)` transport gate
- Windows complete ledger wrapper
- separate shared lease
- live/stale lock 상태 판정
- two-pass stale cleanup
- Windows handle-bound deletion 순서
- concurrent acknowledgement 처리
- complete PreInvocation schema 검증
- stricter ledger ordering
- 관련 unit/live test 추가 및 수정

### `docs/how-to-use-agy-antigravity.md`

- Linux/macOS/Windows 공통 hook transport 설명
- combined stdin fallback 제거 명시
- lease/ledger/ack 수명주기 설명
- DB 저장 이력과 모델 context 구분
- Linux 실측과 macOS/Windows 미실측 범위 구분
- fail-open 및 plaintext 저장 한계 설명

### `website/src/components/docs/sections/AgyProvider.tsx`

- 사용자 웹 문서를 Markdown 문서와 동일한 의미로 갱신
- 한국어/영어 양쪽에 platform parity와 중복 해석 추가
- live coverage와 fail-open 한계 표시

### `devdoc/agy-system-prompt-cross-platform.md`

- 이 조사·설계·구현·검증 기록

---

## 17. 최종 요약

이번 작업의 핵심은 단순한 `cfg` 변경이 아니다.

Linux에서 확인된 Agy의 공식 transient system-message 경로를 macOS와 Windows에도
적용하면서, Windows의 실제 파일 잠금 의미 때문에 깨질 수 있던 prompt/state
exclusive lock 구조를 별도 shared lease로 다시 설계했다. 동시에 Windows wrapper,
ack 동시성, ledger 순서 검증, stale cleanup의 filename-order race, identity-bound
삭제, 문서의 증거 수준까지 함께 보강했다.

최종적으로 세 운영체제의 전송 의미는 다음과 같다.

```text
stdin         = 현재 사용자 요청만
system input  = 현재 PreInvocation ephemeralMessage 한 개
old DB rows   = 저장 이력이며 다음 모델 context에 추가 복사본으로 재생되지 않음
hook failure  = combined fallback 없음, 응답 폐기
```

따라서 사용자가 가장 우려한 “resume할수록 동일 시스템 프롬프트가 실제 모델
context 안에 계속 중복 누적되는가?”에 대한 구현 및 조사상의 답은 **아니다**.
