# Transcription latency: root cause and fix plan

Diagnosis of the 2026-09-01 report that a ~3-minute dictation took over a
minute to transcribe after stopping. Measured, not guessed: numbers below come
from the daemon logs, the history database, and a live timing harness that
replayed a retained recording against the OpenAI API. Status: Phases 1 and 2 were implemented on 2026-09-01
(Opus-compressed uploads with WAV fallback, per-stage timing logs, whisper-1
fallback pass removed). Realtime streaming was subsequently implemented for
the API provider. See the October 18 follow-up below for the current Windows
subscription path and remaining opportunities; the original diagnosis below
describes the September 1 configuration.

## Symptom

Stop-to-paste wall time, from `recording finalized` → `dictation flow
completed` in `agentdictated` logs:

| Date | Jobs | Avg wait | Long recordings (≥2 min) avg wait |
| --- | ---: | ---: | ---: |
| 2026-08-24 … 08-31 | 6–131/day | 4.1–5.2 s | 5.6–9.2 s |
| 2026-09-01 | 11 | 25.1 s | **60.7 s** (45.5 s and 76.0 s) |

Same model (`gpt-transcribe`), same 16 kHz mono WAV capture, same code path.
A 178 s recording that took 5.9 s of overhead on 08-25 took 46.0 s on 09-01.

## Root cause: uncompressed WAV upload on a slow uplink

The daemon uploads the raw PCM WAV (32 KB/s of audio; a 3-minute dictation is
~5.7 MB) in one blocking multipart POST. Live harness on 2026-09-01, same
62.5 s / 2.0 MB retained recording, two runs per model:

| Upload | Model | Total time | Measured upload speed |
| --- | --- | ---: | ---: |
| WAV 2.0 MB | gpt-transcribe | 14.7 s / 8.4 s | 136–239 KB/s |
| WAV 2.0 MB | gpt-4o-transcribe | 12.2 s / 13.1 s | ~150 KB/s |
| WAV 2.0 MB | gpt-4o-mini-transcribe | 13.9 s / 12.0 s | ~155 KB/s |
| WAV 2.0 MB | whisper-1 | 9.4 s / 11.3 s | ~195 KB/s |
| **Opus/OGG 186 KB** | **gpt-transcribe** | **2.6 s** | — |

`time_starttransfer ≈ time_total` in every WAV run: the request spends nearly
all its time uploading, then the model answers in ~1–4 s. The Opus run (24 kbps,
30× smaller payload) produced a transcript of identical length and content
quality in 2.6 s end to end.

Arithmetic check against the worst job (187.8 s audio, 6.0 MB WAV, 76.2 s
wait): 6.0 MB at ~136 KB/s ≈ 44–70 s upload + ~4 s model + 3.7 s cleanup
(measured live, see below) + ~1 s delivery. It reconciles.

So: the model is not slow and the code did not regress — the **uplink to
api.openai.com degraded around 08-31/09-01** (~10+ Mbps effective before,
~1–2 Mbps now), and the architecture multiplies that by 30× by uploading
uncompressed PCM. The app is one network-condition change away from this
happening again anywhere (hotel Wi-Fi, tethering, Imperial dorms).

Not the cause, checked and excluded:

- **Cleanup (`gpt-5.6-luna`, effort low)**: replayed today's real 382-word
  transcript through `/v1/responses` with the production instruction: 3.7 s.
  The overlay also already shows it as a separate "Cleaning up…" stage
  (`process.rs` cleanup-started observer), so the "Transcribing" label the
  user stares at is genuinely transcription (upload) time.
- **whisper-1 fallback second pass** (`openai.rs::transcribe_audio`): today's
  long transcripts were far above the suspicious-shortness threshold, so it
  did not fire. (It *would* silently double latency when it does — see
  observability below; nothing logs it today.)
- **Accuracy-stack changes of 08-31** (glossary transcription prompt, cleanup
  enable): a 176 s job *before* the settings change already showed 25.9 s
  overhead; cleanup adds only ~4 s.

## External context (researched 2026-09-01)

- `gpt-transcribe` (July 2026) is OpenAI's recommended file-transcription
  model; median throughput ~40× real time (~4.5 s of model time for a 3-min
  clip). `whisper-1` and the `gpt-4o-transcribe` family are deprecated,
  shutting down 2027-02-26 — the whisper-1 fallback needs a replacement
  before then.
- `/v1/audio/transcriptions` accepts ogg/opus (name the part `*.ogg`), mp3,
  m4a, flac, etc. Whisper-lineage models downsample to 16 kHz mono
  internally; community benchmarks show no accuracy loss down to ~32 kbps.
- `stream=true` (SSE deltas) only streams the *response*; it starts after the
  upload completes. It improves perceived progress, not time-to-final-text —
  not the fix here.
- The instant-feel dictation apps (Wispr Flow ~700 ms, Aqua Voice ~450 ms)
  stream audio *while the user speaks*. OpenAI's equivalent: Realtime
  WebSocket transcription — `gpt-transcribe` committed-turn mode (append
  pcm16/24 kHz chunks during recording, commit at stop, final text arrives
  almost immediately) or `gpt-live-transcribe` ($0.017/min) for live deltas.

## Plan

### Phase 1 — Compress the upload (small change, removes ~90 % of the wait)

Encode the finalized WAV to Opus-in-OGG before upload in
`ReqwestOpenAiTransport::transcribe_once`:

- Encode at stop: 16 kHz mono, 24–32 kbps. A 3-min WAV encodes in well under
  a second; payload drops 5.7 MB → ~0.6 MB, upload drops from tens of seconds
  to ~1–3 s even on a 1–2 Mbps uplink.
- Mechanism: shell out to `ffmpeg` (`-ac 1 -ar 16000 -c:a libopus -b:a 32k`),
  consistent with the app's existing pw-record/xsel subprocess pattern. If
  `ffmpeg` is missing or encoding fails, fall back to uploading the WAV and
  log it — never lose a dictation to the optimization. Packaging: declare
  ffmpeg as a recommended dependency (deb `Recommends:`, AppImage docs);
  the fallback keeps the app functional without it.
- Keep the durable capture as WAV — recovery, retry, and history semantics
  unchanged; the OGG is a transient upload artifact next to the WAV (or in
  memory), deleted with it.
- The whisper-1 fallback pass reuses the compressed file, so its cost when it
  fires drops too.

### Phase 2 — Per-stage observability (do in the same PR as Phase 1)

Today the log is silent between `recording finalized` and `paste command
submitted`; nothing attributes time to upload vs model vs cleanup, and the
whisper-1 fallback is invisible. Add INFO-level timing logs in the pipeline:

- `transcription request completed`: model, audio seconds, payload bytes,
  encode ms, HTTP ms, and transcript chars — one line per attempt, so the
  fallback pass shows up as a second line with its own model.
- `fallback transcription triggered`: word counts and threshold that fired.
- `cleanup completed`: model, effort, input/output tokens, HTTP ms.
- Extend `dictation flow completed` with total stop-to-paste ms.

That makes the next "why is it slow" answerable from one `grep job_id` instead
of a forensic session, and cheaply confirms Phase 1's effect in production.

### Phase 3 (optional, later) — Stream audio during recording

If sub-second stop-to-text is wanted, move to the Realtime WebSocket
committed-turn mode: open the session at record start, append pcm16 chunks as
PipeWire delivers them, commit at the stop hotkey; the transcript is complete
moments later regardless of uplink speed, because the audio already left the
machine while speaking. This is a real architecture change (recorder must tee
audio to the network, new failure modes: mid-session drops, reconnect,
last-chunk flush) — decide after seeing how far Phase 1 gets. Phase 1 remains
useful regardless as the retry/recovery path.

### Housekeeping surfaced by the diagnosis

- Replace or drop the `whisper-1` fallback before its 2027-02-26 shutdown.
- The 180 s request timeout plus a machine that suspends on idle means a slow
  job can outlive the user's presence; consider a systemd idle-inhibit while
  a job is processing (optional — Phase 1 makes waits short enough that this
  mostly stops mattering).

## Windows follow-up: October 18, 2026

### Current measurements and critical path

The current installation uses ChatGPT subscription transcription, with streaming
and cleanup disabled. The API provider's Opus compression and live WebSocket
path therefore do not accelerate these subscription jobs.

Across 59 completed jobs in the October 9–16 logs, median reported
`stop_to_paste_ms` was **2,005 ms**. Across 66 successful subscription responses
in the same log selection, median request elapsed time was **1,471.5 ms**. These
are different populations, so subtracting their medians is not a stage timing.
The completion metric also includes bookkeeping after paste submission.

The subscription request previously obtained credentials by starting a fresh
`codex app-server` after recording stopped. Three local, read-only
`getAuthStatus` probes without token output measured **1,148 / 598 / 474 ms**
for startup, status and shutdown; startup/initialization accounted for
1,107 / 577 / 452 ms. These are startup probes, not transcription benchmarks.

### Implemented improvements

- **Overlap authentication with recording.** Dispatch the recording lifecycle
  to the selected provider and obtain subscription credentials on a background
  worker after the first durable audio frame. Only the matching recording may
  consume the result, once. Keep at most one preparation worker across rapid
  start/discard cycles. Cancellation invalidates its result without waiting.
  Failed preparation and unrelated recovery requests check authentication again;
  tokens near expiry refresh, and HTTP 401/403 still trigger the existing single
  refresh. No audio is uploaded during this preparation. This removes the
  completed preparation time from the stop path; very short recordings can
  still wait for it. Actual production improvement remains to be measured.
- **Drain live audio in bounded bursts.** The optional API stream used to read
  one 100 ms audio chunk, then wait up to 40 ms for a server event. A backlog of
  20 seconds could incur about eight seconds of artificial waiting, exceeding
  the six-second finish deadline and forcing a file retry. Send up to 32 chunks
  per event read, retaining cancellation, deadline checks, finalized WAV bounds,
  final-item matching and saved-audio fallback. A silent mock WebSocket peer
  verifies that all 20 seconds reach commit within the existing deadline.
- **Attribute the remaining delay.** Log subscription audio-read and auth-wait
  time, preparation duration, and live finalization time and pending PCM bytes.
  Logs contain timings and identifiers, never credentials or transcript text.

### What professional systems suggest

[Wispr's engineering write-up](https://wisprflow.ai/post/technical-challenges)
describes a roughly 700 ms stop-to-result target, with separate ASR, formatting
and networking budgets. That is their stated target, not a benchmark of this
app. Its useful lesson here is to budget each stage and avoid speed changes
that create more corrections.

[OpenAI's Realtime transcription guidance](https://developers.openai.com/api/docs/guides/realtime-transcription)
supports sending audio while speaking, committing at the end, and matching
final results by item ID. It explicitly describes the delay/accuracy tradeoff
and recommends representative microphone recordings rather than synthetic-only
evaluation. [Deepgram's endpointing guidance](https://developers.deepgram.com/docs/endpointing)
shows how automatic systems finalize at speech pauses. This app already has an
explicit stop shortcut, so adding a silence timeout after that shortcut would
add latency without supplying a needed endpoint decision.

### Next opportunities, in priority order

1. **Measure production auth wait after this change.** Compare similar short
   and long recordings, separating successful first attempts, refreshed tokens
   and errors. Add monotonic timings for capture finalization, overlay dismissal
   and the actual paste submission before tuning those stages. Keep durable
   transcript and delivery checkpoints intact.
2. **Use the existing API live path for a larger architectural gain.** Moving
   upload and recognition into recording time can reduce the work left at stop.
   This requires an API key and separate API usage; it is not available through
   the currently implemented subscription endpoint. Do not silently switch the
   installed provider or claim that subscription streaming is supported.
3. **Evaluate the 150 ms overlay dismissal hold.** It deliberately covers a
   120 ms fade plus frame margin, and the delivery gate waits for helper exit.
   A safe redesign could fade during finalization while still confirming the
   overlay cannot intercept paste. Simply bypassing the delivery gate would
   weaken focus and failure handling.
4. **Evaluate lossless compression or earlier encoding for subscription audio.**
   The private endpoint is currently verified with finalized WAV only. Establish
   codec acceptance and real encoding/upload crossover timings before changing
   it. The API path already compresses uploads; those savings cannot be assumed
   for subscription jobs.
5. **For a larger experiment, compare local streaming recognition.** It could
   eliminate network latency, but needs real microphone, bilingual, names and
   punctuation evaluation on this machine's Intel Arc graphics. No local model
   or GPU inference stack was installed as part of this change.

Avoid speculative duplicate transcription requests, arbitrary shorter timeouts,
or pasting unstable partial text: they can increase cost, lose speech, or force
the user to repair output. No new provider, model, UI or paid service is needed
for the implemented authentication optimization.

Production observation later on October 18: a successful 2.96-second dictation
logged HTTP 200, `auth_wait_millis=0`, request elapsed 866 ms and reported
stop-to-paste 1,381 ms. This verifies that preparation completed before stop in
the working daemon. It is one observation, not a controlled overall speedup
estimate against the historical 2,005 ms median.

## Meeting copilot follow-up: October 18, 2026

The copilot has its own capture, transcription and answer pipeline. It already
starts transcription on a provisional pause, drafts likely questions while a
separate model judges them, warms HTTP connections, and primes the answer
prompts with the user's knowledge. Those existing features should be measured
before proposing them as new improvements.

### Evidence and current settings

The October 13–14 logs contain the following historical stage timings. They
include earlier builds, not a controlled benchmark of the current release:

| Stage | Samples | Median | 90th percentile |
| --- | ---: | ---: | ---: |
| Transcription request | 260 | 908.5 ms | 1,394 ms |
| Instant draft first text | 27 | 2,302 ms | 3,602 ms |
| Researched answer first text | 26 | 5,593.5 ms | 8,443 ms |
| Whole answer worker | 27 | 6,968 ms | 11,473 ms |

First-text timings start at the answer worker, after transcription. They do not
measure the full speech-end-to-visible-suggestion delay, and the researched
timing includes waiting for the instant draft. The small reveal-wait sample
(three events) is insufficient for a percentile conclusion.

The saved subscription settings select `gpt-6-astra` for quick drafts and
`gpt-6.1-sol` for researched answers. Both stay selected. A model's label or its
presence in `model/list` does not establish its latency or successful access.

### Implemented improvements

- **Share successful cold authentication across lanes.** Multiple warm-up,
  transcription and answer threads could all miss the credential cache before
  taking its provider lock, then each start a Codex check in sequence. Check
  the cache again after obtaining the provider lock, and publish the result
  before unlocking. Eight concurrent mock lanes now need one successful load.
  Near-expiry tokens and explicit refresh after rejection retain their behavior.
- **Stop canceled work before the next stage.** The answer worker previously
  waited up to eight seconds for a complete quick draft even after cancellation,
  then could start research anyway. Check cancellation while waiting and before
  handing off to research. Providers check before authentication/network work,
  retries and CLI launch. Existing blocking network reads can still finish
  after cancellation; this is not an instantaneous network-abort guarantee.
- **Wake at question deadlines.** During quiet audio, the engine's 250 ms
  maintenance tick could round a confirmation or verdict deadline up by almost
  another 250 ms. Schedule the receive timeout for the actual remaining pause
  and verdict wait. The original pause length, audience judgment, continued
  speech checks and reveal gate remain intact. Normal audio events may already
  wake the engine sooner, so real savings vary.
- **Separate queueing from service time.** Log transcription queue wait, judge
  duration, answer IDs and speech-end-to-reveal time. These supplement the
  existing request-to-first-text timings without logging speech or credentials.
- **Report missing-token state accurately.** Codex can report a ChatGPT sign-in
  while returning no access token. Distinguish that from being signed out.

### Professional patterns and retained tradeoffs

[Deepgram's eager end-of-turn guidance](https://developers.deepgram.com/docs/flux/voice-agent-eager-eot)
prepares an answer before final confirmation, cancels when speech resumes, and
reveals only after confirmation. The copilot already follows that pattern;
the cancellation fix removes work that could survive a rejected draft.
[Go's singleflight package](https://pkg.go.dev/golang.org/x/sync/singleflight)
documents suppressing duplicate concurrent work. The credential fix applies
that principle to successful cold authentication checks.

Retain the complete quick draft as input to research. The current research
prompt explicitly continues what the user is already reading, verifies it and
corrects it when needed. Starting research from only the first token would
change that contract and can repeat or contradict later draft text. A future
version could hand off a completed first sentence, but should evaluate the
whole displayed answer and its corrections before shipping that tradeoff.

### Live model comparison: blocked, with no timing result

A one-round comparison of `gpt-6-luna` and `gpt-6-astra` used the existing
synthetic-handbook diagnostic (four judge, five instant, three rewrite and two
research cases per model). All 28 checks stopped in local authentication before
a model request: Codex reported `authMethod: chatgpt` but returned no access
token, including after an explicit refresh. Both installed Codex executables
were checked through app-server; no credential files were read directly and no
credentials were exported. James confirmed that dictation is working, so the
diagnostic failure does not establish a broken account or a need to sign in
again. The mismatch between the diagnostic and the working runtime remains
unresolved. A subsequent working-daemon log also confirms HTTP 200 and delivery,
with zero foreground authentication wait. These are not 28 model-quality
failures or evidence that one model is faster.

Resolve the diagnostic's token handoff before repeating live comparisons,
without disturbing the working sign-in. Then compare
first useful sentence, full draft, factual correctness, corrections and
rhetorical-question handling on representative calls. Keep the existing models
until that evidence justifies changing them. Public model latency expectations
and the historical table above are not a substitute for those measurements.

## Measurement follow-up: October 18, 2026

### Dictation observed before and after

The October 9–16 daemon logs contain 59 delivered subscription dictations with
both a successful HTTP response and a `stop_to_paste_ms` value. Since the auth
preparation change, there are nine such deliveries, including five spoken
attempts in James's latest measurement run. Two additional attempts returned
empty transcripts and were retained in Recovery; James confirmed that those
were intentional silence tests. They are not included as successful deliveries.

| Cohort | Before median | After median | Observed reduction |
| --- | ---: | ---: | ---: |
| All successful dictations | 2,005 ms (59) | 1,381 ms (9) | 624 ms / 31% |
| Recordings shorter than 10 seconds | 1,742 ms (21) | 1,331 ms (7) | 411 ms / 24% |
| Recordings from 10 to 30 seconds | 1,958 ms (17) | 2,043 ms (2) | No improvement established |

These are observational comparisons, not a randomized A/B measurement. The
after recordings are shorter overall: median 3.98 seconds versus 17.91 seconds
before. Even within the short-recording bucket, their median lengths differ
(2.96 versus 5.95 seconds). Network conditions, spoken content and earlier
builds also vary. The table therefore does not establish that the change alone
caused a 24% or 31% improvement, or that longer recordings improved.

The latest five spoken attempts completed in 1,262–2,160 ms, with a median of
1,327 ms. All five were delivered. Across all nine successful after recordings,
the measured auth work took 125–1,047 ms and finished during recording:
`auth_wait_millis` was zero in every case. This directly verifies that the
foreground auth wait was removed for these recordings. It does not establish
the hypothetical duration of a separate auth lookup after each recording.

The existing `stop_to_paste_ms` name is broader than physical paste latency:
the timer begins when the daemon handles Stop and ends after delivery,
bookkeeping, audio cleanup and the final state update. It neither measures
hotkey dispatch before the handler nor when the destination paints the text.
Before and after use the same timer definition.

Response events were paired with delivered jobs on the same processing thread,
within that job's stop-to-completion interval. The comparison uses timing fields
only; no speech, credentials, recording files or raw runtime records are added
to this repository.

### Copilot measurement limits

There is no post-update meeting session in the logs. James chose to measure
dictation during this run, so there is no measured copilot speech-end-to-useful-
answer speedup. The earlier headless live comparison still has the unresolved
token-handoff limitation described above; it provides no model timing result.

Three focused tests were rerun successfully:

- Eight concurrent cold credential lanes need one successful auth load rather
  than potentially eight serialized loads. This is a request-count result under a
  mock provider, not an eightfold answer-speed improvement.
- A canceled answer worker finishes while its instant draft is still blocked
  and never starts research. The wait checks cancellation at 25 ms intervals;
  this does not abort an existing blocking network read.
- Quiet question timers schedule the actual pause/verdict deadline, removing
  the old maintenance tick's possible extra wait of almost 250 ms. Ordinary
  audio events may already have woken the old loop sooner.

### Next experiments, in priority order

1. **Profile and reduce the delivery tail.** After the successful transcription
   response, another 361–526 ms elapses before flow completion (median 462 ms).
   This includes overlay dismissal, paste and durable bookkeeping; it is not
   all avoidable delay. The delivery gate currently waits for the entire overlay
   helper process to exit, including a 150 ms fade hold. Add stage timings, then
   evaluate acknowledging an observed hidden window before asynchronous process
   cleanup, or retaining a hidden overlay host. Preserve focus checks, the
   bottom-center placement and duplicate-paste protection. Do not simply remove
   the gate or classify all 462 ms as a promised saving.
2. **Transcribe while speaking on the default route.** The optional API-key
   dictation path already uses continuous `gpt-live-transcribe`; the selected
   subscription path uploads a completed recording, and the copilot uses
   provisional file segments. A warm local streaming recognizer, or a separately
   authorized streaming service, could move more recognition before Stop and
   provide stable partial questions earlier. [OpenAI's realtime guide](https://developers.openai.com/api/docs/guides/realtime-transcription)
   distinguishes live transcription from transcription that begins at commit.
   API access is a separate route; do not assume that the subscription endpoint
   offers it or silently change billing. Evaluate French/English, names,
   numbers, continuations and silence, with the current cloud path as fallback.
3. **Give the copilot a shorter complete first answer.** It currently waits for
   up to two sentences and two bullets before starting the research that
   continues them. Compare one useful complete sentence, and a faster draft
   model, against the current answer for factual quality as well as latency.
   Independently gathering research facts in parallel is another experiment,
   but they must be reconciled with the actual draft before display. [OpenAI's
   latency guidance](https://developers.openai.com/api/docs/guides/latency-optimization)
   supports reducing generated output and overlapping independent work. Do not
   launch the existing continuation prompt from an incomplete draft or promise
   a first-token improvement merely from a lower output cap.

Keep the existing speculative question drafting and prompt priming: they are
already present. Shortening the silence gate alone may do little when model
generation is the limiting stage. The next copilot comparison should time the
first useful visible sentence as well as the final answer, with repeated
identical prompts/audio, alternating old/new order and unchanged model settings.
