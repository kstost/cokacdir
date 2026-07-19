# How to Use File Transfer

## Upload Files

Send a file, photo, or media to the bot. Non-audio uploads are saved to the current session's working directory; Telegram audio and voice uploads are transcribed as STT input.

If no session is active, a workspace is automatically created.

### Supported Types

- **Photo** — Saved as `photo_<id>.jpg` (highest quality selected)
- **Document** — Original filename preserved
- **Video** — Saved as `video_<id>.mp4` or original filename
- **Audio** — In Telegram, transcribed as STT input instead of being saved as a workspace file
- **Voice** — In Telegram, transcribed as STT input instead of being saved as a workspace file
- **Animation (GIF)** — Saved as `animation_<id>.mp4` or original filename
- **Video Note** — Saved as `videonote_<id>.mp4`

### Speech Recognition

Telegram audio and voice uploads are recognized with transcriptor. The bot first replies with `Recognizing speech..` and updates that progress while recognition runs. If transcriptor needs to download a model first, the same message shows model-download progress.

Recognition does **not** immediately invoke the Agent. After transcription, the bot displays the recognized text and waits for one of two inline-button decisions:

- `이 내용으로 실행` — commit the transcript as the User request and run it through the normal Agent path.
- `취소` — end the voice request without starting the Agent.

The confirmation has no timeout and only the Telegram user who sent the audio can decide it. A later executable request from that same user supersedes a voice request that is still transcribing or waiting for a decision. `/stop` and `/stopall` also cancel the pending voice request. Once a decision is accepted, the bot removes the buttons so an old callback cannot execute it again.

For long transcripts, the transcript can be sent separately from the compact confirmation message. For an album with multiple audio items, the execution request preserves their order as `[Audio 1]`, `[Audio 2]`, and so on. Album-owned attachments stay isolated while confirmation is pending and are handed to the Agent only after Execute.

See [How to Use Telegram Voice Requests](how-to-use-telegram-voice-requests.md) for replacement, queue, group authorization, album rollback, persistent-memory interaction, and troubleshooting details.

Use `/stt_model` to view the current STT model for the chat:

```
/stt_model
/stt_model small
/stt_model large-v3-turbo
/stt_model path:/absolute/model.bin
/stt_model reset
```

Bare model names are passed to transcriptor as `--model-name` and override an inherited `TRANSCRIPTOR_MODEL` value for that run; `path:<model_path>` is passed as `--model`. Resetting removes the chat override and lets transcriptor use its environment, saved config, or default model.

STT uses the MIT-licensed `transcriptor` binary and Whisper/whisper.cpp model
artifacts. See [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md) for
copyright, license, model, and audio-consent notices.

### Limits

- Maximum file size: **20MB** (Telegram Bot API limit)
- If a file with the same name already exists, a counter is appended: `file(1).txt`, `file(2).txt`, etc.

### Upload with Caption

If you include a caption with a non-audio file, the caption is sent to the AI along with the file context. For Telegram audio or voice, the caption is held with the recognized transcript and is sent only after the audio sender chooses `이 내용으로 실행`; choosing `취소` does not send either part to the AI.

### Upload While AI Is Busy

If the AI is busy processing a request and queue mode is ON, file uploads are captured and queued along with the message. When the queued message is processed, the file context is preserved.

---

## Download Files

### /down \<filepath\>

Downloads a file from the server to your Telegram chat.

```
/down /home/user/report.pdf
/down ./output.csv
```

- Accepts absolute paths anytime, or relative paths when a session is active (resolved against the current working directory).
- Only single files can be downloaded. If the target path exists but is not a regular file (e.g., a directory or symlink target), the bot replies with `Not a file: <path>`. If the path does not exist, the bot replies with `File not found: <path>`.
- If a relative path is given but no session is active, the bot replies with `No active session. Use absolute path or /start <path> first.`
