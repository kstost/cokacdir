# How to Use Telegram Voice Requests

## Overview

Telegram audio and voice messages are transcribed with the local `transcriptor` integration and then presented for an explicit decision. A recognized transcript is never sent to the Agent merely because speech recognition finished.

The normal flow is:

```text
Send audio
  → speech recognition
  → review transcript
  → choose “이 내용으로 실행” or “취소”
  → run the Agent or stop without running it
```

This confirmation applies to a single voice/audio message and to Telegram media albums that contain transcribable audio.

---

## Basic Flow

1. Send a Telegram voice note or audio file to the bot.
2. The bot displays `Recognizing speech..` while `transcriptor` works.
3. If the selected model is not installed, the same progress message can show model-download progress first.
4. When transcription finishes, the bot shows each transcript prefixed with `🗣️` and asks:

   ```text
   이 내용으로 실행할까요?
   ```

5. Choose one of the inline buttons:

   - `이 내용으로 실행`
   - `취소`

The decision is recorded by editing the confirmation message and removing its buttons. A removed or stale button cannot execute the request later.

### Long transcripts

If the transcript and question fit in one Telegram message, cokacdir edits the recognition progress message in place. If the transcript is too long, the transcript is sent through the normal long-message path and a separate compact message contains the confirmation buttons.

In both cases the Agent waits for the same explicit decision.

---

## What Each Button Does

| Decision | Result |
|---|---|
| `이 내용으로 실행` | Commits the transcript as the User request and passes it through the normal Agent request path. Existing provider, session, output, queue, and memory settings apply. |
| `취소` | Ends the voice request without invoking the Agent. Reserved album-upload context is rolled back and the per-chat request slot is released. |

Only the first valid decision is accepted. Repeated taps, an old callback ID, a callback attached to the wrong message, or a callback after cancellation is treated as stale and cannot start work.

The sender's decision is committed before the button acknowledgement is shown. This prevents two nearly simultaneous taps from executing the same voice request twice.

---

## There Is No Confirmation Timeout

Voice confirmation intentionally has no timer. The request task suspends without busy-waiting until one of these events occurs:

- the sender chooses Execute;
- the sender chooses Cancel;
- the sender submits a replacement request before a decision is committed;
- `/stop` or `/stopall` cancels the active request;
- the dispatch fails or the bot shuts down.

Waiting longer does not automatically execute or cancel the transcript. While it waits, the voice request still owns the chat's active request slot so the transcript and any album attachments cannot be detached from their decision.

During an orderly bot shutdown, pending decisions are cancelled, visible confirmation buttons are removed, and the message is marked as stopped on a best-effort basis. If the process or host is terminated too abruptly to call Telegram, the old buttons can remain visible, but their in-memory callback state is gone and they cannot reconstruct or run the request after restart.

---

## Sending Another Request Instead of Pressing a Button

If the same user sends another executable request while their voice request is still transcribing or waiting for confirmation, the new request supersedes the uncommitted voice request.

The old confirmation is finalized with:

```text
↪️ 새 요청이 입력되어 이 음성 요청을 취소했습니다.
```

The new request then follows the normal chat policy:

- with queue mode ON, it is admitted to the queue and runs after voice cleanup releases the slot;
- with queue mode OFF, it follows latest-wins redirect behavior;
- if the new request cannot be admitted—for example, because the queue is full—the new request is rejected and the original voice confirmation remains pending.

Replacement is allowed only before an Execute or Cancel decision has committed. Once Execute is committed, the transcript is a normal Agent request; later messages use the ordinary queue or redirect rules rather than retroactively cancelling the voice decision.

In a group, a request from another participant does not supersede the original sender's pending voice decision. It follows the group's normal busy/queue behavior.

---

## Who Can Press the Buttons

Only the Telegram user who sent the audio can choose Execute or Cancel.

In a group chat, another participant who presses a button receives an alert indicating that only the audio sender can choose. Bot ownership or public-group access does not transfer ownership of that specific confirmation.

The callback is also bound to:

- the chat;
- the confirmation message;
- a random per-request confirmation ID;
- the original sender's Telegram user ID.

These checks prevent a callback copied from another message, chat, or request from being accepted.

---

## Cancellation and Queue Interaction

| Event while speech recognition or confirmation is active | Behavior |
|---|---|
| Sender taps Cancel | Voice request ends; no Agent process starts |
| Sender sends another executable request | Uncommitted voice request is superseded; new request follows queue/redirect policy |
| Another group member sends a request | Original voice request remains owned by its sender; new request follows normal queue policy |
| `/stop` | Cancels the active voice request and releases its slot; queued messages remain |
| `/stopall` | Cancels the voice request and clears the chat's queue |
| Execute races with `/stop` | Cancellation wins if the request token was cancelled before Agent execution; the UI is corrected to Cancel |
| Dispatch panic or dropped decision receiver | The request is cancelled, owned uploads are rolled back, and the confirmation UI is finalized as failed |
| Orderly server shutdown | Pending request tokens and decisions are cancelled; confirmation buttons are disabled on a best-effort basis |
| Abrupt process/host termination | Telegram UI may remain visible, but callbacks are stale after restart and unconfirmed album records were never published |

The confirmation wait is part of the active request, not a separate queue item. This is why `/stop` can cancel it and why cleanup triggers the next queued message afterward.

See [How to Manage Requests](how-to-manage-requests.md) for the general queue, redirect, `/stop`, and `/stopall` rules.

---

## Captions, Multiple Audio Items, and Attachments

### One audio item

Without a caption, the approved transcript becomes the User request. With a caption, cokacdir keeps the caption as the leading instruction and adds the transcription as audio context.

Conceptually:

```text
<caption>

[Transcribed audio]
<transcript>
```

### Multiple audio items

For an album containing several audio items, the execution request preserves their order:

```text
[Audio 1]
<first transcript>

[Audio 2]
<second transcript>
```

The confirmation view shows all non-empty transcripts. If an item fails transcription, the album request fails rather than silently executing a partial interpretation as though it were complete.

### Mixed album attachments

Non-audio files belonging to the same album are saved to the workspace immediately, but their upload records are not yet published to session history, the generic pending-upload bucket, or the shared group log. Those records are committed only after Execute and are handed explicitly to that Agent request. Cancel, replacement, failure, shutdown, or a dropped confirmation therefore cannot leave an unconfirmed attachment recorded as conversation history or let another message inherit it.

Unrelated uploads that arrive while confirmation is pending are not consumed or removed by the voice request.

---

## Choose the Speech Recognition Model

Use `/stt_model` to inspect or change the current chat's transcription model:

```text
/stt_model
/stt_model small
/stt_model large-v3-turbo
/stt_model path:/absolute/model.bin
/stt_model reset
```

- A bare name is passed to `transcriptor` as `--model-name`.
- `path:<model_path>` is passed as `--model`.
- `reset`, `clear`, `default`, or `unset` removes the chat override and restores `transcriptor`'s environment, saved configuration, or default.

Changing `/stt_model` affects recognition, not the Execute/Cancel requirement. See [How to Configure Settings](how-to-configure-settings.md#stt_model) for the full model-setting reference.

---

## Persistent Memory Interaction

When `/usememory` is ON, a voice request becomes eligible for persistent memory only after the sender chooses Execute and the resulting Agent turn completes successfully.

- The confirmed textual request and canonical final Assistant answer can be stored.
- Raw audio bytes are not copied into the persistent memory record.
- Recognition progress, the confirmation question, button callbacks, and cancellation status are not stored.
- A cancelled, replaced, failed, or never-executed transcript creates no memory turn.

See [How to Use Persistent Conversation Memory](how-to-use-persistent-memory.md) for the complete eligibility and privacy rules.

---

## Troubleshooting

### The transcript is wrong

Choose Cancel and send a corrected text request or record the audio again. There is no edit-in-place command for a pending transcript, and cokacdir does not ask the Agent to infer what the speech recognizer probably meant.

### The buttons do not appear

If the bot cannot display a valid confirmation UI, it fails closed: the transcript is not sent to the Agent. Check Telegram API connectivity and the local debug log, then resend the audio.

### Telegram says only the sender can choose

The button was pressed from a different Telegram account than the account that sent the audio. The original sender must decide or cancel the request with an authorized control path.

### Telegram says the request is no longer pending

The confirmation was already decided, superseded, cancelled, cleaned up after an error, or created by an earlier bot process. Its buttons are stale and cannot run anything.

### Recognition fails before confirmation

The Agent is not invoked. Check the configured model path/name, available storage for a model download, file size and format, and the debug log. Telegram's normal bot download limit still applies.

### Another message was queued while waiting

For the same sender, an admitted executable request should supersede an uncommitted voice request. In groups, a message from a different participant does not own the voice confirmation and therefore waits according to the normal queue policy.

---

## Privacy and Limits

- Telegram audio and voice uploads are subject to Telegram's Bot API file-size and retention behavior.
- `transcriptor` and its Whisper/whisper.cpp model artifacts have separate upstream licenses and model limitations.
- Obtain appropriate consent before recording or submitting other people's speech.
- Speech recognition can be wrong; explicit confirmation exists so the user can prevent an incorrect transcript from becoming an Agent instruction.

See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md) for dependency, model, and audio-consent notices, and [How to Use File Transfer](how-to-use-file-transfer.md) for Telegram upload limits.
