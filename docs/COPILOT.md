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
Drag it by the round logo. It starts listening immediately; the timer shows the
session length and the meter shows audio from both sides.

## Shortcuts

| Shortcut | Action |
| --- | --- |
| <kbd>Ctrl</kbd> + <kbd>Enter</kbd> | **Ask AI**: what should I say right now, using the conversation and your screen |
| <kbd>Ctrl</kbd> + <kbd>\\</kbd> | Show or hide the copilot |
| <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>Enter</kbd> | Type a question |
| <kbd>Esc</kbd> | Stop typing and return focus to your meeting |

These shortcuts exist only while the copilot runs. If another app already owns
one, the copilot uses <kbd>Alt</kbd> as an extra modifier and shows the actual
keys on its buttons. While the copilot runs, <kbd>Ctrl</kbd> + <kbd>Enter</kbd>
does not reach other apps, including chat apps that use it to send.

## What you see

- **Instant and researched answers.** Every answer streams in two lanes. A fast
  model drafts a line you can say in about a second. Meanwhile the main model
  researches, using web search when needed, and streams a verified answer with
  sources underneath. The instant draft never states specific prices, versions,
  dates, or news it cannot verify.
- **Automatic answers.** When the other side asks you a question, an answer
  starts without a keypress. Turn this off with **Answer questions
  automatically**.
- **Live insights.** The current topic, a short summary, any unanswered
  question directed at you, and suggested prompts you can click.
- **Quick actions.** What should I say?, Follow-up questions, Recap so far,
  Fact-check, and Explain my screen.
- **Transcript.** Everything said, labeled **You** (your microphone) and
  **Them** (system audio).
- **Meeting notes.** When you end the session with **✕**, the copilot writes a
  summary with key points, decisions, and action items, followed by the
  copilot's answers and the full transcript. Notes are saved in
  `%LOCALAPPDATA%\AgentDictate\meetings`; open the folder from the tray with
  **Meeting notes**.

The panel opens when there is something to show and collapses to the bar with
the chevron. While collapsed, new answers reopen it.

## Answer engines

Choose the engine and models in the copilot settings (the sliders button).

| Engine | Setup | Notes |
| --- | --- | --- |
| **ChatGPT** (default) | None. Uses the ChatGPT sign-in AgentDictate already uses. | Default models: GPT-6 Sol for researched answers, GPT-6 Luna for instant answers and insights. The model list comes from your account. |
| **Claude API** | Paste an Anthropic API key in settings. | Claude Opus 5.5 by default, with Anthropic's web search tool. Sonnet 5.5, Haiku 5.5, and Fable 5.1 are also offered. |
| **Claude Code** | Install Claude Code and run `claude auth login`. | Uses your Claude subscription through the `claude` CLI with its web tools. Opus by default. |
| **OpenAI API** | Paste an OpenAI API key, or reuse the dictation key. | Uses the OpenAI Responses API with web search. |

**Speed** sets how much the researched model reasons before answering. Fast is
recommended during live calls.

**Meeting type** (General, Sales call, Job interview, Hiring, Lecture,
Negotiation) tunes how answers are framed. Add your role, company, product,
and goals in **About you**, and standing rules in **Standing instructions**.

## Privacy and consent

- Transcription uses the ChatGPT dictation service through your sign-in, or the
  OpenAI API if you configured a key. Answers go to the engine you choose.
  Screenshots are sent only for Ask AI, typed questions, and Explain my screen,
  and only while **Use my screen** is on.
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

- **"Them" stays silent:** the copilot records the default playback device.
  If your call plays through a different device, make it the Windows default.
- **Your words appear as "Them":** use headphones. With speakers, the copilot
  removes most echo but can miss some.
- **Claude Code is signed out:** run `claude auth login` in a terminal.
