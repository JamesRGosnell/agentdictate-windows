# Continuous transcription investigation — October 18, 2026

## What would actually remove the remaining wait

The selected ChatGPT subscription dictation path uploads a completed WAV after
Stop. Authentication now overlaps recording, but audio upload and recognition
still follow Stop. The meeting copilot sends provisional file segments after
short pauses. Its earlier transcription reduces the wait without making the
recognizer continuous.

The optional API-key dictation path already streams to `gpt-live-transcribe`,
with final-only delivery and a saved-WAV fallback. Enabling that setting does
not make the subscription adapter stream. The two services use different
protocols and authentication. [OpenAI's live transcription documentation](https://developers.openai.com/api/docs/guides/realtime-transcription)
describes the API-key route, incremental deltas, manual commit, and the quality
tradeoffs of the model's delay setting.

## Current subscription stream: static contract verified

Read-only inspection of the installed Codex 26.1013.4894.0 package found that it
still contains a first-party subscription streaming implementation. This updates
the older August evidence in `codex-subscription-transcription.md`; it is not
evidence of a successful AgentDictate streaming session.

The inspected files are static JavaScript inside the installed `app.asar`:

- `.vite/build/main-C4DYk222.js`, SHA-256
  `9972d22e8066aba9c8fee5e83145e315fe92fbaf1680010751952139e80ed384`.
- `webview/assets/app-initial-5851df3f6cb1.js`, SHA-256
  `c4aeacc79f53f8895dd1b48a79dbe55be497839bd11ed0772d10e5ee4c6a55e7`.

No running-app hooks, credential files or user recordings were inspected.

The contract differs materially from the existing public Realtime adapter:

| Concern | Installed first-party client behavior |
| --- | --- |
| Endpoint | WebSocket at `/backend-api/dictation/stream` |
| Authentication | App Server backend auth; bearer supplied in a WebSocket subprotocol, kept in memory |
| Workspace routing | Connect-info rejects workspace routing and requests HTTP transcription instead |
| Session start | `session.start`, with separate dictation-session and attempt IDs |
| Audio | Base64 PCM16 in `audio.append`; sample rate supplied explicitly; one channel |
| Recognition mode | Requests `provider_mode: streaming_sse` |
| Transcript mode | Requests `segment` or `final_only`; event schema also recognizes deltas |
| Turn detection | Requests server VAD, threshold 0.5, 300 ms prefix padding and 500 ms silence |
| Client bounds | Requests a 4 MiB buffer, 30-second utterances and a five-minute session TTL |
| Finish | Sends `session.close`; waits for the session's closed state |
| Assembly | Tracks each utterance ID and revision; final text supersedes partial text |

Those durations are client-requested configuration, not independently verified
server maximums. An AgentDictate adapter must test permitted values and session
rollover rather than assuming indefinite recording. It must also avoid duplicating
revised segments or treating an incomplete stream as a complete dictation.

The first-party client additionally tracks processed-audio acknowledgments and
sampled frame round-trip timings. These would let a prototype distinguish audio
queueing and transport delay from server work. A WebSocket connection alone would
not prove that recognition overlapped speech: test for useful transcript events
before Stop and compare final text after closing.

### Live verification remains open

A fresh, headless call through AgentDictate's existing `CodexSubscriptionTransport`
and its standard App Server auth provider stopped before transcription: Codex
reported a ChatGPT sign-in but supplied no access token. The test used a synthetic
English WAV. No credentials were read directly, printed or exported, and no
refresh, login change or alternate credential route was attempted.

Ordinary installed dictation works. The headless mismatch therefore does not
establish a broken account or a rejected streaming entitlement. No authenticated
WebSocket request was made, and neither subscription-stream latency nor quality
has been measured. Resolve this diagnostic mismatch through the existing auth
boundary before treating the reconstructed wire protocol as ready to ship.

## Local CPU feasibility experiment

The machine is running Windows 11 Home build 26200 with an Intel Core Ultra 9
185H (16 cores, 22 logical processors) and Intel Arc integrated graphics. A
separate ignored research environment uses CPU-only `sherpa-onnx` 1.13.8; it
does not add an application dependency or change settings.

Two genuine streaming transducer models were downloaded from the repositories
linked by the [sherpa-onnx model documentation](https://k2-fsa.github.io/sherpa/onnx/pretrained_models/online-transducer/zipformer-transducer-models.html):

- English 20M Zipformer, repository
  `csukuangfj/sherpa-onnx-streaming-zipformer-en-20M-2023-02-17`, revision
  `d42f2d9f7ca24806fb667456a18a9f1b60f70d16`.
- French Zipformer, repository
  `shaojieli/sherpa-onnx-streaming-zipformer-fr-2023-04-14`, revision
  `3db9565d9633758d6b87b9a7b3dc09ebfb6b2c73`.

The experiment uses int8 weights, two inference threads, 20 ms input blocks,
and manual finalization with 660 ms of right-context padding supplied immediately
at Stop. It separates model loading, decode work while receiving audio, and
residual decode work after Stop. Public read-speech samples and two Windows TTS
fixtures are replayed as fast as possible, with one public example per language
also replayed at its real audio rate. Silence is checked separately. These
fixtures cannot establish quality on James's microphone, accent, bilingual
speech or meeting audio.

| Measurement | English, five fixtures | French, three fixtures |
| --- | ---: | ---: |
| Model loading | 995 ms | 1,920 ms |
| Median active decode time / audio duration | 0.040 | 0.081 |
| Audio consumed before first nonempty partial | 740–2,340 ms | 1,060–1,380 ms |
| Residual decode after manual Stop | 19–29 ms | 42–50 ms |
| Word errors / reference words, unpaced fixtures | 18 / 104 | 5 / 35 |

The real-time factor includes active decode work and final flushing, excluding
time waiting for the next audio block. Both recognizers kept up with paced input;
the paced examples produced the same final text as their unpaced counterparts.
The figures show substantial compute headroom, not 19–50 ms end-to-end dictation:
they exclude capture, queuing, IPC, delivery, and model loading. Keeping a model
warm is necessary. The manual Stop bypasses an additional real-silence wait by
feeding zero padding immediately; that is a prototype finalization policy.
The first-partial figures describe position in the input audio, not stable-word
latency or elapsed time in an unpaced replay. For very short recordings the
recognizer may produce no useful partial before Stop; small residual decode
time alone does not establish earlier copilot drafting.

The accuracy sample is small and is neither a production WER benchmark nor a
comparison against the current cloud service on identical audio. It nevertheless
exposes concrete problems. The English model rendered the synthetic terms
“Supabase” and “Postgres” as unrelated words and sometimes lost initial words.
French samples contained proper-name errors and a changed verb tense. Neither
model emitted text for a three-second silence fixture. No hotword biasing or
language model was enabled. Normalization ignores case and punctuation but does
not equate alternative spellings or number formats.

A follow-up tried 320 and 640 ms of leading silence. The longer padding recovered
three missing initial words in one public English fixture, but did not solve the
synthetic vocabulary errors. That is a useful startup experiment, not evidence
that those models are ready for final dictation. The models are separately
monolingual and do not preserve the current automatic English/French behavior.

**Disposition:** local continuous recognition is computationally feasible on this
CPU. These particular small models are not an acceptable automatic replacement
for final dictation. A better first role is hidden speculative copilot input,
with cloud confirmation and the existing reveal controls, while evaluating a
more accurate multilingual model and vocabulary biasing. If cloud confirmation
is required for every dictation, local speculation alone does not shorten the
final cloud-dependent paste.

## Other candidates and constraints

- **Public OpenAI streaming:** already implemented for API-key dictation. It is
  the most direct supported cloud comparison, but uses separate API billing and
  is not a verified subscription feature. Do not switch the current route or
  add paid fallback as a side effect of this latency investigation.
- **whisper.cpp:** multilingual and Windows-capable. Its [streaming example](https://github.com/ggml-org/whisper.cpp/blob/master/examples/stream/README.md)
  repeatedly decodes a rolling window; that differs from an incremental
  transducer. Evaluate overlapping compute and a stable-prefix policy rather
  than assuming the sample's 500 ms step is final-transcript latency.
- **Windows AI speech:** the current [Microsoft documentation](https://learn.microsoft.com/en-us/windows/ai/apis/speech-recognition)
  marks the API experimental and requires MSIX capabilities. AgentDictate's
  current portable Win32 packaging does not satisfy that contract. It is a
  separate packaging experiment, not a drop-in production dependency.

## Integration and acceptance criteria

Retain the durable WAV as the source of truth. Run recognition off the recording
thread with bounded buffering. Associate all results with the recording job,
cancel stale results, and deliver exactly one final transcript. A stream failure
must retain the saved recording and use the current selected provider's fallback
semantics.

For the copilot, stable partial speech can start a hidden draft before a pause;
continued speech must still cancel it, and audience/turn checks must still govern
reveal. For dictation, a local draft alone does not remove final cloud latency if
cloud confirmation remains mandatory. Earlier text is not the same as an earlier
correct final paste.

Before a provider change, compare repeated identical audio against the current
subscription route, alternating order and keeping model settings fixed. Measure
first stable words, stop-to-final text, complete stop-to-delivery time, CPU and
memory, and transcription quality. Include English/French code-switching, names,
legal and technical vocabulary, numbers, one-word phrases, pauses inside a
sentence, background noise, both meeting audio channels, long-session rollover,
disconnects and silence. Keep prototype results separate from production claims.

The next cloud experiment should be a small subscription-stream diagnostic using
the existing application auth boundary and synthetic English/French fixtures.
Require a transcript before Stop, matching final text, complete handling of
utterance revisions, and successful close/rollover. The current headless auth
limitation must be resolved before that experiment can establish compatibility.
For local recognition, evaluate a more accurate multilingual model and glossary
support before allowing final paste. Both experiments can reuse the existing
durable-audio capture; neither needs a replacement recording or overlay flow.
