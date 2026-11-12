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
- **By itself**, when **Opens** is set to **By itself when a call starts**.
  The copilot then opens as a call begins. If you end it during that call, it
  stays closed for the rest of the call; the dictation shortcut still reopens
  it.

A meeting is detected when a call app or browser (Zoom, Teams, Meet, Webex,
Slack, Discord, and others) is using the microphone, or when a meeting window
is open. **Opens** in Settings, under Meeting Copilot, chooses between **With
my dictation shortcut in calls** (the default), **By itself when a call
starts**, and **Only from the tray**, where the shortcut always dictates.

## Settings

The copilot's settings are in the AgentDictate window's **Settings**, under
**Meeting Copilot**. Seven are shown: **When to answer**, the **Answer
engine** with the key it needs (the OpenAI key is always offered, since it
also transcribes), **Meeting type**, **Opens**, **Save meeting notes**, **Your
name**, and **About you**. Everything else waits behind **Advanced**, which
starts closed, in four parts:

- **Answers:** Instant answers, Search the web, the full and instant answer
  models, Thinking time, the rewrite engine, and Custom instructions.
- **Audio and language:** Spoken language, Translate what they say, and
  **Don't transcribe my side**, which stops transcribing your microphone. The
  other side is always heard.
- **Privacy and screen:** Hide from screen sharing, Use my screen, Live
  insights, and Remember past meetings.
- **Connections:** the AssemblyAI key, the calendar feed, the webhook, Company
  knowledge, and, when that is Leadlord, its setup.

**Ask your documents** comes last under Advanced. During a call, the sliders
button on the bar opens a short sheet with **When to answer**, **Goal for this
meeting**, **Meeting type**, **Spoken language**, **Open knowledge folder**,
and **More settings**, which opens the AgentDictate window's Settings.

The copilot appears as a small bar at the top center of the primary monitor.
Drag any empty part of the bar to move it; it stays where you put it, in this
meeting and the next, and the panel moves up if it would open past the bottom
of the screen. Clicks beside
the bar and panel reach the window underneath. The copilot starts listening
immediately; the timer shows the session length and the meter shows audio from
both sides.

While a session is listening, its own icon sits in the notification area, even
when the window is hidden: click it to show or hide the copilot, or right-click
it to end the session. A session also ends on its own, with its wrap-up and
notes, a minute after the meeting it detected is over and the line goes quiet,
or after 15 minutes in which no one speaks.

## Shortcuts

| Shortcut | Action |
| --- | --- |
| <kbd>Ctrl</kbd> + <kbd>Enter</kbd> | **Ask AI**: what should I say right now, using the conversation and your screen |
| <kbd>Ctrl</kbd> + <kbd>\\</kbd> | Show or hide the copilot |
| <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>Enter</kbd> | Type a question |
| <kbd>Ctrl</kbd> + <kbd>Shift</kbd> + <kbd>Backspace</kbd> | **Catch me up**: what the last two minutes covered and what is waiting on you |
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
- **Rewrites.** Under the line to say, **Shorter**, **More casual**, **More
  specific**, and **Another way** rewrite it in about a second with the
  quickest model, or with Claude Sonnet through the `claude` CLI and your Claude
  subscription when **Rewrites** is set to **Claude Sonnet**; if Claude is
  unavailable, such as when the CLI is signed out, the quickest model rewrites
  instead. More specific draws on your documents and on what research
  added to the answer, never on invented figures. **‹ ›** switches between
  versions; copying and the meeting notes use the one you settle on.
- **Automatic answers.** When the other side asks you something and pauses
  for your reply, an answer appears without a keypress. The fast model judges
  each thing they say in context, so requests without a question mark ("walk
  me through your pricing") and objections count, while rhetorical questions,
  questions they answer themselves, remarks to someone else, and advice do
  not. A question is drafted the moment it is transcribed, while it is being
  judged, and shown only once confirmed; a rejected draft is discarded unseen,
  so checking adds almost no delay.
- **When to answer.** The first choice in the copilot's settings, both in the
  AgentDictate window's Settings under Meeting Copilot and at the top of the
  in-call settings sheet. **Automatically** answers as described above.
  **When I ask** shows an answer only when you press <kbd>Ctrl</kbd> +
  <kbd>Enter</kbd> or **Ask AI**; it is the fastest way to an answer, because
  nothing waits for the other side to stop talking, and the bar tints **Ask
  AI** to show the mode. Transcript, insights, and notes work the same in
  both.
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
- **Where a line comes from.** Under the line to say, a small mark reads
  **From your docs** when its facts come from your documents, profile, or the
  conversation, and **General knowledge** when they do not.
- **Several answers at once.** A new answer no longer stops the one being
  written: the earlier one keeps streaming just below it, so the first half of
  a two-part question is not lost.
- **Compact panel.** The **−** button on the bar shows only the line to say,
  and the copilot remembers it. **Show everything** brings the full
  panel back.
- **Goal and checklist.** Each meeting type has a short checklist (a sales
  call: their problem, budget, decision maker, timeline, next step). Type a
  **Goal for this meeting** in the in-call settings sheet, such as "get a second meeting, ask
  about budget", and each part joins the list. Items tick themselves off as
  the conversation covers them. The goal is cleared when the meeting ends.
- **You promised.** When you commit to something ("I'll send that Friday"),
  it appears under **You promised** and is saved with the notes as a
  commitment you can tick off later.
- **Alerts.** While you are only listening, the copilot alerts you when
  someone says your name, with what was said in the 30 seconds before and
  **What should I say?**. Set **Your name** in settings; it defaults to your
  Windows account name. It also flags a figure you state that your own
  documents contradict.
- **Notes.** Type `note: they renew in March` in the ask box to keep
  something. Notes appear in the transcript and are worked into the summary.
- **Before you start.** Once the copilot knows who is on the call, from a
  name in the conversation or your calendar, it shows the last meeting with
  them and what you still owe them.
- **Calendar.** Paste a private `.ics` address into **Calendar feed**, under
  Advanced, Connections (Outlook:
  Settings, Calendar, Shared calendars, Publish a calendar). The meeting under
  way then names the notes and offers its guests as names for the voices.
- **Translation.** Choose a language under **Translate what they say**, in
  Advanced, Audio and language.
  Lines in another language get a translation under them in the transcript,
  and the line to say comes with its meaning. **Spoken language** fixes the
  language transcription expects; it applies from the next meeting.
- **Wrap-up.** When a session ends on its own, the copilot writes notes
  with a summary, key points, decisions, action items, open questions, a
  section for the meeting type, and, when you took part in a conversation, a
  ready-to-send follow-up email with its subject line. When you end it
  yourself with **✕**, the bar closes at once and a small **Notes ready** bar
  appears when the notes are saved, with **Wrap-up** and **Draft email**; it
  leaves by itself after 25 seconds.
  The wrap-up shows them with **Draft email**, which opens the email as a
  draft in your mail app (Outlook, when it handles email links) and never sends
  it, **Copy email**, **Open notes**, and the full transcript one tab away. An
  email too long for a mail link opens with its subject, and its text waits on
  the clipboard to paste in; press **Done** when you are finished. Press **✕** again while the
  notes are being written to close as soon as they are saved. Notes, with the
  copilot's answers and the transcript, are saved in
  `%LOCALAPPDATA%\AgentDictate\meetings`.
- **Meetings.** Every saved meeting is listed, newest first, on the
  **Meetings** page of the AgentDictate window; the tray's **Meeting notes**
  opens it, and the **Overview** shows the latest three. Search finds meetings
  by every word you type, across titles, people, summaries, and transcripts.
  Open a meeting to read its summary and transcript, draft or copy its
  follow-up email, or delete it: **Delete**, then **Confirm delete**, moves its
  notes to the Recycle Bin, where they can be restored. Type a question in the
  search box and press <kbd>Enter</kbd> or **Ask** to have it answered from
  every saved meeting, with the meeting each fact comes from. **You still
  owe** lists unticked commitments across meetings; **People** filters the
  list to one person. An opened meeting also shows your commitments, your
  notes, and **How you spoke** (your share of the words, longest stretch,
  questions, filler words); **Copy notes** copies everything but the
  transcript.
- **Webhook.** Paste an address into **Send finished meetings to** and each
  finished meeting is posted there as JSON (title, people, summary,
  commitments, notes, transcript), for a CRM, Notion, Zapier, or Make.

The panel opens when there is something to show and collapses to the bar with
the chevron. While collapsed, new answers reopen it.

## Your knowledge

Choose **Company knowledge** in Settings, under Meeting Copilot, Advanced,
Connections: **Personal HQ**, **Leadlord**, or **None**.
With a brain selected, each answer reads up to two relevant live pages, retaining
their source and revision. Both the instant and researched answers receive those
excerpts, alongside your own documents. The separate **Remember past
meetings** preference still controls retrieval from earlier meetings. A failed
brain connection is reported and never treated as an empty brain.

To see what the copilot would say before a call, type a question under **Ask
your documents**, last under Advanced in the AgentDictate window's Settings.
The answer comes from
your knowledge folder alone and names the document each fact is from; when
nothing covers the question, it says so.

Personal HQ uses a separately approved, read-only HQ CLI profile named
`agentdictate`. Enroll it with `hq auth login --local --profile agentdictate
--label "AgentDictate Copilot" --scopes read`, and approve the code in HQ's
**Assistant access**. The profile retains the origin and credential; AgentDictate
does not copy the owner login or store the token in its settings. HQ must provide
the `/api/v1/brain/search` and `/api/v1/brain/pages/:id` read routes, which enforce
HQ's existing evidence and privacy eligibility rules.

`agentdictate copilot --brain-check` verifies the selected brain through the
production reader without opening the copilot, capturing audio/screens, sending
a model request, or printing private source text.

For Leadlord, choose it as Company knowledge, then enter the installed
`packages/hq-cli/src/hq-brain-index.ts` path in **Leadlord brain**, which
appears below it. Enter a Bitwarden Secrets Manager access token for an existing
appropriately scoped company machine account in **Bitwarden access token**, then
save. The masked token is stored with the owner-protected copilot settings and
redacted from settings logs. It is passed to the child CLI through
`BWS_ACCESS_TOKEN`, never command arguments or model prompts. Node, `bws`, and
the checkout's CLI dependencies must be available to the desktop process. The
copilot invokes only bounded `search` and `read` commands, without a shell;
the HQ CLI's underlying shared credential can still have broader permissions.
A changed revision requires a fresh read.

Answers about you and your business come from your own material first, so the
copilot quotes your real prices, offer, and results instead of guessing.

- **Knowledge folder.** In the in-call settings sheet, choose **Open
  knowledge folder** and add pricing sheets, product docs, case studies, FAQs, or your
  resume as Word (`.docx`), Markdown, text, or CSV files. Subfolders are read
  too. The folder is `%LOCALAPPDATA%\AgentDictate\knowledge`. The copilot
  knows your documents from the start: every answer is given them whole,
  smallest first, up to about 100,000 characters (some 25,000 words), and only
  what does not fit is searched for the parts that match the question. The
  session sends this to the answer models once when it starts, so the first
  question does not wait for them to read it. Changes are picked up within
  seconds, even mid-call. PDFs and slides cannot be read yet; the sheet lists any it skipped
  so you can save them as Word or text.
- **Past meetings.** With **Remember past meetings** on, answers also know what
  your earlier meetings concluded, newest first, and search their transcripts
  for details, such as what a client asked last time.
- **About you** and **Standing instructions** describe your role and how you
  like answers written; they apply to every answer.

When nothing in your material covers a question, the copilot answers from
general knowledge, and never invents facts about your business. It searches the
web only when it is not sure of an answer or the answer depends on facts that
change, such as current prices, news, or recent releases; `agentdictate copilot
--primer [instant-model]` checks this and the priming speed with the live
models.

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
through the naming model, `agentdictate copilot --rewrites [model | claude:sonnet]` rewrites a scripted
line in every style, and `agentdictate copilot --window [shot.jpg]`
reads the open meeting window (or a saved screenshot) once and prints who it
shows speaking. `agentdictate copilot --echo-test path\to\speech.wav` plays a recording
through your speakers while the copilot listens and reports anything that
reached **You**; run it in a quiet room without speaking.

## Answer engines

Choose the engine in Settings, under Meeting Copilot; the models are under
Advanced, Answers.

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
you pick, so they keep pace with the call. With the **OpenAI API** engine,
judging goes through OpenAI's Decisions API, which returns a probability
without writing a reply: about a quarter of a second, against more than a
second for asking the model. If that service fails, the model is asked.

**Faster transcription (optional).** Paste an **AssemblyAI API key** in
Settings, under Meeting Copilot, Advanced, Connections, and calls are transcribed by AssemblyAI from
the next meeting. Its model judges from the words that a speaker has finished,
a fraction of a second after they stop, so a question reaches the copilot
sooner than when a fixed pause has to pass. The copilot then shows its answer
after a shorter pause. Open sessions are billed by AssemblyAI by the minute;
the copilot closes them after ten minutes of quiet. If the service fails,
speech is uploaded at each pause as before.

**Thinking time**, under Advanced, Answers, sets how much the researched model
reasons before answering. Fast is recommended during live calls.

**Meeting type** (General, Sales call, Job interview, Hiring, Lecture,
Negotiation) tunes how answers are framed. Add your role, company, product,
and goals in **About you**, and standing rules in **Custom instructions**,
under Advanced, Answers.

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

- **"Can't hear them" on the bar:** you have been speaking for a while and no
  sound has come from the other side. The cause is the next item.
- **"Them" stays silent:** the copilot records the default playback device and,
  when it differs, the default communications device. If your call app plays
  through a third device, make it the Windows default.
- **Their words appear as "You":** run the echo test above. If it fails, check
  that the call app and Windows use the same speakers.
- **Claude Code is signed out:** run `claude auth login` in a terminal.
