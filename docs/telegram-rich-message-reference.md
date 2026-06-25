# Telegram Rich Message 고급 렌더링 참고 문서

작성일: 2026-06-24  
대상 구현: cokacdir `0.6.38`의 Telegram `/rich` 및 고급 렌더링 경로

이 문서는 cokacdir에 Telegram Bot API 10.1 Rich Message 기반 고급 렌더링을 적용할 때 참고한 외부 공식 문서와 내부 구현 판단을 한곳에 정리한 것이다. 원문 문서를 그대로 복제하기보다, 구현에 필요한 요건·제약·설계 결정을 추적 가능한 형태로 요약한다.

## 1. 참고 문서 목록

### 1.1 Telegram Bot API — Rich messages

- URL: https://core.telegram.org/bots/api#rich-messages
- 사용 목적:
  - Rich Message 기능의 정식 API 표면 확인
  - `InputRichMessage` 구조 확인
  - `sendRichMessage`, `sendRichMessageDraft`, `editMessageText.rich_message` 확인
  - Rich Markdown/HTML 입력 방식과 제한값 확인

핵심 확인 사항:

- Bot API 10.1에서 Rich Messages가 추가되었다.
- 봇은 구조화된 리치 메시지를 보낼 수 있고, AI 답변 생성 중 draft 형태로 스트리밍할 수도 있다.
- `InputRichMessage`는 `html` 또는 `markdown` 중 정확히 하나를 사용한다.
- `skip_entity_detection`으로 자동 URL·이메일·멘션·해시태그·명령어 등 탐지를 끌 수 있다.
- `sendRichMessage`는 최종 Rich Message 전송용이다.
- `sendRichMessageDraft`는 30초짜리 임시 draft 스트리밍용이며, 최종 영속 메시지는 별도로 `sendRichMessage`를 호출해야 한다.
- `editMessageText`에는 `rich_message` 파라미터가 추가되어 기존 메시지를 Rich Message로 편집할 수 있다.

### 1.2 Telegram Bot API changelog — Bot API 10.1

- URL: https://core.telegram.org/bots/api-changelog#june-11-2026
- 사용 목적:
  - 기능 도입 시점과 Bot API 10.1 변경 범위 확인
  - 관련 클래스·메서드가 같은 릴리스에서 추가되었는지 확인

핵심 확인 사항:

- 2026-06-11 Bot API 10.1에서 Rich Messages가 추가되었다.
- 추가된 주요 항목:
  - `RichMessage`
  - `InputRichMessage`
  - `InputRichMessageContent`
  - `Message.rich_message`
  - `sendRichMessage`
  - `sendRichMessageDraft`
  - `editMessageText.rich_message`
- Rich Message는 텍스트 스타일뿐 아니라 paragraph, heading, list, quote, table, details, media, map, formula 등 블록 구조를 포함한다.

### 1.3 Telegram 공식 블로그 — Rich Text for Bots

- URL: https://telegram.org/blog/watch-apps-and-more
- 한국어 URL: https://telegram.org/blog/watch-apps-and-more/ko
- 사용 목적:
  - 사용자 관점 기능 범위 확인
  - 실제 클라이언트 렌더링 대상 기능 확인
  - 긴 메시지 UX 확인

핵심 확인 사항:

- 봇 메시지에 다양한 리치 텍스트 옵션이 지원된다.
- 공식 블로그가 언급한 사용자 노출 기능:
  - 인라인 미디어
  - 캐러셀·콜라주
  - 표
  - 중첩 인용구
  - 제목·앵커
  - 접을 수 있는 섹션
  - 각주
  - 수식·수학 포맷팅
  - 위첨자·아래첨자
- Rich Message는 한 메시지에 최대 32,768자까지 담을 수 있고, 긴 메시지는 클라이언트에서 “Show More/더 보기” UX로 표시될 수 있다.

### 1.4 Telegram Desktop changelog

- URL: https://github.com/telegramdesktop/tdesktop/blob/dev/changelog.txt
- 사용 목적:
  - 클라이언트 측 Rich Message 렌더링 지원 상태 확인
  - 초기 클라이언트 버그 존재 여부 확인

핵심 확인 사항:

- Telegram Desktop 6.9에서 “Rich Text Formatting for Bots”가 추가되었다.
- 6.9.1에서 rich message layout 관련 수정이 있었다.
- 6.9.2에서 텍스트 없는 rich message 표시와 Recent Actions의 rich message 표시 문제가 수정되었다.
- 6.9.3에서 formula parsing crash 수정이 있었다.

구현 판단에 준 영향:

- 서버 API가 지원되더라도 모든 클라이언트가 완전히 같은 렌더링 품질을 보장하지 않을 수 있다.
- 따라서 기본 delivery 값은 `auto`, 기본 profile은 `safe`로 두고, 기존 `sendMessage`/split/file fallback을 보존했다. 단, `auto`에서도 Markdown 표처럼 classic 경로에서 구조가 사라지는 Rich 전용 블록은 Rich Message를 시도한다.
- Rich delivery가 `auto` 또는 `on`이면 시스템 프롬프트에 명시적인 Rich 응답 형식 지침을 자동 삽입한다. 이 지침은 최종 답변이 source-code 예시가 아니라 렌더링될 메시지 본문임을 알리고, AI가 Telegram Rich Markdown/HTML로 렌더링 가능한 응답을 작성하며, 요청된 표를 실제 Markdown table로 직접 출력하고, literal Markdown/HTML source를 요구받은 경우가 아니면 Rich markup을 코드블록으로 감싸지 않도록 한다.
- draft 스트리밍은 opt-in으로만 제공한다. `/rich draft on`일 때 final-only private chat에서 `sendRichMessageDraft` preview를 전송하고, 최종 응답은 animated processing placeholder의 rich edit/fallback 경로로 영속 전송한다.

### 1.5 내부 구현 문서와 코드

- `src/services/telegram.rs`
- `docs/how-to-configure-settings.md`
- `CHANGELOG.md`
- `README.md`
- `Cargo.toml`

사용 목적:

- 기존 Telegram 응답 전송 구조 파악
- `/silent final`의 final-only 응답 경로 파악
- placeholder를 쓰는 rolling response 최종 편집 경로 파악
- 긴 응답의 split/file fallback 정책 파악
- 설정 저장 구조와 per-chat 설정 패턴 파악

## 2. 공식 기능 범위 정리

### 2.1 API 객체와 메서드

현재 구현에서 직접 사용하는 API:

- `sendRichMessage`
  - 최종 리치 메시지를 새 메시지로 전송한다.
- `editMessageText` + `rich_message`
  - 기존 placeholder 메시지를 리치 메시지로 최종 편집한다.
- `InputRichMessage.markdown`
  - Rich Markdown 문자열을 전달한다.
- `InputRichMessage.skip_entity_detection`
  - 자동 entity 탐지를 끈다.

현재 구현에서 opt-in 또는 간접 지원하는 API:

- `sendRichMessageDraft`
  - AI 생성 중 임시 draft 스트리밍용이다.
  - 최종 메시지를 남기려면 별도 `sendRichMessage`가 필요하다.
  - `/rich draft on`에서 final-only private chat에 한해 사용한다.
- `InputRichMessage.html`
  - Rich HTML 입력 경로다.
  - 이번 구현은 AI 응답 원문 Markdown을 최대한 살리기 위해 `markdown` 필드를 사용한다. Bot API 문서상 Rich Markdown은 Rich HTML 태그를 포함할 수 있으므로, `/rich full` profile에서는 공식 Rich HTML surface와 arbitrary HTML을 Markdown field 안에서 그대로 통과시킨다.
- `InputRichMessageContent`
  - inline query 결과용 리치 메시지 콘텐츠다.
  - 현재 cokacdir의 일반 채팅 응답 경로와는 직접 관련이 없어 제외했다.

### 2.2 공식 제한값

공식 제한:

- Rich Message 텍스트 최대 32,768 UTF-8 문자
- 최대 500개 블록
- 최대 16단계 중첩
- 미디어 첨부 최대 50개
- 표 최대 20열

cokacdir 구현 제한:

- 문자 수는 `chars().count()` 기준으로 32,768 이하일 때만 Rich 경로를 시도한다.
- 블록 수는 정확한 Rich AST를 만들지 않고, 보수적으로 “비어 있지 않은 줄 수”를 500개 이하로 추정한다.
- 초과 시 기존 classic 경로로 fallback한다.

## 3. Rich Markdown 적용 범위

공식 Rich Markdown은 GitHub Flavored Markdown과 가능한 범위에서 호환되며, Rich HTML 태그도 일부 포함할 수 있다.

cokacdir에서 보존하도록 의도한 텍스트 중심 요소:

- 제목: `#`, `##`, `###` 등
- 굵게, 기울임, 취소선, 코드, spoiler, marked text
- 일반 목록
- 체크리스트/task list
- blockquote
- 구분선
- 표
- footnote
- inline 수식: `$...$`
- block 수식: `$$...$$`
- fenced math block
- code fence
- details/summary 접기 블록
- 안전하다고 판단한 일부 inline HTML 태그

`safe` profile sanitizer가 허용하는 HTML 태그:

- `<u>`, `<ins>`
- `<s>`, `<strike>`, `<del>`
- `<tg-spoiler>`
- `<sub>`, `<sup>`
- `<code>`, `<pre>`
- `<details>`, `<details open>`, `</details>`
- `<summary>`, `</summary>`
- `<aside>`, `</aside>`
- `<cite>`, `</cite>`
- `<br>`, `<br/>`, `<br />`

`safe` profile sanitizer가 의도적으로 escape하는 요소:

- `![](...)` 형태의 Markdown media block
- `<tg-map ...>`
- `<tg-collage ...>`
- `<tg-slideshow ...>`
- `<img ...>`
- `<video ...>`
- 기타 allowlist 밖의 raw HTML

`full` profile 동작:

- AI 응답의 Telegram Rich Markdown을 그대로 전달한다.
- Markdown media block을 escape하지 않는다.
- 공식 Rich HTML 태그와 arbitrary HTML을 escape하지 않는다.
- 따라서 다음 항목까지 Telegram Bot API parser에 맡긴다.
  - media block: photo, video, audio, voice note, animation
  - custom emoji: `tg://emoji`, `<tg-emoji>`, emoji `<img>`
  - date-time entity: `tg://time`, `<tg-time>`
  - map: `<tg-map>`
  - collage/slideshow: `<tg-collage>`, `<tg-slideshow>`
  - anchor/reference: `<a name>`, `<a href="#...">`, `<tg-reference>`
  - full table HTML: `bordered`, `striped`, `caption`, `colspan`, `rowspan`, `align`, `valign`
  - draft-only thinking block: `<tg-thinking>`

## 4. 왜 Rich Markdown을 선택했는가

초기 MVP는 기존 `markdown_to_telegram_html` 변환 결과를 Rich Message에 넣는 구조로 접근할 수 있었다. 그러나 이 방식은 Telegram Rich Message의 장점인 표, task list, footnote, LaTeX, details block 같은 고급 Markdown 구조를 HTML 변환 과정에서 잃을 수 있다.

따라서 최종 구현은 다음 구조를 선택했다.

1. Rich API에는 AI 응답 원문 Markdown을 기반으로 한 sanitized Rich Markdown을 전달한다.
2. classic fallback에는 기존 `markdown_to_telegram_html` 결과를 그대로 사용한다.
3. Rich API가 실패하면 기존 HTML `sendMessage`, 긴 메시지 split, file attachment 경로로 복귀한다.

이렇게 하면:

- Rich를 지원하는 Telegram 클라이언트에서는 고급 구조가 살아난다.
- Rich API가 실패해도 기존 사용자 경험은 유지된다.
- 기존 HTML fallback과 신규 Rich Markdown 경로가 서로 독립적으로 동작한다.

## 5. 보안·안전 설계

### 5.1 자동 entity detection 비활성화

`skip_entity_detection=true`를 사용한다.

이유:

- AI 응답 안의 URL, 이메일, 전화번호, 봇 명령어 등이 의도치 않게 자동 링크화되는 것을 줄인다.
- 명시적 Markdown 링크는 계속 사용할 수 있고, Telegram 클라이언트는 inline link를 열 때 사용자에게 URL 확인 UI를 보여준다.

### 5.2 미디어 block 비활성화

공식 Rich Markdown은 media block, collage, slideshow, map 등을 지원한다. 하지만 이번 구현에서는 텍스트 응답 경로에 자동 미디어 fetch를 열지 않았다.

이유:

- AI 응답이 외부 HTTP/HTTPS 미디어 URL을 포함할 경우, Telegram이 해당 URL을 미디어로 처리할 수 있다.
- 사용자가 의도하지 않은 원격 리소스 접근·첨부가 발생할 수 있다.
- 응답 텍스트 렌더링 개선이 목적이므로, 미디어 첨부는 별도 설계와 권한 모델이 필요하다.

`safe` profile 구현:

- `![](...)`는 `\![](...)`로 escape한다.
- media 관련 raw HTML 태그는 allowlist 밖이므로 escape한다.

`full` profile 구현:

- media block과 media HTML을 escape하지 않는다.
- Telegram API가 media 권한·URL·MIME·형식 문제로 거부하면 classic fallback으로 복귀한다.

### 5.3 raw HTML allowlist

공식 Rich Markdown은 임의 HTML을 포함할 수 있다. 그러나 AI 응답 원문을 그대로 HTML로 허용하면, 의도하지 않은 구조·미디어·링크·클라이언트 렌더링 문제가 생길 수 있다.

구현:

- allowlist에 있는 단순 텍스트/구조 태그만 보존한다.
- 나머지 `<...>` 태그는 `&lt;...&gt;` 형태로 escape한다.
- fenced code block 내부는 원문 그대로 보존한다.

### 5.4 fail-closed fallback

Rich 경로는 항상 best-effort다.

- API client 생성 실패
- HTTP 요청 실패
- Telegram API reject
- JSON 응답 파싱 실패
- 제한 초과

위 경우 모두 classic 경로로 fallback한다.

## 6. 구현 구조

### 6.1 설정

명령어:

```text
/rich
/rich status
/rich off
/rich auto
/rich on
/rich safe
/rich full
/rich profile safe|full
/rich rtl on|off
/rich draft on|off
```

모드:

- `off`
  - 항상 classic `sendMessage` / split / file fallback 사용
- `auto`
  - 기본값
  - classic 경로라면 메시지 분할/파일 첨부가 필요할 때 Rich 시도
  - Markdown 표처럼 classic 경로에서 구조가 사라지는 Rich 전용 블록도 Rich 시도
- `on`
  - 가능한 모든 eligible final response에서 Rich 우선 시도
- `safe`
  - 기본 profile
  - media block과 unsupported raw HTML을 escape
- `full`
  - delivery도 `on`으로 전환
  - Telegram Rich Markdown/HTML 전체 surface를 통과
- `rtl on|off`
  - `InputRichMessage.is_rtl` 지정
- `draft on|off`
  - final-only private chat에서 `sendRichMessageDraft` preview 전송

설정 저장:

- `BotSettings.rich_message_mode`
- chat id별 문자열 값: `off`, `auto`, `on`
- `BotSettings.rich_message_profile`
- chat id별 문자열 값: `safe`, `full`
- `BotSettings.rich_message_rtl`
- chat id별 boolean 값
- `BotSettings.rich_message_draft`
- chat id별 boolean 값

### 6.2 전송 경로

placeholder 없는 final 메시지 전송:

- 함수: `send_final_response_without_placeholder`
- 사용 사례:
  - 기존 placeholder가 없는 fallback/stopped/cancelled tail 전송
- 동작:
  1. 원문 응답을 `sanitize_rich_markdown`으로 정리
  2. `/rich` 설정에 따라 `sendRichMessage` 시도
  3. 성공하면 종료
  4. 실패하면 기존 file/html/plain fallback 진행

기존 placeholder 최종 편집:

- 함수:
  - normal rolling placeholder final edit 경로
  - schedule rolling placeholder final edit 경로
  - bot-to-bot rolling placeholder final edit 경로
- 동작:
  1. 최종 표시 텍스트를 normalize
  2. classic fallback용 HTML 생성
  3. Rich용 sanitized Markdown 생성
  4. `/rich` 설정에 따라 `editMessageText` + `rich_message` 시도
  5. 성공하면 classic edit 생략
  6. 실패하면 기존 HTML edit 또는 split/file fallback 진행

### 6.3 Raw Bot API 사용

현재 teloxide 의존성에 Rich Message helper가 노출되어 있지 않으므로, 해당 API는 `reqwest` raw call로 호출한다.

`sendRichMessage` payload 형태:

```json
{
  "chat_id": 123456,
  "rich_message": {
    "markdown": "# 제목\n\n| A | B |\n|---|---|\n| 1 | 2 |",
    "is_rtl": false,
    "skip_entity_detection": true
  }
}
```

`editMessageText` rich edit payload 형태:

```json
{
  "chat_id": 123456,
  "message_id": 789,
  "rich_message": {
    "markdown": "최종 응답 Markdown",
    "is_rtl": false,
    "skip_entity_detection": true
  }
}
```

`sendRichMessageDraft` payload 형태:

```json
{
  "chat_id": 123456,
  "draft_id": 123456789,
  "rich_message": {
    "markdown": "<tg-thinking>Thinking...</tg-thinking>",
    "is_rtl": false,
    "skip_entity_detection": true
  }
}
```

### 6.4 retry_after 처리

Raw Bot API 응답에서 다음 형태를 파싱한다.

```json
{
  "ok": false,
  "error_code": 429,
  "parameters": {
    "retry_after": 7
  }
}
```

`retry_after`가 있으면 기존 rate-limit 공유 대기 상태에 반영한다.

### 6.5 token redaction

Raw HTTP 오류 문자열이나 Telegram API 응답 body에 bot token이 포함될 가능성에 대비해:

- 현재 bot token 문자열을 `<bot_token_redacted>`로 치환
- 추가로 `redact_known_tokens` 적용

## 7. 테스트 기준

추가·확인한 테스트 범위:

- Rich mode 기본값이 `auto`인지 확인
- `/rich` mode parsing alias 확인
- chat별 설정 저장 확인
- `/rich` status 출력이 옵션을 포함하는지 확인
- `auto`가 classic split/file 필요 시점에만 Rich를 시도하는지 확인
- Rich Message 제한이 보수적으로 적용되는지 확인
- raw API `retry_after` 파싱 확인
- sanitizer가 표, task list, LaTeX, footnote, details block을 보존하는지 확인
- safe profile sanitizer가 media block과 unsafe HTML을 escape하는지 확인
- full profile이 media/map/collage/slideshow/anchor/reference/date-time/math/thinking 태그를 verbatim으로 통과시키는지 확인
- sanitizer가 fenced code block 내부를 원문 보존하는지 확인

실행한 검증:

```bash
cargo fmt
cargo test rich_message_mode_tests -- --nocapture
cargo test services::telegram -- --nocapture
cargo check
git diff --check
```

결과:

- `rich_message_mode_tests`: 10 passed
- `services::telegram`: 46 passed
- `cargo check`: success
- `git diff --check`: success

주의:

- 기존 코드베이스의 unused/unsafe 관련 warning은 계속 출력된다.
- 이번 변경으로 인한 컴파일 실패나 테스트 실패는 없었다.

## 8. 현재 의도적으로 제외한 것

이번 단계에서 제외한 항목:

- inline query용 `InputRichMessageContent`
- 구조화된 `RichBlock`/`RichText` AST builder
- client별 렌더링 capability 자동 감지
- media URL/domain allowlist 관리 UI
- business/direct-message/topic/reply-markup 같은 모든 optional `sendRichMessage` 파라미터에 대한 전용 사용자 명령

이번 단계에서 구현된 항목:

- `sendRichMessage`
- `editMessageText.rich_message`
- `sendRichMessageDraft` opt-in streaming
- `InputRichMessage.is_rtl`
- safe/full profile
- full profile의 media/map/collage/slideshow/arbitrary HTML passthrough
- inline query용 `InputRichMessageContent`
- Rich Message AST를 직접 생성하는 구조화 builder
- 클라이언트별 렌더링 capability 감지

제외 이유:

- 목적은 기존 최종 응답의 렌더링 품질 개선이다.
- draft streaming은 opt-in으로 구현했지만 기본값은 off다.
- media/map/collage는 외부 URL fetch와 권한·보안 정책이 필요하다.
- arbitrary HTML은 AI 응답 경로에서 과하게 넓은 공격면을 만든다.
- fallback 중심의 안정성을 우선했다.

## 9. 향후 확장 후보

안전하게 다음 단계로 확장하려면 다음 순서를 권장한다.

1. Rich Message client compatibility 관찰 로그 추가
   - API 성공/실패뿐 아니라 사용자가 실제 렌더링 문제를 신고할 때 추적하기 쉽게 한다.
2. media block opt-in 설정 추가
   - 예: `/rich media off|trusted|on`
   - 기본은 `off`
3. allowed media domain allowlist 추가
   - 외부 URL fetch를 허용할 도메인을 제한한다.
4. Rich AST builder 검토
   - Markdown 문자열 대신 구조화된 block 생성이 필요해질 때 검토한다.

## 10. 최종 구현 판단 요약

이번 구현의 핵심 판단은 다음과 같다.

- 기본 delivery는 `auto`, 기본 profile은 `safe`가 안전하다.
- Rich API에는 HTML 변환 결과가 아니라 sanitized Rich Markdown을 넣어야 고급 렌더링 이점이 살아난다.
- 기존 fallback은 반드시 유지해야 한다.
- 미디어와 arbitrary HTML은 Telegram 공식 기능이더라도 AI 응답 자동 렌더링 경로에서는 기본 차단이 안전하다. 사용자가 명시적으로 `/rich full`을 선택한 경우에만 전체 surface를 통과시킨다.
- draft streaming은 `/rich draft on`에서만 사용한다.
