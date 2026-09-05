# AGY 1.1.27 추가 실측 및 공식 자료 대조

검증일: 2026-09-05. 기준 소스: `9f784a1`과 현재 미커밋 변경.

아래 조사 본문은 **수정 전 실측**을 기록한다. 같은 날 후속 수정에서 구조화
응답 검증과 부모 프로세스의 전체 시간 제한을 구현했으며, 추가로 예약 세션
복제의 내부 ID 문제를 발견했다. 현재 구현과 재검증 결과는 마지막
[후속 수정](#후속-수정과-재검증) 절에 구분해 기록한다.

모델 목록 수정은 설치된 CLI의 출력과 일치했다. 추가 확인에서는 **세션이
사라졌을 때 잘못된 세션 ID로 성공을 보고하는 cokacdir 결함**을 재현했다.
또한 손상된 세션을 읽는 AGY가 자체 시간 제한을 넘겨 종료하지 않는 현상을
확인했다. 반면 이전 조사에서 언급한 일반적인 종료코드 0 오류와 이전 응답의
재출력은 이번 정상 경로 실측에서 재현되지 않았다.

## 환경과 방법

| 항목 | 확인값 |
|---|---|
| OS | Linux aarch64 |
| CLI | `/root/.local/bin/agy`, 버전 `1.1.27` |
| 공식 최신 릴리스 | `1.1.27`, 2026-09-05 04:23:25 UTC 공개 |
| 모델 목록 | 14개, JSON과 TSV의 ID·라벨이 전부 일치 |
| 실제 추론에 사용한 모델 | `gemini-3.8-flash-high` |
| 실행 방식 | 비TTY stdin에 입력 후 EOF, stdout/stderr 분리 저장 |
| 자동 업데이트 | 테스트 프로세스에만 `AGY_CLI_DISABLE_AUTO_UPDATE=true` |

[공식 릴리스](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.1.27)와
GitHub Releases API의 최신 태그를 모두 확인했다. 모델 14개의 목록을 확인한
것이며, 모든 모델에 추론 요청을 보낸 것은 아니다.

18개 CLI 사례를 독립된 테스트 디렉터리에서 실행했다. 각 사례에 명령 인자,
종료코드, 경과시간, stdout, stderr, 전용 AGY 로그를 저장했다. 별도로 실제
cokacdir 어댑터를 통한 세션 장애 주입을 수행했다. 사용자의 기존 세션·인증
설정은 수정하지 않았고, 장애 주입으로 이동한 테스트 세션 파일은 복구했다.

원본 측정 자료는 이 작업 환경의 다음 디렉터리에 있다.

```text
/tmp/cokac-agy-extra-audit-z_ayx7lk/
  probe.py                   # 기본 16개 사례
  followup.py                # 손상 세션, 단일 프로세스 2턴
  measurements.json          # 18개 사례의 원본 결과
  *.stdout / *.stderr        # 분리된 출력
  *.agy.log                  # 해당 사례의 AGY 로그
  agy-session-race            # 실제 어댑터의 세션 사라짐 장애 주입
  adapter-*.log / *.json      # 어댑터 검증 결과
```

이 경로의 원본 로그는 로컬 조사 자료이며 저장소에 포함하지 않는다.

## 실제 CLI 측정 결과

| 조건 | 출력 형식 | 종료코드 | 관측 결과 |
|---|---|---:|---|
| `models` | JSON / text | 각각 0 | 14개 항목 일치, 진행 안내는 stderr |
| 존재하지 않는 모델 | text / JSON / stream-json | 각각 1 | text는 stderr 오류, 구조화 출력은 `ERROR` |
| `--print ""`와 비어 있지 않은 stdin | text / JSON | 각각 1 | stdin을 사용하지 않고 빈 프롬프트 오류 |
| `--print-timeout 1ms` | text / JSON | 각각 1 | 응답 대기 시간 초과; 전체 실행은 약 3.8~4.2초 |
| 존재하지 않는 `--conversation` | JSON | 0 | stderr 경고 후 **새 ID로 새 대화**, `SUCCESS` |
| 첫 요청 → text 재개 → JSON 재개 | JSON / text | 모두 0 | 기억 토큰 유지, 3턴, 동일 ID, 과거 응답 재출력 없음 |
| 일반 stdin 입력 | stream-json | 0 | `init` 1개, `step_update` 3개, `result` 1개 |
| 입력 2개를 쓰고 즉시 EOF | stream-json 입출력 | 0 | `init` 1개, `result` 2개, 두 번째 턴이 첫 토큰 기억 |
| 오류처럼 보이는 문장을 그대로 출력 요청 | text | 0 | 정상 답변으로 `Error: timeout waiting for response` 출력 |
| `PreInvocation` helper를 `/bin/false`로 설정 | JSON | 0 | ledger는 `start/fail`, AGY는 응답과 `SUCCESS` 반환 |
| 의도적으로 손상시킨 테스트 `.db` 재개 | JSON | 외부 SIGKILL | 크래시 안내 후 정지; 20초 설정에도 45초까지 종료하지 않음 |

JSON 검증 실패는 `init` 없이 **`result` 이벤트 하나만** 나올 수도 있었다.
따라서 향후 stream-json 파서는 모든 실행이 `init`으로 시작한다고 가정하면
안 된다. 정상 2턴 실행에서는 마지막 stdin EOF 이후에도 두 번째 `result`가
전달됐다.

[공식 headless 문서](https://antigravity.google/docs/cli/headless/)의 출력 형식,
턴별 결과, 오류 상태와 비교했다. 일반 오류 7개 사례는 모두 비정상 종료로
처리됐다. 인증 만료·실제 할당량 소진·서버 장애까지 재현한 것은 아니므로,
모든 오류 경로가 정상이라고 확대해서 해석하지 않는다.

## 확인된 문제와 구현 영향

### 1. 세션 재개 실패를 정상 재개로 보고할 수 있다

AGY 1.1.27에 존재하지 않는 ID를 주면 stderr에 경고를 남긴 뒤 다른 ID의
새 세션을 시작했다. 현재 cokacdir는 실행 전 파일 존재를 확인하지만, 완료
시에는 요청한 `session_id`를 그대로 사용한다. 실제로 사용된 ID와 비교하지
않는다. 관련 코드는 `conversation_exists`와 `execute_command_streaming`의
`last_session_id` 결정 부분이다.

실제 어댑터 검증에서는 다음 순서로 재현했다.

1. 기존 live 테스트가 새 세션을 생성하도록 한다.
2. 재개 시 cokacdir의 존재 확인이 끝난 후, 이 테스트가 만든 `.db`만 잠시
   다른 이름으로 옮긴다. stdin·프롬프트·모델·세션 ID 인자는 변경하지 않는다.
3. 실제 AGY를 실행하고 종료 후 해당 작업 디렉터리의 실제 세션 ID를 기록한다.
4. 이동했던 파일을 원래 위치로 돌려놓는다.

AGY는 다른 ID로 성공했고, cokacdir는 이전 ID로 `Done`을 보고했다. 기존
live 테스트는 응답 문구·파일 생성·반환된 ID만 확인했으므로 이 상황도 통과했다.
따라서 기존 테스트 통과만으로 대화 맥락까지 이어졌다고 단정할 수 없었다.

이번 조사에서 live 테스트에 다음 확인을 추가했다.

- 첫 사용자 메시지에 무작위 기억 토큰을 넣고, 재개 시 되묻게 한다.
- 반환된 ID를 격리된 테스트 작업 디렉터리에 실제 저장된 ID와 대조한다.

검증 결과는 다음과 같다.

| 검사 | 결과 |
|---|---|
| 기존 테스트 + 세션 파일 사라짐 | 27.98초에 통과하여 검증 누락 확인 |
| 보강 테스트의 정상 경로 | 25.40초에 통과; 파일 생성 2회와 기억 토큰 유지 확인 |
| 보강 테스트 + 같은 장애 주입 | 최종 35.39초에 실제/보고 세션 ID 불일치 assertion으로 실패 |
| AGY 단위 테스트 | 43개 통과, 별도 실행하는 live 테스트 2개는 기본 실행에서 제외 |

장애를 주입한 테스트의 실패는 제품 결함을 검출한 결과다. 이를 일반 경로의
통과나 제품 결함의 수정 완료로 기록하지 않는다. 중간 장애 주입 실행에서는
응답 시간 초과가 먼저 발생했다. 없는 기억을 파일에서 탐색하지 않도록 테스트
지시를 제한한 뒤, 마지막 실행에서 ID 검사 자체가 결함을 검출함을 확인했다.

제품 코드의 세션 ID 검증 결함은 이 조사에서 수정하지 않았다. 구조화 결과의
`conversation_id`를 읽고, 재개 요청 ID와 다르면 성공으로 저장하지 않는 처리가
필요하다. 작업 디렉터리별 `last_conversations.json`은 같은 디렉터리의 동시
요청이 공유하므로 장기적으로 요청별 결과 ID를 대신하는 근거로 삼기 어렵다.
동시 요청의 ID 혼동은 코드상 위험이며, 이번 장애 주입과는 별개다.

### 2. 요청 전체에 독립적인 종료 제한이 필요하다

고유 ID의 테스트 `.db`에 의도적으로 잘못된 SQLite 내용을 넣었다. 이 파일은
현재 `conversation_exists` 검사를 통과할 수 있는 일반 파일이다.

AGY는 세션을 읽다가 `dbTrajectory.NumSteps` 경로에서 panic을 기록하고,
stderr에 크래시 안내를 출력했다. `--print-timeout 20s`에도 종료하지 않아
실측 도구가 45.018초에 해당 프로세스 그룹을 종료했다. 손상 파일은 제거했다.

현재 모델 **목록 조회**에는 별도 30초 제한이 있지만 일반 **추론 요청 전체**에
같은 보호가 있는 것은 아니다. 시스템 프롬프트를 쓰는 경우 훅 미완료의 30초
제한이 초기 정지를 막을 수 있으나, 훅이 없는 호출과 훅 완료 후의 정지까지
보장하는 전체 실행 제한은 아니다. 독립적인 프로세스 종료 제한과 오류 진단이
필요하다는 근거다. 실제 cokacdir가 이 사례에서 무한히 멈췄다는 의미로 확대하지
않는다.

### 3. 기존 훅 검증은 계속 필요하다

현재 설치된 cokacdir 플러그인의 실행 환경에만 실패 helper를 지정했다.
ledger에는 `start`와 `fail`이 남고 AGY 로그에는 훅 실패가 기록됐다. 그런데
AGY의 stderr는 비어 있었고 JSON은 `SUCCESS`와 정상 응답을 반환했다.

따라서 JSON `status`만 검사하도록 바꾸면서 cokacdir의 acknowledgement와
ledger 검사를 없애면 안 된다. 현재의 응답 보류·실패 시 폐기는 이 조건을
방어하는 데 필요하다. [공식 훅 문서](https://antigravity.google/docs/hooks/)의
`PreInvocation`과 `ephemeralMessage` 계약은 유지된다.

### 4. 오류 문자열 검색을 추가하는 방식은 적절하지 않다

현재 버전의 일반 오류는 실제로 종료코드 1로 확인됐다. 같은 오류 문장도
사용자가 요청한 정상 답변으로는 종료코드 0, stderr 없음으로 출력됐다.
따라서 본문의 `Error:` 같은 문자열만으로 성공 여부를 바꾸면 오탐이 생긴다.

최신 버전에서는 JSON 또는 stream-json의 `status`, `error`, `conversation_id`를
우선 읽는 방향이 적합하다. 현재 cokacdir가 JSON을 사용하는 곳은 모델 목록이며
추론 응답은 아직 text다. 로그는 실패 진단을 보충하는 용도여야 하고, 성공한
실행의 초기 인증 경고나 복구된 도구 오류까지 실패로 취급해서는 안 된다.

## 공식 변경 기록과 이슈 상태

[공식 변경 기록](https://github.com/google-antigravity/antigravity-cli/blob/main/CHANGELOG.md)은
1.1.8의 구조화 응답, 1.1.12의 구조화 모델 목록, 1.1.18의 일부 빈 성공 응답
수정, 1.1.24의 상속된 출력 핸들 관련 종료 정지 수정, 1.1.27의 대화 저장
완료 전 종료 수정 등을 명시한다. 이전 버전의 관측을 최신 버전의 재현 결과로
그대로 사용하면 안 된다.

| 이슈 | 조회 상태 | 이번 확인 |
|---|---|---|
| [cokacdir #54](https://github.com/kstost/cokacdir/issues/54) | open, 댓글 0 | TSV의 ID·라벨 분리는 실제 계약. 현재 수정본과 14개 항목 일치 |
| [cokacdir #48](https://github.com/kstost/cokacdir/issues/48) | open, 댓글 0 | 빈 `--print`가 stdin을 버리는 현상은 현재 CLI에서도 재현. 현재 어댑터는 해당 플래그를 사용하지 않음 |
| [cokacdir #53](https://github.com/kstost/cokacdir/issues/53) | open, 댓글 0 | Windows용 인라인 명령은 현재 소스에 남아 있음. 이 환경에서 Windows 실행은 검증하지 못함 |
| [upstream #222](https://github.com/google-antigravity/antigravity-cli/issues/222) | open, 댓글 7 | 오래된 디스패치 보고뿐 아니라 1.1.8의 Windows 인용 문제 보고도 포함 |

#53의 인용 문제 설명은 cmd.exe를 일반적인 Windows argv 인용과 구분하는
[Go 공식 문서](https://pkg.go.dev/os/exec#Command)와 부합한다. 다만 이 사실만으로
현재 AGY의 Windows 실행 경로와 모든 경로·버전의 실패를 확정하지는 않는다.
별도 `.cmd`로 옮기는 제안도 공백·비ASCII 경로에서 실제 AGY를 통한 검증이
필요하다.

이번 환경에는 Windows, macOS 또는 Windows 실행 호환 도구가 없었다.
해당 플랫폼의 해결 여부, 모든 모델의 실제 가용성, 인증·할당량의 모든 오류
조건까지 검증 완료했다고 표현하지 않는다.

## 후속 수정과 재검증

추론 어댑터를 `--output-format stream-json`으로 전환했다. 새 대화의 ID는
해당 요청의 terminal `result.conversation_id`에서 읽고, 재개 요청은 이벤트의
ID가 요청한 ID와 같은지 검사한다. 다른 대화로 바뀌면 응답을 폐기하고 오류를
보고한다. 공유 캐시를 완료 ID의 근거로 사용하지 않는다.

종료코드와 terminal `status`·`error`·본문을 함께 검사한다. JSON 손상, 결과
누락·중복, 세션 ID 불일치, 빈 성공 응답도 성공으로 전달하지 않는다. `init`
없는 사전 검증 오류는 허용하고, 일반 답변의 `Error:` 같은 문구는 오류로
추정하지 않는다. 기존 hook acknowledgement와 ledger 검증을 유지한다.

`COKAC_AGY_PRINT_TIMEOUT`의 양수 Go 형식 시간을 CLI와 별개로 부모가 강제한다.
기본값은 기존과 같은 `1h`이며 프로세스 시작부터 종료까지 적용한다. 입력과
출력은 이름을 제거한 전용 파일 핸들로 처리하므로, 자식이 stdin을 읽지 않거나
하위 프로세스가 출력 핸들을 상속해도 파이프 쓰기·EOF 대기에 갇히지 않는다.
시간 초과·취소·출력 크기 초과에는 프로세스를 종료하고 회수한다.

### 추가 발견: 예약 세션 복제

정상 요청과 재개 뒤 예약 실행용 복제본을 재개했더니 `trajectory not found`가
발생했다. 파일 이름은 새 ID였지만 SQLite `trajectory_meta.cascade_id`는 원래
ID였다. 실제 CLI에서 복제본의 해당 필드만 새 ID로 바꾸자 기억 토큰이 유지된
정상 응답을 얻었고 원본 파일의 SHA-256도 유지됐다. 기존의 파일 복사만으로는
현재 AGY 세션 복제가 완성되지 않는다는 실측 근거다.

제품 코드도 SQLite 백업 직후 **예약한 복제 파일 핸들에 연결된 DB만** 갱신한다.
`trajectory_id`와 과거 메시지·도구·생성 메타데이터는 유지한다. 알려진 메타데이터
테이블에 행이 없거나 여러 개면 복제를 중단하고 소유권이 확인된 실패 복제본만
제거한다. 원본의 WAL에만 있는 최신 변경을 포함하는 백업과 경로 교체 방어는
유지한다.

백업 API가 WAL의 커밋된 페이지를 복제 파일에 모두 반영한 뒤에는 해당 파일의
SQLite 연결을 닫는다. 독립된 백업본에 남은 WAL 형식 표시만
[공식 파일 형식의 18·19번 헤더 바이트](https://sqlite.org/fileformat.html#file_format_version_numbers)에
따라 rollback 형식으로 전환하고, 같은 예약 핸들로 다시 열어 ID를 갱신한다.
원본 DB나 원본 WAL의 헤더는 변경하지 않는다. 이를 통해 별도 WAL·journal
경로를 열지 않는 기존 VFS에서 메타데이터를 수정할 수 있다.

[공식 대화 문서](https://antigravity.google/docs/cli/conversations/)의 `/fork`도
직접 확인했다. AGY 1.1.27의 headless 실행은 `--conversation`을 함께 줘도
`/fork is not available in print mode`로 종료코드 2를 반환했다. 따라서 이
인터페이스로 예약용 복제를 대체하지 않았다. 내부 DB 형식은 공개 API가 아니며
다른 AGY 버전의 형식 변화는 별도 검증이 필요하다.

### 최종 검증 결과

| 검사 | 결과 |
|---|---|
| AGY 회귀 테스트 | 61개 통과; 별도 실행하는 live 테스트 4개는 기본 실행에서 제외 |
| Telegram 모델 메뉴·이전 설정 호환 | 3개 통과 |
| 모델 목록 실측 | JSON·TSV의 14개 ID·라벨 일치; ID 선택 검증 통과 |
| 실제 새 요청 → 재개 → 예약용 복제 재개 | 47.09초에 통과; 두 번의 훅 지시 파일 생성, 기억 토큰 유지, 복제 ID 일치, 원본 DB 바이트 불변 |
| 실제 누락 세션의 교체 ID 검증 | 3.48초에 오류 차단 확인; Text·AssistantFinal·Done 없음 |
| 실제 손상 세션, 부모 제한 2초 / CLI 제한 30초 | 부모가 2.043초에 종료·회수; 성공 메시지 없음 |
| 실제 어댑터의 존재 확인 직후 테스트 DB 이동 | 잘못된 재개를 `different conversation` 오류로 차단; 오류의 stdout은 빈 문자열, 이동한 원본 복구 확인 |
| 실제 어댑터의 hook helper를 `/bin/false`로 설정 | `system-prompt hook` 오류로 차단; 오류의 stdout은 빈 문자열 |
| 테스트 바이너리와 애플리케이션 빌드 | 통과, 기존 경고는 남아 있음 |

마지막 두 장애 주입은 정상 응답을 기대하는 live 테스트를 그대로 실행하므로
Rust 테스트 프로세스 자체는 예상한 오류에서 종료코드 101을 반환한다. 외부
검증기가 **해당 오류 메시지로 차단됐는지** 검사해 통과로 판정했다. 정상 경로
테스트의 실패를 통과로 바꿔 기록한 것이 아니다. 입력을 읽지 않는 자식, 닫힌
출력, 상속된 출력 핸들, 취소·수신 종료, 손상된 JSON과 비정상 종료는 별도의
결정적 회귀 테스트로도 확인했다.

후속 측정 로그와 빌드 기록은 `/tmp/cokac-agy-runtime-fix-i4np650v/`에 보관한다.
Windows와 macOS의 실제 실행, 모든 모델에 대한 추론, 실제 인증 만료·할당량
소진은 이번 검증 범위에 포함하지 않는다.
