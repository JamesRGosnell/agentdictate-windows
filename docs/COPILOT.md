# Meeting Copilot

The meeting copilot is a live assistant for calls on Windows. It listens to
both sides of a conversation, answers questions as they are asked, searches the
web when facts matter, and stays invisible to screen sharing.

## Start a session

- **In a call, press your dictation shortcut** (default <kbd>Ctrl</kbd> +
  <kbd>Space</kbd>). When AgentDictate detects a meeting, the shortcut opens the
  copilot instead of dictating. Once the copilot is running, the shortcut
  dictates as usual, so you can still dictate into the meeting chat.
- **From the tray**, choose **Meeting Copilot**.
- **From a terminal**, run `agentdictate copilot`.

A meeting is detected when a call app or browser (Zoom, Teams, Meet, Webex,
Slack, Discord, and others) is using the microphone, or when a meeting window
is open. Turn this off with **Open with the dictation shortcut in meetings** in
the copilot settings.

The copilot appears as a small bar at the top center of the primary monitor.
Drag any empty part of the bar to move it; it stays where you put it, and the
panel moves up if it would open past the bottom of the screen. Clicks beside
the bar and panel reach the window underneath. The copilot starts listening
immediately; the timer shows the session length and the meter shows audio from
both sides.

## Shortcuts

| Shortcut | Action |
| --- | --- |
| <kbd>Ctrl</kbd> + <kbd>Enter</kbd> | **Ask AI**: what should I say right now, using the conversation and your screen |
| <kbd>Ctrl</kbd> + <kbd>\\</kbd> | Show or hide the copilot |
| <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>Enter</kbd> | Type a question |
| <kbd>Esc</kbd> | Stop typing and return focus to your meeting |

These shortcuts exist only while the copilot runs. If another app already owns
one, the copilot uses <kbd>Alt</kbd> as an extra modifier and shows the actual
keys on its buttons. <kbd>\\</kbd> means the key that types a backslash in your
current keyboard layout: on Canadian French, show or hide is <kbd>Ctrl</kbd> +
<kbd>Shift</kbd> + the key left of <kbd>1</kbd>. Switching layouts moves the
shortcut with it. While the copilot runs, <kbd>Ctrl</kbd> + <kbd>Enter</kbd>
does not reach other apps, including chat apps that use it to send.

## What you see

- **Instant and researched answers.** Every answer streams in two lanes. A fast
  model drafts a line you can say in about a second. Meanwhile the main model
  researches, using web search when needed, and streams a verified answer with
  sources underneath. The instant draft never states specific prices, versions,
  dates, or news it cannot verify.
- **Automatic answers.** When the other side asks you something and pauses
  for your reply, an answer appears without a keypress. The fast model judges
  each thing they say in context, so requests without a question mark ("walk
  me through your pricing") and objections count, while rhetorical questions,
  questions they answer themselves, remarks to someone else, and advice do
  not. A question is drafted the moment it is transcribed, while it is being
  judged, and shown only once confirmed; a rejected draft is discarded unseen,
  so checking adds almost no delay. Turn this off with **Answer questions
  automatically**.
- **Live insights.** The current topic, a short summary, any unanswered
  question directed at you as a card you can click to answer, and suggested
  prompts. Insights also tell a conversation you take part in from a
  presentation or a video or podcast you are watching; in the latter two,
  automatic answers pause and the panel says so, while insights and notes
  continue.
- **Quick actions.** What should I say?, Follow-up questions, Recap so far,
  Fact-check, and Explain my screen.
- **Transcript.** Everything said, labeled **You** (your microphone) and
  **Them** (system audio). When more than one person speaks on the other side,
  the copilot tells their voices apart and labels them **Them A**, **Them B**,
  and so on, in the transcript, the notes, and what answers see. Voices are
  matched by timbre and pitch, on this PC; people who talk over each other or
  trade lines without a pause share one label.
- **Names.** Labels become people's names as soon as they are clear: someone
  introduces themselves ("Hi, I'm Sam"), answers after being addressed by
  name, or is highlighted as speaking in the meeting app's window (Teams, Zoom,
  Meet, and others) while their voice is heard. A name from the window needs
  two sightings, or one when the call shows a single other person. Names
  appear in the transcript, answers, notes, and the follow-up email. Click a
  name or **Them B** in the transcript, or a chip in its **People** row, to
  type a name or pick one shown on screen; your names always win, and
  **Forget** clears one.
- **Wrap-up.** When you end the session with **✕**, the copilot writes notes
  with a summary, key points, decisions, action items, and, when you took part
  in a conversation, a ready-to-send follow-up email. The wrap-up shows them
  with **Copy follow-up email**, **Open notes**, and the full transcript one
  tab away; press **Done** when you are finished. Press **✕** again while the
  notes are being written to close as soon as they are saved. Notes, with the
  copilot's answers and the transcript, are saved in
  `%LOCALAPPDATA%\AgentDictate\meetings`.
- **Meetings.** Every saved meeting is listed, newest first, on the
  **Meetings** page of the AgentDictate window; the tray's **Meeting notes**
  opens it. Open a meeting to read its summary and transcript or copy its
  follow-up email.

The panel opens when there is something to show and collapses to the bar with
the chevron. While collapsed, new answers reopen it.

## Your knowledge

Answers about you and your business come from your own material first, so the
copilot quotes your real prices, offer, and results instead of guessing.

- **Knowledge folder.** In the copilot settings, choose **Open knowledge
  folder** and add pricing sheets, product docs, case studies, FAQs, or your
  resume as Word (`.docx`), Markdown, text, or CSV files. Subfolders are read
  too. The folder is `%LOCALAPPDATA%\AgentDictate\knowledge`. A small set of
  documents is given whole to every answer; a larger one is searched for the
  parts that match the question. Changes are picked up within seconds, even
  mid-call. PDFs and slides cannot be read yet; settings lists any it skipped
  so you can save them as Word or text.
- **Past meetings.** With **Remember past meetings** on, answers can also draw
  on notes from your earlier meetings, such as what a client asked last time.
- **About you** and **Standing instructions** describe your role and how you
  like answers written; they apply to every answer.

When nothing in your material covers a question, the copilot answers from
general knowledge and web search, and never invents facts about your business.

## Speakers and echo

Headphones are not required. When the call plays through speakers, the
microphone also hears the other side; the copilot removes that echo so their
words are not written as **You**:

- **Windows echo cancellation.** The copilot records the microphone the way
  call apps do, in communications mode, so Windows applies its voice
  processing: echo cancellation and noise suppression from the device or
  Voice Clarity. It points echo cancellation at the device the call plays
  through and does not trigger Windows' communications ducking.
- **Echo gate.** The copilot also hears exactly what the speakers play. Every
  20 ms it compares the microphone with that output and removes audio the
  speakers explain, while your voice, even over the other side, passes. This
  covers PCs without Windows echo cancellation and any residue.
- **Transcript check.** A line from the microphone whose words mostly repeat
  what the other side was saying at that moment is dropped, including fragments
  that span two of their sentences. Replies you start after they finish are
  kept, even when they repeat their words.

`agentdictate copilot --check` reports whether Windows echo cancellation is
active. `agentdictate copilot --voices a.wav b.wav` shows how recordings'
voices are told apart. `agentdictate copilot --names` runs scripted calls
through the naming model, and `agentdictate copilot --window [shot.jpg]`
reads the open meeting window (or a saved screenshot) once and prints who it
shows speaking. `agentdictate copilot --echo-test path\to\speech.wav` plays a recording
through your speakers while the copilot listens and reports anything that
reached **You**; run it in a quiet room without speaking.

## Answer engines

Choose the engine and models in the copilot settings (the sliders button).

| Engine | Setup | Notes |
| --- | --- | --- |
| **ChatGPT** (default) | None. Uses the ChatGPT sign-in AgentDictate already uses. | GPT-6 Sol by default for both researched answers (with web search) and instant answers (without, in about a second). The model list comes from your account. |
| **Claude API** | Paste an Anthropic API key in settings. | Claude Opus 5.5 by default, with Anthropic's web search tool. Sonnet 5.5, Haiku 5.5, and Fable 5.1 are also offered. |
| **Claude Code** | Install Claude Code and run `claude auth login`. | Uses your Claude subscription through the `claude` CLI with its web tools. Opus by default. |
| **OpenAI API** | Paste an OpenAI API key, or reuse the dictation key. | Uses the OpenAI Responses API with web search. |

Models are labeled from measured instant-draft times: **Fastest** (under a
second, smaller models), **Fast** (about a second, larger models that know
more), and **Slower** (several seconds). Fast models make the best instant
answers: they start as quickly and draw on more world knowledge. Judging what
to answer and live insights always use the engine's fastest model, whatever
you pick, so they keep pace with the call.

**Speed** sets how much the researched model reasons before answering. Fast is
recommended during live calls.

**Meeting type** (General, Sales call, Job interview, Hiring, Lecture,
Negotiation) tunes how answers are framed. Add your role, company, product,
and goals in **About you**, and standing rules in **Standing instructions**.

## Privacy and consent

- Transcription uses the ChatGPT dictation service through your sign-in, or the
  OpenAI API if you configured a key. Answers go to the engine you choose.
  Screenshots are sent only for Ask AI, typed questions, and Explain my screen,
  and only while **Use my screen** is on. With it on, the meeting app's window
  is also read while the other side speaks, until everyone has a name: at most
  every few seconds and 40 times a session, and paused while no call is in
  view.
- Audio is not saved. Meeting notes are saved only while **Save meeting notes**
  is on.
- The copilot hides itself from screen sharing, recordings, and screenshots
  (`WDA_EXCLUDEFROMCAPTURE`). Turn off **Hide from screen sharing** to show it
  in a share. It is still visible to anyone looking at your screen or recording
  it with a camera.
- **Recording a conversation can require everyone's consent.** Several US
  states and many countries require all parties to agree to recording or
  transcription. Tell participants when you use AI notes.

## Troubleshooting

Run the built-in check. It signs in, lists models, transcribes a recording if
you provide one, streams a researched answer and live insights, captures the
screen, and briefly opens both audio sources:

```powershell
agentdictate copilot --check
agentdictate copilot --check path\to\speech.wav --latency
```

`--latency` also times each model with and without web search. The copilot
logs to `%LOCALAPPDATA%\AgentDictate\logs\copilot.log*`.

- **"Them" stays silent:** the copilot records the default playback device and,
  when it differs, the default communications device. If your call app plays
  through a third device, make it the Windows default.
- **Their words appear as "You":** run the echo test above. If it fails, check
  that the call app and Windows use the same speakers.
- **Claude Code is signed out:** run `claude auth login` in a terminal.
