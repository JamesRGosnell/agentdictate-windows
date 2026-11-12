//! The meeting copilot: a floating control bar with a live assistance panel.

use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agentdictate_core::{
    AlertKind, AnswerStatus, CopilotAction, CopilotEvent, CopilotMode, CopilotSettings, FollowUp,
    Grounding, LANGUAGES, ListenState, QuickAction, RewriteStyle, Situation, Speaker,
    SpeakerLabels, TranscriptLine, VoiceNames, combined_answer, format_offset, given_name,
    has_several_voices, researched_follow_up,
};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, Entity, FontWeight, Hsla, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Render, ScrollHandle, SharedString,
    Stateful, Subscription, WeakEntity, Window, WindowControlArea, canvas, div, prelude::*, px,
    rems, svg,
};
use gpui_component::{
    h_flex,
    highlighter::HighlightTheme,
    input::{Input, InputEvent, InputState},
    text::{TextView, TextViewStyle},
    v_flex,
};

use crate::copilot::{
    AnswerCardModel, COPILOT_BAR_HEIGHT, COPILOT_PANEL_GAP, COPILOT_PANEL_HEIGHT,
    COPILOT_TOP_MARGIN, COPILOT_WIDTH, CopilotEffect, CopilotShape, CopilotViewModel,
    CopilotWindowHost, source_host,
};

#[cfg(windows)]
const FONT: &str = "Segoe UI";
#[cfg(not(windows))]
const FONT: &str = "Sans";

const GLASS: u32 = 0x111114f2;
const GLASS_RAISED: u32 = 0x1c1c21ff;
const BORDER: u32 = 0xffffff1a;
const HAIRLINE: u32 = 0xffffff12;
const HOVER: u32 = 0xffffff14;
const TEXT: u32 = 0xf4f4f5ff;
const MUTED: u32 = 0xa1a1aaff;
const DIM: u32 = 0x71717aff;
const ACCENT: u32 = 0xe6a48aff;
const ACCENT_SOFT: u32 = 0xe6a48a29;
const ME: u32 = 0x8ab4f8ff;
/// Colors for the other side's voices, Them A first.
/// None is close to the blue used for "You".
const VOICES: [u32; 6] = [
    ACCENT, 0xc4b5fdff, 0x86efacff, 0xfcd34dff, 0xf9a8d4ff, 0x67e8f9ff,
];
const DANGER: u32 = 0xf87171ff;
const WARNING: u32 = 0xfbbf24ff;
const SHADOW_MARGIN: f32 = 14.0;
const PILL_HEIGHT: f32 = 38.0;
const PANEL_RADIUS: f32 = 18.0;
/// The shortest the live panel shrinks to around little content.
const LIVE_PANEL_MIN_HEIGHT: f32 = 160.0;
/// The compact panel: room for the line to say and nothing else.
const COMPACT_PANEL_HEIGHT: f32 = 132.0;
/// How long the end button waits for its confirming second click.
const END_CONFIRM: Duration = Duration::from_secs(4);
const NOTICE_TTL: Duration = Duration::from_secs(4);

fn color(hex: u32) -> Hsla {
    gpui::rgba(hex).into()
}

/// A few words for the bar about a lasting problem; the panel shows the
/// full message.
fn problem_label(problem: &str) -> &'static str {
    let lower = problem.to_lowercase();
    if lower.contains("limit") {
        "Transcription limit reached"
    } else if lower.contains("hear the other side") {
        "Can't hear them"
    } else if lower.contains("insights") {
        "Insights failing"
    } else {
        "Not transcribing"
    }
}

fn icon(name: &str, size: f32, tint: u32) -> impl IntoElement + use<> {
    svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(px(size))
        .flex_none()
        .text_color(color(tint))
}

/// A shortcut's keycap labels as words for a sentence, like "Ctrl+Enter".
pub(super) fn shortcut_text(labels: &[String]) -> String {
    labels
        .iter()
        .map(|key| {
            if key == "\u{21b5}" {
                "Enter"
            } else {
                key.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

fn keycaps(labels: &[String]) -> impl IntoElement + use<> {
    h_flex().gap(px(3.)).children(labels.iter().map(|label| {
        div()
            .min_w(px(16.))
            .h(px(16.))
            .px(px(4.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .bg(color(0xffffff12))
            .border_1()
            .border_color(color(0xffffff1c))
            .text_size(px(10.))
            .text_color(color(MUTED))
            .child(label.clone())
    }))
}

fn chip(id: impl Into<gpui::ElementId>, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(28.))
        .px(px(11.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .rounded(px(14.))
        .border_1()
        .text_size(px(12.))
        .cursor_pointer()
        .when(selected, |chip| {
            chip.bg(color(ACCENT_SOFT))
                .border_color(color(0xe6a48a66))
                .text_color(color(TEXT))
        })
        .when(!selected, |chip| {
            chip.bg(color(0xffffff08))
                .border_color(color(HAIRLINE))
                .text_color(color(MUTED))
                .hover(|style| style.bg(color(HOVER)).text_color(color(TEXT)))
        })
}

/// Bar buttons occlude the bar's drag area so clicks reach them.
fn bar_button(id: &'static str) -> Stateful<Div> {
    div()
        .debug_selector(move || id.to_owned())
        .id(id)
        .occlude()
        .h(px(30.))
        .px(px(8.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .rounded(px(15.))
        .cursor_pointer()
        .text_color(color(TEXT))
        .hover(|style| style.bg(color(HOVER)))
}

fn icon_button(id: &'static str, name: &'static str, tint: u32) -> Stateful<Div> {
    div()
        .debug_selector(move || id.to_owned())
        .id(id)
        .occlude()
        .size(px(28.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .hover(|style| style.bg(color(HOVER)))
        .child(icon(name, 14., tint))
}

/// A speaker's label in the narrow transcript column: a given name, unless
/// two people share it.
fn short_label(labels: &SpeakerLabels, names: &VoiceNames, line: &TranscriptLine) -> String {
    match labels.name(line) {
        Some(name) => {
            let given = given_name(name);
            let shared = names
                .values()
                .filter(|other| given_name(other).eq_ignore_ascii_case(given))
                .count()
                > 1;
            if shared { name } else { given }.to_owned()
        }
        None => labels.display(line),
    }
}

/// A small mark saying where a line's facts come from.
fn grounding_mark(grounding: Grounding) -> impl IntoElement + use<> {
    let mine = grounding == Grounding::Mine;
    h_flex()
        .gap(px(4.))
        .text_color(color(if mine { ACCENT } else { DIM }))
        .child(icon(
            if mine { "check" } else { "globe" },
            10.,
            if mine { ACCENT } else { DIM },
        ))
        .child(grounding.label())
}

fn divider() -> impl IntoElement + use<> {
    div().w(px(1.)).h(px(16.)).mx(px(2.)).bg(color(BORDER))
}

/// How much the copilot knows beyond the conversation.
fn knowledge_status(knowledge: &agentdictate_core::KnowledgeSummary) -> String {
    let plural =
        |count: usize, word: &str| format!("{count} {word}{}", if count == 1 { "" } else { "s" });
    if knowledge.documents == 0 && knowledge.meetings == 0 {
        return "Add pricing, product docs, case studies, or your resume to the knowledge \
folder as Word, Markdown, or text files. Answers about you and your business come from them first."
            .to_owned();
    }
    let mut parts = Vec::new();
    if knowledge.documents > 0 {
        parts.push(plural(knowledge.documents, "document"));
    }
    if knowledge.meetings > 0 {
        parts.push(plural(knowledge.meetings, "past meeting"));
    }
    parts.push(plural(knowledge.words, "word"));
    format!(
        "{}. Answers about you and your business come from these first.",
        parts.join(" \u{00b7} ")
    )
}

/// What the panel says the copilot is listening to, and whether that has
/// paused automatic answers.
fn situation_label(situation: Situation, auto_answer: bool) -> Option<&'static str> {
    match (situation, auto_answer) {
        (Situation::Unclear, _) => None,
        (Situation::Conversation, _) => Some("Conversation"),
        (Situation::Presentation, true) => Some("Presentation \u{00b7} automatic answers paused"),
        (Situation::Presentation, false) => Some("Presentation"),
        (Situation::Media, true) => Some("Video or podcast \u{00b7} automatic answers paused"),
        (Situation::Media, false) => Some("Video or podcast"),
    }
}

fn section_label(text: &'static str) -> impl IntoElement + use<> {
    div()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color(DIM))
        .child(text.to_uppercase())
}

fn pulse(period_ms: f64) -> f32 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64;
    (0.5 + 0.5 * (millis / period_ms * std::f64::consts::TAU).sin()) as f32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Live,
    Transcript,
}

pub type CopilotActionSink = Arc<dyn Fn(CopilotAction) + Send + Sync>;

/// Whether an answer already covers the question insights detected. Insights
/// reword the question to stand alone, so the texts rarely match exactly.
fn covers_question(title: &str, question: &str) -> bool {
    title.eq_ignore_ascii_case(question)
        || agentdictate_core::utterance_similarity(title, question) >= 0.4
}

pub struct CopilotWindow {
    model: CopilotViewModel,
    actions: CopilotActionSink,
    host: Rc<dyn CopilotWindowHost>,
    tab: Tab,
    panel_open: bool,
    /// The user collapsed the panel; insights no longer reopen it.
    user_collapsed: bool,
    settings_open: bool,
    visible: bool,
    typing: bool,
    ask: Entity<InputState>,
    /// The voice being named, from the transcript.
    naming: Option<u8>,
    name_input: Entity<InputState>,
    /// What the user wants from this meeting.
    goal: Entity<InputState>,
    /// The brief was closed for this session.
    brief_closed: bool,
    body_scroll: ScrollHandle,
    transcript_scroll: ScrollHandle,
    expanded: HashSet<u64>,
    window_size: Option<(f32, f32)>,
    capture_excluded: bool,
    notice_at: Option<Instant>,
    shape: Rc<RefCell<ShapeTracker>>,
    live_fit: Rc<Cell<LiveFit>>,
    /// The end button was clicked once; a second click ends the meeting.
    end_armed_at: Option<Instant>,
    _subscriptions: Vec<Subscription>,
}

/// Measured heights that let the live panel shrink to its content, so it
/// covers no more of the meeting than it needs to.
#[derive(Clone, Copy, Default, PartialEq)]
struct LiveFit {
    /// Header, footer, and border: the panel height minus the body viewport.
    chrome: Option<f32>,
    content: Option<f32>,
}

impl LiveFit {
    fn height(self) -> Option<f32> {
        Some(
            (self.chrome? + self.content?)
                .ceil()
                .clamp(LIVE_PANEL_MIN_HEIGHT, COPILOT_PANEL_HEIGHT),
        )
    }
}

#[derive(Clone, Copy)]
enum FitProbe {
    /// The body viewport, laid out inside a panel of this height.
    Viewport {
        panel_height: f32,
    },
    Content,
}

/// An invisible element that reports its parent's height for [`LiveFit`],
/// re-rendering the copilot on a later turn when a height changes.
fn fit_probe(
    fit: &Rc<Cell<LiveFit>>,
    view: WeakEntity<CopilotWindow>,
    probe: FitProbe,
) -> impl IntoElement + use<> {
    let fit = Rc::clone(fit);
    canvas(
        move |bounds, _, cx| {
            let height = f32::from(bounds.size.height);
            let mut next = fit.get();
            let slot = match probe {
                FitProbe::Viewport { panel_height } => {
                    next.chrome = Some(panel_height - height);
                    next.chrome
                }
                FitProbe::Content => {
                    next.content = Some(height);
                    next.content
                }
            };
            let previous = match probe {
                FitProbe::Viewport { .. } => fit.get().chrome,
                FitProbe::Content => fit.get().content,
            };
            if previous.zip(slot).is_some_and(|(a, b)| (a - b).abs() < 0.5) {
                return;
            }
            let resized = next.height() != fit.get().height();
            fit.set(next);
            if resized {
                let view = view.clone();
                cx.spawn(async move |cx| {
                    let _ = view.update(cx, |_, cx| cx.notify());
                })
                .detach();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// Measured bounds of the bar and panel, applied to the host as the window's
/// clickable shape whenever they change.
#[derive(Default)]
struct ShapeTracker {
    bar: Option<Bounds<Pixels>>,
    panel: Option<Bounds<Pixels>>,
    /// The window scale; the host region is in device pixels, so moving to a
    /// monitor with different scaling must reapply the same logical shape.
    scale: f32,
    applied: Option<(Vec<CopilotShape>, f32)>,
}

impl ShapeTracker {
    fn shapes(&self) -> Vec<CopilotShape> {
        // Probes measure inside the 1px border of the bar and panel.
        let shape = |bounds: Bounds<Pixels>, radius: f32| CopilotShape {
            x: f32::from(bounds.origin.x) - 1.0,
            y: f32::from(bounds.origin.y) - 1.0,
            width: f32::from(bounds.size.width) + 2.0,
            height: f32::from(bounds.size.height) + 2.0,
            radius,
        };
        self.bar
            .map(|bounds| shape(bounds, PILL_HEIGHT / 2.0))
            .into_iter()
            .chain(self.panel.map(|bounds| shape(bounds, PANEL_RADIUS)))
            .collect()
    }
}

/// Applies the tracked shape on a later turn of the event loop, since the
/// window region change sends Win32 messages back into GPUI.
fn apply_shape(tracker: &Rc<RefCell<ShapeTracker>>, host: &Rc<dyn CopilotWindowHost>, cx: &App) {
    let applied = {
        let tracker = tracker.borrow();
        (tracker.shapes(), tracker.scale)
    };
    if applied.0.is_empty() || tracker.borrow().applied.as_ref() == Some(&applied) {
        return;
    }
    let shapes = applied.0.clone();
    tracker.borrow_mut().applied = Some(applied);
    let host = Rc::clone(host);
    cx.spawn(async move |_| host.set_shape(&shapes)).detach();
}

/// An invisible element that reports its parent's bounds for the window shape.
fn shape_probe(
    tracker: &Rc<RefCell<ShapeTracker>>,
    host: &Rc<dyn CopilotWindowHost>,
    panel: bool,
) -> impl IntoElement + use<> {
    let tracker = Rc::clone(tracker);
    let host = Rc::clone(host);
    canvas(
        move |bounds, window, cx| {
            {
                let mut shape = tracker.borrow_mut();
                shape.scale = window.scale_factor();
                if panel {
                    shape.panel = Some(bounds);
                } else {
                    shape.bar = Some(bounds);
                }
            }
            apply_shape(&tracker, &host, cx);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

impl CopilotWindow {
    pub fn new(
        model: CopilotViewModel,
        actions: CopilotActionSink,
        host: Rc<dyn CopilotWindowHost>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let ask = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(
                    "Ask anything, or type \u{201c}note: \u{2026}\u{201d} to keep something",
                )
                .clean_on_escape()
        });
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Their name"));
        let goal = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g. Get a second meeting, ask about budget")
        });
        let mut subscriptions = vec![cx.subscribe_in(
            &ask,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let question = input.read(cx).value().trim().to_owned();
                    if !question.is_empty() {
                        input.update(cx, |input, cx| input.set_value("", window, cx));
                        (this.actions)(CopilotAction::Ask(question));
                        this.tab = Tab::Live;
                        this.settings_open = false;
                        this.panel_open = true;
                        this.stop_typing(true, cx);
                        cx.notify();
                    }
                }
            },
        )];
        subscriptions.push(cx.subscribe_in(
            &name_input,
            window,
            |this, input, event: &InputEvent, _, cx| {
                if let (InputEvent::PressEnter { .. }, Some(voice)) = (event, this.naming) {
                    let name = input.read(cx).value().to_string();
                    this.name_voice(voice, &name, cx);
                }
            },
        ));
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() && this.typing {
                this.stop_typing(false, cx);
                cx.notify();
            }
        }));
        let capture_excluded = model.settings.hide_from_screen_share;
        let mut view = Self {
            model,
            actions,
            host,
            tab: Tab::Live,
            // The session starts as the bar alone so it covers as little of
            // the meeting as possible; content opens the panel.
            panel_open: false,
            user_collapsed: false,
            settings_open: false,
            visible: true,
            typing: false,
            ask,
            naming: None,
            name_input,
            goal,
            brief_closed: false,
            body_scroll: ScrollHandle::new(),
            transcript_scroll: ScrollHandle::new(),
            expanded: HashSet::new(),
            window_size: None,
            capture_excluded,
            notice_at: None,
            shape: Rc::default(),
            live_fit: Rc::default(),
            end_armed_at: None,
            _subscriptions: subscriptions,
        };
        view.load_text_settings(window, cx);
        view.start_ticker(cx);
        view
    }

    /// Redraws at a modest rate for meters, the timer, and status pulses.
    fn start_ticker(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(90))
                    .await;
                let Ok(animating) = this.update(cx, |this, cx| {
                    if this.notice_at.is_some_and(|at| at.elapsed() >= NOTICE_TTL) {
                        this.notice_at = None;
                        this.model.notice = None;
                    }
                    if this
                        .end_armed_at
                        .is_some_and(|at| at.elapsed() >= END_CONFIRM)
                    {
                        this.end_armed_at = None;
                        cx.notify();
                    }
                    let animating = this.visible
                        && (matches!(
                            this.model.listen,
                            ListenState::Listening | ListenState::Starting
                        ) || this.model.answer_active()
                            || this.model.ending);
                    if animating {
                        cx.notify();
                    }
                    animating
                }) else {
                    return;
                };
                if !animating {
                    cx.background_executor()
                        .timer(Duration::from_millis(250))
                        .await;
                }
            }
        })
        .detach();
    }

    fn load_text_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.model.settings.meeting_goal.clone();
        self.goal
            .update(cx, |input, cx| input.set_value(goal, window, cx));
    }

    pub fn apply(&mut self, event: CopilotEvent, window: &mut Window, cx: &mut Context<Self>) {
        let had_notice = self.model.notice.clone();
        let settings_event = matches!(event, CopilotEvent::Settings(_));
        let answer_started = matches!(event, CopilotEvent::AnswerStarted { .. });
        let insights = matches!(event, CopilotEvent::Insights(_));
        let quiet = matches!(event, CopilotEvent::NotesSaved { quiet: true, .. });
        let ending = !quiet
            && matches!(
                event,
                CopilotEvent::Ending | CopilotEvent::NotesSaved { .. }
            );
        let alert = matches!(event, CopilotEvent::Alert(_));
        if answer_started
            && let Some(latest) = self.model.answers.last()
            && (latest.status.is_active() || latest.instant_active)
        {
            // The answer being written keeps showing in full below the new
            // one.
            self.expanded.insert(latest.id);
        }
        match self.model.apply(event) {
            CopilotEffect::None => {}
            CopilotEffect::ToggleVisibility => self.set_visible(!self.visible, cx),
            CopilotEffect::Show => self.set_visible(true, cx),
            CopilotEffect::FocusAsk => {
                self.panel_open = true;
                self.settings_open = false;
                self.start_typing(window, cx);
            }
            CopilotEffect::ScrollTranscript => {
                if self.tab == Tab::Transcript {
                    self.transcript_scroll.scroll_to_bottom();
                }
            }
            CopilotEffect::Exit => {
                cx.quit();
                return;
            }
        }
        if answer_started {
            // New answers appear at the top of the live panel.
            self.panel_open = true;
            self.settings_open = false;
            self.tab = Tab::Live;
            self.body_scroll.set_offset(gpui::point(px(0.), px(0.)));
        }
        if alert {
            // An alert is worth interrupting for, hidden or collapsed.
            self.panel_open = true;
            self.settings_open = false;
            self.tab = Tab::Live;
            self.body_scroll.set_offset(gpui::point(px(0.), px(0.)));
            self.set_visible(true, cx);
        }
        // Compact mode opens for a line to say, not for insights.
        if insights && !self.panel_open && !self.user_collapsed && !self.model.settings.compact {
            self.panel_open = true;
        }
        if quiet {
            // The meeting was ended by hand: only a small bar says the notes
            // are ready.
            self.panel_open = false;
            self.settings_open = false;
            self.tab = Tab::Live;
            self.stop_typing(false, cx);
            self.set_visible(true, cx);
        }
        if ending {
            // The wrap-up is shown even if the copilot was hidden or collapsed.
            self.panel_open = true;
            self.settings_open = false;
            self.tab = Tab::Live;
            self.stop_typing(false, cx);
            self.set_visible(true, cx);
        }
        if settings_event && !self.settings_open {
            self.load_text_settings(window, cx);
        }
        if self.model.notice.is_some() && self.model.notice != had_notice {
            self.notice_at = Some(Instant::now());
        }
        cx.notify();
    }

    /// Opens the settings sheet.
    pub fn show_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.panel_open = true;
        self.load_text_settings(window, cx);
        cx.notify();
    }

    /// Switches the panel to the live transcript.
    pub fn show_transcript(&mut self, cx: &mut Context<Self>) {
        self.tab = Tab::Transcript;
        self.settings_open = false;
        self.panel_open = true;
        cx.notify();
    }

    pub fn set_panel_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.panel_open = open;
        cx.notify();
    }

    fn act(&self, action: CopilotAction) {
        (self.actions)(action);
    }

    fn update_settings(&mut self, change: impl FnOnce(&mut CopilotSettings)) {
        let mut settings = self.model.settings.clone();
        change(&mut settings);
        self.model.settings = settings.clone();
        self.act(CopilotAction::UpdateSettings(Box::new(settings)));
    }

    fn save_text_settings(&mut self, cx: &mut Context<Self>) {
        // The goal is the sheet's only text; the rest is edited in Settings.
        let goal = self.goal.read(cx).value().trim().to_owned();
        self.update_settings(|settings| settings.meeting_goal = goal);
        self.model.notice = Some("Goal saved".into());
        self.notice_at = Some(Instant::now());
    }

    fn panel_height(&self) -> f32 {
        let insights = &self.model.insights;
        let first_run = !self.model.settings.onboarded
            && !self.settings_open
            && self.tab == Tab::Live
            && !self.model.ending
            && self.model.notes_path.is_none()
            && self.model.answers.is_empty()
            && insights.topic.is_empty()
            && insights.summary.is_empty()
            && insights.question.is_none()
            && insights.suggestions.is_empty();
        if self.compact_shown() {
            COMPACT_PANEL_HEIGHT
        } else if first_run {
            220.0
        } else if self.live_body_shown() {
            self.live_fit.get().height().unwrap_or(COPILOT_PANEL_HEIGHT)
        } else {
            COPILOT_PANEL_HEIGHT
        }
    }

    /// Whether the panel shows only live insights, which fit their content.
    /// Answers get the full height from their first word, so the ask box does
    /// not move while one streams in; settings, the transcript, and the
    /// wrap-up keep full height too.
    fn live_body_shown(&self) -> bool {
        !self.settings_open
            && self.tab == Tab::Live
            && !self.model.ending
            && self.model.notes_path.is_none()
            && self.model.answers.is_empty()
            && !self.compact_shown()
    }

    /// Whether the panel shows only the line to say.
    fn compact_shown(&self) -> bool {
        self.model.settings.compact
            && !self.settings_open
            && self.tab == Tab::Live
            && !self.model.ending
            && self.model.notes_path.is_none()
            && self.model.alert.is_none()
    }

    /// Whether only the notes-ready bar is shown: the meeting was ended by
    /// hand and the wrap-up has not been opened.
    fn notes_ready_shown(&self) -> bool {
        self.model.notes_quiet && self.model.notes_path.is_some() && !self.panel_open
    }

    fn desired_size(&self) -> (f32, f32) {
        let height = COPILOT_TOP_MARGIN
            + COPILOT_BAR_HEIGHT
            + if self.panel_open {
                COPILOT_PANEL_GAP + self.panel_height()
            } else {
                0.0
            }
            + SHADOW_MARGIN;
        (COPILOT_WIDTH, height)
    }

    /// Host window calls run on a later turn of the event loop: Win32 sends
    /// size and activation messages synchronously, and GPUI must not be
    /// re-entered while it is rendering.
    fn with_host(
        &self,
        cx: &mut Context<Self>,
        call: impl FnOnce(&dyn CopilotWindowHost) + 'static,
    ) {
        let host = Rc::clone(&self.host);
        cx.spawn(async move |_, _| call(host.as_ref())).detach();
    }

    fn sync_host(&mut self, cx: &mut Context<Self>) {
        let size = self.desired_size();
        if self.window_size != Some(size) {
            self.window_size = Some(size);
            self.with_host(cx, move |host| host.resize(size.0, size.1));
        }
        let excluded = self.model.settings.hide_from_screen_share;
        if self.capture_excluded != excluded {
            self.capture_excluded = excluded;
            self.with_host(cx, move |host| host.set_capture_excluded(excluded));
        }
    }

    fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if !visible && self.typing {
            self.stop_typing(true, cx);
        }
        self.with_host(cx, move |host| host.set_visible(visible));
    }

    fn start_typing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            self.set_visible(true, cx);
        }
        if !self.typing {
            self.typing = true;
            self.with_host(cx, |host| host.begin_typing());
        }
        self.ask.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn start_settings_typing(&mut self, cx: &mut Context<Self>) {
        if !self.typing {
            self.typing = true;
            self.with_host(cx, |host| host.begin_typing());
        }
    }

    fn stop_typing(&mut self, restore_focus: bool, cx: &mut Context<Self>) {
        if self.typing {
            self.typing = false;
            self.with_host(cx, move |host| host.end_typing(restore_focus));
        }
    }

    fn rewrite(&mut self, id: u64, style: RewriteStyle, cx: &mut Context<Self>) {
        if let Some(text) = self.model.begin_rewrite(id, style) {
            self.act(CopilotAction::Rewrite { id, style, text });
            cx.notify();
        }
    }

    fn step_version(&mut self, id: u64, forward: bool, cx: &mut Context<Self>) {
        if let Some(text) = self.model.step_version(id, forward) {
            self.act(CopilotAction::KeepVersion { id, text });
            cx.notify();
        }
    }

    /// Opens the name editor for one of the other side's voices.
    pub fn begin_naming(&mut self, voice: u8, window: &mut Window, cx: &mut Context<Self>) {
        self.naming = Some(voice);
        let current = self
            .model
            .speaker_names
            .get(&voice)
            .cloned()
            .unwrap_or_default();
        self.name_input.update(cx, |input, cx| {
            input.set_value(current, window, cx);
            input.focus(window, cx);
        });
        self.start_settings_typing(cx);
        cx.notify();
    }

    /// Names a voice; an empty name returns it to "Them A".
    fn name_voice(&mut self, voice: u8, name: &str, cx: &mut Context<Self>) {
        self.model.name_speaker(voice, name);
        self.act(CopilotAction::NameSpeaker {
            voice,
            name: name.trim().to_owned(),
        });
        self.naming = None;
        self.stop_typing(true, cx);
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            if self.settings_open {
                self.settings_open = false;
            }
            self.naming = None;
            self.stop_typing(true, cx);
            cx.notify();
        }
    }

    fn elapsed_label(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        format_offset(
            now.saturating_sub(self.model.session_started_unix_ms)
                .max(0) as u64,
        )
    }

    fn render_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let shortcuts = self.model.shortcuts.clone();
        let listening = self.model.listen == ListenState::Listening;
        let levels: Vec<f32> = self
            .model
            .levels_me
            .iter()
            .zip(&self.model.levels_them)
            .map(|(me, them)| me.max(*them))
            .collect();
        let problem = self.model.problem.clone();
        let listen_content: AnyElement =
            match &self.model.listen {
                ListenState::Listening if problem.is_some() => h_flex()
                    .debug_selector(|| "copilot-problem".to_owned())
                    .gap(px(7.))
                    .child(icon("alert", 13., WARNING))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color(WARNING))
                            .child(problem_label(problem.as_deref().unwrap_or_default())),
                    )
                    .into_any_element(),
                ListenState::Listening => h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(color(ACCENT).opacity(0.45 + 0.55 * pulse(1_400.)))
                            .shadow(vec![gpui::BoxShadow {
                                color: color(ACCENT).opacity(0.5),
                                offset: gpui::point(px(0.), px(0.)),
                                blur_radius: px(6.),
                                spread_radius: px(0.),
                            }]),
                    )
                    .child(h_flex().h(px(14.)).gap(px(2.)).items_center().children(
                        levels.iter().map(|level| {
                            div()
                                .w(px(2.))
                                .h(px(3. + 11. * level))
                                .rounded_full()
                                .bg(color(ACCENT).opacity(0.35 + 0.65 * level))
                        }),
                    ))
                    .child(
                        div()
                            .min_w(px(32.))
                            .text_size(px(12.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color(TEXT))
                            .child(self.elapsed_label()),
                    )
                    .into_any_element(),
                ListenState::Starting => h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .bg(color(ACCENT).opacity(0.3 + 0.7 * pulse(700.))),
                    )
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(color(MUTED))
                            .child("Starting"),
                    )
                    .into_any_element(),
                ListenState::Paused => h_flex()
                    .gap(px(7.))
                    .child(icon("play", 12., ACCENT))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(color(TEXT))
                            .child("Resume"),
                    )
                    .into_any_element(),
                ListenState::Unavailable(_) => h_flex()
                    .gap(px(7.))
                    .child(icon("mic", 13., DANGER))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(color(DANGER))
                            .child("Audio off"),
                    )
                    .into_any_element(),
            };
        let panel_open = self.panel_open;
        // When answers come only on request, Ask AI is tinted as the way to
        // get one.
        let on_demand = !self.model.settings.auto_answer;
        let pill = h_flex()
            .debug_selector(|| "copilot-bar".to_owned())
            .id("copilot-bar")
            .relative()
            .h(px(PILL_HEIGHT))
            .pl(px(4.))
            .pr(px(4.))
            .gap(px(1.))
            .items_center()
            .rounded(px(PILL_HEIGHT / 2.0))
            // Empty parts of the bar move the window; buttons occlude it.
            .window_control_area(WindowControlArea::Drag)
            .bg(color(GLASS))
            .border_1()
            .border_color(color(BORDER))
            .shadow(vec![gpui::BoxShadow {
                color: gpui::rgba(0x00000066).into(),
                offset: gpui::point(px(0.), px(6.)),
                blur_radius: px(14.),
                spread_radius: px(0.),
            }])
            .child(
                div()
                    .id("copilot-grip")
                    .size(px(30.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .cursor_move()
                    .window_control_area(WindowControlArea::Drag)
                    .child(
                        div()
                            .size(px(22.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(gpui::linear_gradient(
                                135.,
                                gpui::linear_color_stop(color(0xf3c4a9ff), 0.),
                                gpui::linear_color_stop(color(0xd9826aff), 1.),
                            ))
                            .child(icon("sparkles", 12., 0x1a1110ff)),
                    ),
            )
            .child(
                bar_button("copilot-listen")
                    .when(listening, |button| button.bg(color(0xffffff0a)))
                    .child(listen_content)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                        this.act(CopilotAction::ToggleListening);
                    })),
            )
            .child(divider())
            .child(
                bar_button("copilot-assist")
                    .when(on_demand, |button| button.bg(color(ACCENT_SOFT)))
                    .child(icon("sparkles", 13., ACCENT))
                    .child(
                        div()
                            .when(on_demand, |label| {
                                label.debug_selector(|| "copilot-assist-on-demand".to_owned())
                            })
                            .text_size(px(12.5))
                            .font_weight(FontWeight::MEDIUM)
                            .child("Ask AI"),
                    )
                    .child(keycaps(&shortcuts.assist))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.act(CopilotAction::Assist);
                        this.panel_open = true;
                        cx.notify();
                    })),
            )
            .child(
                bar_button("copilot-hide")
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(color(MUTED))
                            .child("Hide"),
                    )
                    .child(keycaps(&shortcuts.toggle))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.set_visible(false, cx);
                        cx.notify();
                    })),
            )
            .child(divider())
            .child(
                icon_button(
                    "copilot-panel",
                    if panel_open {
                        "chevron-up"
                    } else {
                        "chevron-down"
                    },
                    MUTED,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.panel_open = !this.panel_open;
                    this.user_collapsed = !this.panel_open;
                    if !this.panel_open {
                        this.stop_typing(true, cx);
                    }
                    cx.notify();
                })),
            )
            .child(
                icon_button(
                    "copilot-compact-toggle",
                    if self.model.settings.compact {
                        "plus"
                    } else {
                        "minus"
                    },
                    if self.model.settings.compact {
                        ACCENT
                    } else {
                        MUTED
                    },
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.update_settings(|settings| settings.compact = !settings.compact);
                    cx.notify();
                })),
            )
            .child(
                icon_button(
                    "copilot-settings",
                    "sliders",
                    if self.settings_open { ACCENT } else { MUTED },
                )
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.settings_open = !this.settings_open;
                    this.panel_open = true;
                    if this.settings_open {
                        this.load_text_settings(window, cx);
                    } else {
                        this.stop_typing(true, cx);
                    }
                    cx.notify();
                })),
            )
            .child(if self.end_armed_at.is_some() {
                // A second click within END_CONFIRM ends the meeting, so a
                // stray click on the X never throws a meeting away.
                bar_button("copilot-end")
                    .text_size(px(12.))
                    .text_color(color(DANGER))
                    .bg(color(0xf8717122))
                    .child(icon("x", 12., DANGER))
                    .child("End meeting?")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.end_armed_at = None;
                        this.act(CopilotAction::End);
                        this.stop_typing(true, cx);
                        // The bar goes at once; notes finish out of sight and
                        // land on the Meetings page.
                        this.set_visible(false, cx);
                        cx.notify();
                    }))
            } else {
                icon_button("copilot-end", "x", MUTED).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.end_armed_at = Some(Instant::now());
                        cx.notify();
                    },
                ))
            })
            .child(shape_probe(&self.shape, &self.host, false));
        h_flex()
            .w_full()
            .h(px(COPILOT_BAR_HEIGHT))
            .pt(px(COPILOT_TOP_MARGIN))
            .justify_center()
            .child(pill)
    }

    /// The bar after a meeting ended by hand: the notes are ready, one
    /// click from the wrap-up or the follow-up email, and it leaves on its
    /// own.
    fn render_notes_ready(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let topic = self.model.insights.topic.trim().to_owned();
        let has_email = self.model.follow_up.is_some();
        let pill = h_flex()
            .debug_selector(|| "copilot-notes-ready".to_owned())
            .id("copilot-notes-ready")
            .relative()
            .h(px(PILL_HEIGHT))
            .pl(px(12.))
            .pr(px(4.))
            .gap(px(8.))
            .items_center()
            .rounded(px(PILL_HEIGHT / 2.0))
            .window_control_area(WindowControlArea::Drag)
            .bg(color(GLASS))
            .border_1()
            .border_color(color(BORDER))
            .child(icon("check", 14., ACCENT))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Notes ready"),
            )
            .when(!topic.is_empty(), |pill| {
                pill.child(
                    div()
                        .max_w(px(220.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(12.))
                        .text_color(color(MUTED))
                        .child(topic),
                )
            })
            .child(divider())
            .child(
                bar_button("copilot-notes-ready-open")
                    .child(icon("file-text", 12., ACCENT))
                    .child(div().text_size(px(12.5)).child("Wrap-up"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.act(CopilotAction::KeepOpen);
                        this.panel_open = true;
                        this.tab = Tab::Live;
                        cx.notify();
                    })),
            )
            .when(has_email, |pill| {
                pill.child(
                    bar_button("copilot-notes-ready-email")
                        .child(icon("mail", 12., ACCENT))
                        .child(div().text_size(px(12.5)).child("Draft email"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.act(CopilotAction::DraftEmail);
                        })),
                )
            })
            .child(
                icon_button("copilot-notes-ready-close", "x", MUTED).on_click(cx.listener(
                    |this, _: &ClickEvent, _, _| {
                        this.act(CopilotAction::End);
                    },
                )),
            )
            .child(shape_probe(&self.shape, &self.host, false));
        h_flex()
            .w_full()
            .h(px(COPILOT_BAR_HEIGHT))
            .pt(px(COPILOT_TOP_MARGIN))
            .justify_center()
            .child(pill)
    }

    /// The panel in compact mode: the line to say, large, and where it
    /// comes from.
    fn render_compact(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let latest = self.model.latest_answer().cloned();
        let (line, grounding, checking) = match &latest {
            Some(answer) => (
                answer.line_to_say(),
                answer.grounding.filter(|_| answer.shown == 0),
                answer.status.is_active(),
            ),
            None => (String::new(), None, false),
        };
        let waiting = line.trim().is_empty();
        v_flex()
            .debug_selector(|| "copilot-compact".to_owned())
            .id("copilot-compact-body")
            .size_full()
            .overflow_y_scroll()
            .gap(px(6.))
            .px(px(18.))
            .py(px(14.))
            .child(
                div()
                    .w_full()
                    .text_size(px(if waiting { 13. } else { 17. }))
                    .line_height(rems(1.4))
                    .text_color(color(if waiting { DIM } else { TEXT }))
                    .child(if waiting {
                        if latest.is_some() {
                            "Writing your line\u{2026}".to_owned()
                        } else {
                            "The line to say appears here when they ask you something.".to_owned()
                        }
                    } else {
                        // Markdown emphasis is for the full card.
                        line.replace("**", "")
                    }),
            )
            .when(!waiting, |column| {
                column.child(
                    h_flex()
                        .gap(px(10.))
                        .text_size(px(11.))
                        .text_color(color(DIM))
                        .when_some(grounding, |row, grounding| {
                            row.child(grounding_mark(grounding))
                        })
                        .when(checking, |row| row.child("Checking the facts"))
                        .child(div().flex_1())
                        .child(
                            div()
                                .debug_selector(|| "copilot-compact-more".to_owned())
                                .id("copilot-compact-more")
                                .cursor_pointer()
                                .hover(|style| style.text_color(color(TEXT)))
                                .child("Show everything")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.update_settings(|settings| settings.compact = false);
                                    cx.notify();
                                })),
                        ),
                )
            })
    }

    /// Something that needs the user's eye now: their name was said, or
    /// their documents contradict what they said.
    fn render_alert(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let alert = self.model.alert.clone()?;
        let mention = alert.kind == AlertKind::Mention;
        let tint = if mention { DANGER } else { WARNING };
        let question = alert.text.clone();
        Some(
            v_flex()
                .debug_selector(|| "copilot-alert".to_owned())
                .w_full()
                .gap(px(8.))
                .p(px(12.))
                .rounded(px(12.))
                .bg(color(tint).opacity(0.12))
                .border_1()
                .border_color(color(tint).opacity(0.4))
                .child(
                    h_flex().gap(px(6.)).child(icon("alert", 13., tint)).child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color(tint))
                            .child(if mention {
                                "THEY SAID YOUR NAME"
                            } else {
                                "CHECK WHAT YOU SAID"
                            }),
                    ),
                )
                .child(
                    div()
                        .text_size(px(14.5))
                        .line_height(rems(1.4))
                        .child(alert.text.clone()),
                )
                .when(!alert.context.trim().is_empty(), |card| {
                    card.child(
                        v_flex()
                            .gap(px(3.))
                            .child(section_label("Just before"))
                            .children(alert.context.lines().map(|line| {
                                div()
                                    .text_size(px(12.))
                                    .text_color(color(MUTED))
                                    .child(line.to_owned())
                            })),
                    )
                })
                .child(
                    h_flex()
                        .gap(px(6.))
                        .when(mention, |row| {
                            row.child(
                                chip("copilot-alert-answer", true)
                                    .debug_selector(|| "copilot-alert-answer".to_owned())
                                    .child(icon("sparkles", 11., ACCENT))
                                    .child("What should I say?")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.model.alert = None;
                                        this.act(CopilotAction::AnswerQuestion(question.clone()));
                                        cx.notify();
                                    })),
                            )
                        })
                        .child(
                            chip("copilot-alert-dismiss", false)
                                .debug_selector(|| "copilot-alert-dismiss".to_owned())
                                .child("Dismiss")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.model.alert = None;
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }

    /// The meeting's checklist, ticking itself as things are covered.
    fn render_checklist(&self) -> Option<impl IntoElement + use<>> {
        let items = self.model.checklist.clone();
        if items.is_empty() {
            return None;
        }
        let (done, total) = self.model.checklist_progress();
        Some(
            v_flex()
                .debug_selector(|| "copilot-checklist".to_owned())
                .w_full()
                .gap(px(6.))
                .child(
                    h_flex()
                        .gap(px(6.))
                        .child(icon("check", 12., DIM))
                        .child(section_label("Checklist"))
                        .child(
                            div()
                                .ml_auto()
                                .text_size(px(11.))
                                .text_color(color(DIM))
                                .child(format!("{done} of {total}")),
                        ),
                )
                .child(h_flex().flex_wrap().gap_x(px(14.)).gap_y(px(4.)).children(
                    items.into_iter().map(|item| {
                        h_flex()
                            .gap(px(5.))
                            .child(if item.done {
                                icon("check", 12., ACCENT).into_any_element()
                            } else {
                                div()
                                    .size(px(10.))
                                    .mx(px(1.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(color(DIM))
                                    .into_any_element()
                            })
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(color(if item.done { DIM } else { TEXT }))
                                    .when(item.done, |label| label.line_through())
                                    .child(item.label),
                            )
                    }),
                )),
        )
    }

    fn render_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // After the meeting, the transcript stays one tab away from the wrap-up.
        let compact = self.compact_shown();
        let body: AnyElement = if compact {
            self.render_compact(cx).into_any_element()
        } else if self.settings_open {
            self.render_settings(cx).into_any_element()
        } else if self.tab == Tab::Transcript {
            self.render_transcript(cx).into_any_element()
        } else if self.model.ending || self.model.notes_path.is_some() {
            self.render_ending(window, cx).into_any_element()
        } else {
            self.render_live(window, cx).into_any_element()
        };
        let show_footer = !compact
            && !self.settings_open
            && !self.model.ending
            && self.model.notes_path.is_none();
        let panel_height = self.panel_height();
        let fits = self.live_body_shown();
        let fit = Rc::clone(&self.live_fit);
        let view = cx.entity().downgrade();
        div()
            .w_full()
            .px(px(SHADOW_MARGIN))
            .pt(px(COPILOT_PANEL_GAP))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .h(px(panel_height))
                    .rounded(px(PANEL_RADIUS))
                    .bg(color(GLASS))
                    .border_1()
                    .border_color(color(BORDER))
                    .overflow_hidden()
                    .shadow(vec![gpui::BoxShadow {
                        color: gpui::rgba(0x00000073).into(),
                        offset: gpui::point(px(0.), px(8.)),
                        blur_radius: px(16.),
                        spread_radius: px(0.),
                    }])
                    .child(shape_probe(&self.shape, &self.host, true))
                    .when(!compact, |panel| panel.child(self.render_header(cx)))
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .when(fits, |body| {
                                body.child(fit_probe(
                                    &fit,
                                    view,
                                    FitProbe::Viewport { panel_height },
                                ))
                            })
                            .child(body),
                    )
                    .when(show_footer, |panel| panel.child(self.render_footer(cx))),
            )
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tab = self.tab;
        let settings = &self.model.settings;
        let mut capabilities = Vec::new();
        if settings.web_search {
            capabilities.push(("globe", "Web"));
        }
        if settings.screen_context {
            capabilities.push(("monitor", "Screen"));
        }
        let model_label = format!(
            "{} \u{00b7} {}",
            settings.provider.label(),
            self.model.model_label()
        );
        let tab_button = |id: &'static str, label: &'static str, active: bool| {
            div()
                .debug_selector(move || id.to_owned())
                .id(id)
                .h(px(28.))
                .px(px(12.))
                .flex()
                .items_center()
                .rounded(px(14.))
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .when(active, |button| {
                    button.bg(color(0xffffff14)).text_color(color(TEXT))
                })
                .when(!active, |button| {
                    button
                        .text_color(color(DIM))
                        .hover(|style| style.text_color(color(TEXT)))
                })
                .child(label)
        };
        let title = if self.settings_open {
            h_flex()
                .gap(px(8.))
                .child(icon("sliders", 14., ACCENT))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Copilot settings"),
                )
                .into_any_element()
        } else {
            h_flex()
                .gap(px(4.))
                .child(
                    tab_button(
                        "copilot-tab-live",
                        if self.model.notes_path.is_some() {
                            "Wrap-up"
                        } else {
                            "Live insights"
                        },
                        tab == Tab::Live,
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.tab = Tab::Live;
                        cx.notify();
                    })),
                )
                .child(
                    tab_button(
                        "copilot-tab-transcript",
                        "Transcript",
                        tab == Tab::Transcript,
                    )
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.tab = Tab::Transcript;
                        this.transcript_scroll.scroll_to_bottom();
                        cx.notify();
                    })),
                )
                .into_any_element()
        };
        h_flex()
            .h(px(46.))
            .flex_none()
            .px(px(12.))
            .justify_between()
            .border_b_1()
            .border_color(color(HAIRLINE))
            .child(title)
            .child(
                h_flex()
                    .gap(px(10.))
                    .text_size(px(11.5))
                    .text_color(color(DIM))
                    .when_some(self.model.problem.clone(), |row, problem| {
                        row.child(
                            h_flex()
                                .max_w(px(320.))
                                .gap(px(5.))
                                .px(px(8.))
                                .py(px(2.))
                                .rounded(px(10.))
                                .bg(color(0xfbbf241f))
                                .text_color(color(WARNING))
                                .child(icon("alert", 11., WARNING))
                                .child(div().overflow_hidden().whitespace_nowrap().child(problem)),
                        )
                    })
                    .when_some(self.model.notice.clone(), |row, notice| {
                        row.child(
                            div()
                                .max_w(px(260.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .px(px(8.))
                                .py(px(2.))
                                .rounded(px(10.))
                                .bg(color(0xffffff0d))
                                .text_color(color(MUTED))
                                .child(notice),
                        )
                    })
                    .children(capabilities.into_iter().map(|(name, label)| {
                        h_flex()
                            .gap(px(4.))
                            .child(icon(name, 11., DIM))
                            .child(label)
                    }))
                    .child(
                        div()
                            .max_w(px(220.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(color(MUTED))
                            .child(model_label),
                    ),
            )
    }

    fn render_live(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let answers = self.model.answers.clone();
        let latest = answers.last().cloned();
        let earlier: Vec<AnswerCardModel> = answers.iter().rev().skip(1).cloned().collect();
        let insights = self.model.insights.clone();
        let has_insights = !insights.topic.is_empty() || !insights.summary.is_empty();
        let mut content = v_flex()
            .relative()
            .w_full()
            .gap(px(14.))
            .p(px(14.))
            .child(fit_probe(
                &self.live_fit,
                cx.entity().downgrade(),
                FitProbe::Content,
            ));
        if let Some(alert) = self.render_alert(cx) {
            content = content.child(alert);
        }
        if let Some(answer) = latest {
            content = content.child(self.render_answer(&answer, true, window, cx));
        }
        if let Some(checklist) = self.render_checklist() {
            content = content.child(checklist);
        }
        if let Some(brief) = self
            .model
            .brief
            .clone()
            .filter(|_| !self.brief_closed && self.model.answers.is_empty())
        {
            let style = TextViewStyle {
                is_dark: true,
                highlight_theme: HighlightTheme::default_dark(),
                ..TextViewStyle::default()
            }
            .paragraph_gap(rems(0.4));
            content = content.child(
                v_flex()
                    .debug_selector(|| "copilot-brief".to_owned())
                    .w_full()
                    .gap(px(6.))
                    .p(px(12.))
                    .rounded(px(12.))
                    .bg(color(GLASS_RAISED))
                    .border_1()
                    .border_color(color(HAIRLINE))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .child(icon("book", 12., ACCENT))
                            .child(section_label("Before you start"))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id("copilot-brief-close")
                                    .debug_selector(|| "copilot-brief-close".to_owned())
                                    .size(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded_full()
                                    .cursor_pointer()
                                    .hover(|style| style.bg(color(HOVER)))
                                    .child(icon("x", 11., MUTED))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.brief_closed = true;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_size(px(13.))
                            .line_height(rems(1.4))
                            .text_color(color(MUTED))
                            .child(
                                TextView::markdown("copilot-brief-text", brief, window, cx)
                                    .style(style)
                                    .selectable(true),
                            ),
                    ),
            );
        }
        if let Some(question) = insights.question.clone()
            && !self
                .model
                .answers
                .iter()
                .any(|answer| covers_question(&answer.title, &question))
        {
            content = content.child(
                h_flex()
                    .debug_selector(|| "copilot-detected-question".to_owned())
                    .id("copilot-detected-question")
                    .w_full()
                    .gap(px(10.))
                    .p(px(12.))
                    .rounded(px(12.))
                    .bg(color(ACCENT_SOFT))
                    .border_1()
                    .border_color(color(0xe6a48a40))
                    .cursor_pointer()
                    .hover(|style| style.bg(color(0xe6a48a38)))
                    .child(icon("message-question", 16., ACCENT))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(color(ACCENT))
                                    .child("QUESTION DETECTED \u{00b7} CLICK TO ANSWER"),
                            )
                            .child(div().text_size(px(13.)).child(question.clone())),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                        this.act(CopilotAction::AnswerQuestion(question.clone()));
                    })),
            );
        }
        if has_insights {
            let suggestions = insights.suggestions.clone();
            let situation = situation_label(insights.situation, self.model.settings.auto_answer);
            content = content.child(
                v_flex()
                    .debug_selector(|| "copilot-insights".to_owned())
                    .w_full()
                    .gap(px(8.))
                    .child(
                        h_flex()
                            .w_full()
                            .gap(px(6.))
                            .child(icon("activity", 12., DIM))
                            .child(section_label("Live insights"))
                            .when_some(situation, |row, label| {
                                row.child(
                                    div()
                                        .debug_selector(|| "copilot-situation".to_owned())
                                        .ml_auto()
                                        .text_size(px(11.))
                                        .text_color(color(DIM))
                                        .child(label),
                                )
                            }),
                    )
                    .when(!insights.topic.is_empty(), |section| {
                        section.child(
                            div()
                                .text_size(px(14.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(insights.topic.clone()),
                        )
                    })
                    .children(insights.summary.iter().map(|item| {
                        h_flex()
                            .items_start()
                            .gap(px(8.))
                            .child(div().mt(px(7.)).size(px(4.)).rounded_full().bg(color(DIM)))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(13.))
                                    .text_color(color(MUTED))
                                    .child(item.clone()),
                            )
                    }))
                    .when(!suggestions.is_empty(), |section| {
                        section.child(
                            h_flex().flex_wrap().gap(px(6.)).children(
                                suggestions
                                    .into_iter()
                                    .enumerate()
                                    .map(|(index, suggestion)| {
                                        let prompt = suggestion.clone();
                                        chip(("copilot-suggestion", index), false)
                                            .child(icon("sparkles", 11., ACCENT))
                                            .child(suggestion)
                                            .on_click(cx.listener(
                                                move |this, _: &ClickEvent, _, _| {
                                                    this.act(CopilotAction::Ask(prompt.clone()));
                                                },
                                            ))
                                    }),
                            ),
                        )
                    }),
            );
        }
        if !self.model.promises.is_empty() {
            content = content.child(
                v_flex()
                    .debug_selector(|| "copilot-promises".to_owned())
                    .w_full()
                    .gap(px(6.))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .child(icon("file-text", 12., DIM))
                            .child(section_label("You promised")),
                    )
                    .children(self.model.promises.iter().map(|promise| {
                        h_flex()
                            .items_start()
                            .gap(px(8.))
                            .child(
                                div()
                                    .mt(px(7.))
                                    .size(px(4.))
                                    .rounded_full()
                                    .bg(color(ACCENT)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(13.))
                                    .text_color(color(MUTED))
                                    .child(promise.clone()),
                            )
                    })),
            );
        }
        if !self.model.settings.onboarded && self.model.answers.is_empty() && !has_insights {
            content = content.child(self.render_welcome(cx));
        } else if self.model.answers.is_empty() && !has_insights {
            content = content.child(self.render_empty_state(cx));
        }
        if !earlier.is_empty() {
            content = content.child(section_label("Earlier"));
            for answer in earlier {
                if self.expanded.contains(&answer.id) {
                    content = content.child(self.render_answer(&answer, false, window, cx));
                } else {
                    let id = answer.id;
                    let source = if answer.body.trim().is_empty() {
                        &answer.instant
                    } else {
                        &answer.body
                    };
                    let preview: String = source
                        .lines()
                        .map(|line| line.trim_start_matches(['#', '-', '*', ' ', '>']))
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or_default()
                        .chars()
                        .filter(|character| !matches!(character, '*' | '_' | '`'))
                        .take(120)
                        .collect();
                    content = content.child(
                        v_flex()
                            .id(("copilot-earlier", id))
                            .w_full()
                            .gap(px(2.))
                            .px(px(12.))
                            .py(px(9.))
                            .rounded(px(10.))
                            .bg(color(0xffffff06))
                            .cursor_pointer()
                            .hover(|style| style.bg(color(0xffffff0d)))
                            .child(
                                div()
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(answer.title.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(color(DIM))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .child(preview),
                            )
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.expanded.insert(id);
                                cx.notify();
                            })),
                    );
                }
            }
        }
        div()
            .debug_selector(|| "copilot-live-body".to_owned())
            .id("copilot-live-body")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .child(content)
    }

    /// A dismissible first-run hint that leaves room for the conversation.
    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        h_flex()
            .debug_selector(|| "copilot-welcome".to_owned())
            .w_full()
            .items_center()
            .gap(px(8.))
            .px(px(10.))
            .py(px(6.))
            .child(icon("sparkles", 14., ACCENT))
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
                    .text_color(color(MUTED))
                    .child("Meeting Copilot"),
            )
            .child(
                icon_button("copilot-welcome-done", "x", MUTED).on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| {
                        this.update_settings(|settings| settings.onboarded = true);
                        cx.notify();
                    },
                )),
            )
    }

    fn render_empty_state(&self, _cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let shortcut = shortcut_text(&self.model.shortcuts.assist);
        let headline = match &self.model.listen {
            ListenState::Listening => "Listening to your meeting",
            ListenState::Starting => "Connecting to your audio",
            ListenState::Paused => "Listening is paused",
            ListenState::Unavailable(_) => "Audio is unavailable",
        };
        let detail = match &self.model.listen {
            ListenState::Unavailable(message) => message.clone(),
            _ if !self.model.settings.auto_answer => format!(
                "Answers come only when you ask. Press {shortcut} or Ask AI for help with what's on your screen and in the conversation."
            ),
            _ => format!(
                "Questions they ask are answered automatically. Press {shortcut} anytime for help with what's on your screen and in the conversation."
            ),
        };
        v_flex()
            .w_full()
            .items_center()
            .gap(px(10.))
            .pt(px(56.))
            .px(px(40.))
            .child(
                div()
                    .size(px(44.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(color(ACCENT_SOFT))
                    .child(icon("sparkles", 20., ACCENT)),
            )
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(headline),
            )
            .child(
                div()
                    .max_w(px(460.))
                    .text_center()
                    .text_size(px(12.5))
                    .text_color(color(MUTED))
                    .child(detail),
            )
    }

    fn render_answer(
        &mut self,
        answer: &AnswerCardModel,
        latest: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = answer.id;
        let active = answer.status.is_active();
        let failed = matches!(answer.status, AnswerStatus::Failed(_));
        let status = answer.status_label();
        let body = answer.body.clone();
        let instant = answer.instant.clone();
        let has_instant = !instant.trim().is_empty();
        // One answer that grows: the instant draft is the line to say, and the
        // researched answer adds to it, corrects it, or confirms it.
        let (hero, follow_up) = if has_instant {
            (instant.clone(), researched_follow_up(&body))
        } else {
            (body.clone(), FollowUp::Pending)
        };
        // A rewrite replaces the line to say, including a correction it was
        // made from.
        let rewritten = answer.shown > 0;
        let (hero, follow_up) = if rewritten {
            let follow_up = match follow_up {
                FollowUp::Correction(_) => FollowUp::Pending,
                other => other,
            };
            (answer.line_to_say(), follow_up)
        } else {
            (hero, follow_up)
        };
        let corrected = matches!(&follow_up, FollowUp::Correction(line) if !line.is_empty());
        let copy_text = if rewritten {
            answer.line_to_say()
        } else {
            combined_answer(&instant, &body)
        };
        let rewrite_row = (latest && (answer.line_ready() || answer.rewriting.is_some()))
            .then(|| self.render_rewrites(answer, cx));
        let rewriting_now = rewritten && hero.trim().is_empty();
        let style = TextViewStyle {
            is_dark: true,
            highlight_theme: HighlightTheme::default_dark(),
            ..TextViewStyle::default()
        }
        .paragraph_gap(rems(0.55));
        let header = h_flex()
            .w_full()
            .gap(px(8.))
            .child(
                div()
                    .size(px(22.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(color(if answer.automatic {
                        ACCENT_SOFT
                    } else {
                        0xffffff10
                    }))
                    .child(icon(
                        if answer.automatic {
                            "message-question"
                        } else {
                            "sparkles"
                        },
                        12.,
                        ACCENT,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(color(if latest { TEXT } else { MUTED }))
                    .child(answer.title.clone()),
            )
            .when(answer.used_screen, |row| {
                row.child(
                    h_flex()
                        .gap(px(4.))
                        .text_size(px(11.))
                        .text_color(color(DIM))
                        .child(icon("monitor", 11., DIM))
                        .child("Screen"),
                )
            })
            .when(active, |row| {
                row.child(
                    div()
                        .id(("copilot-stop", id))
                        .h(px(24.))
                        .px(px(9.))
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .rounded(px(12.))
                        .text_size(px(11.5))
                        .text_color(color(MUTED))
                        .cursor_pointer()
                        .hover(|style| style.bg(color(HOVER)))
                        .child(icon("square", 10., MUTED))
                        .child("Stop")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.act(CopilotAction::Cancel);
                        })),
                )
            })
            .when(!active && !copy_text.is_empty(), |row| {
                row.child(
                    div()
                        .id(("copilot-copy", id))
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|style| style.bg(color(HOVER)))
                        .child(icon("copy", 12., MUTED))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                            this.act(CopilotAction::CopyText(copy_text.clone()));
                        })),
                )
            })
            .when(!latest, |row| {
                row.child(
                    div()
                        .id(("copilot-collapse", id))
                        .size(px(26.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|style| style.bg(color(HOVER)))
                        .child(icon("chevron-up", 12., MUTED))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.expanded.remove(&id);
                            cx.notify();
                        })),
                )
            });
        // With a draft on screen, the researched lane's progress reads as
        // checking the draft rather than a wait.
        let status = match (&answer.status, has_instant) {
            (AnswerStatus::Thinking | AnswerStatus::Streaming, true) => {
                Some("Checking the facts".to_owned())
            }
            _ => status,
        };
        let show_status = active && matches!(follow_up, FollowUp::Pending);
        let status_row = status.filter(|_| show_status || failed).map(|label| {
            let dots: Vec<f32> = (0..3)
                .map(|index| 0.3 + 0.7 * pulse(1_100. + f64::from(index) * 180.))
                .collect();
            h_flex()
                .gap(px(8.))
                .text_size(px(12.))
                .text_color(color(if failed { DANGER } else { DIM }))
                .when(active, |row| {
                    row.child(h_flex().gap(px(3.)).children(dots.into_iter().map(|alpha| {
                        div()
                            .size(px(5.))
                            .rounded_full()
                            .bg(color(ACCENT).opacity(alpha))
                    })))
                })
                .when(matches!(answer.status, AnswerStatus::Searching(_)), |row| {
                    row.child(icon("globe", 12., DIM))
                })
                .child(div().flex_1().min_w_0().child(label))
        });
        let markdown = |id: String, text: String, window: &mut Window, cx: &mut Context<Self>| {
            TextView::markdown(SharedString::from(id), text, window, cx)
                .style(style.clone())
                .selectable(true)
        };
        let hero_view = (!hero.trim().is_empty()).then(|| {
            let id = if rewritten {
                format!("copilot-rewrite-text-{id}-{}", answer.shown)
            } else if has_instant {
                format!("copilot-instant-text-{id}")
            } else {
                format!("copilot-answer-text-{id}")
            };
            markdown(id, hero.clone(), window, cx)
        });
        let follow_up_view = match &follow_up {
            FollowUp::Adds(more) if !more.is_empty() => Some(
                v_flex()
                    .debug_selector(|| "copilot-answer-more".to_owned())
                    .w_full()
                    .gap(px(6.))
                    .pt(px(10.))
                    .border_t_1()
                    .border_color(color(HAIRLINE))
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color(DIM))
                            .child(icon("globe", 10., ACCENT))
                            .child(if answer.sources.is_empty() {
                                "MORE".to_owned()
                            } else {
                                format!(
                                    "MORE \u{00b7} {} SOURCE{}",
                                    answer.sources.len(),
                                    if answer.sources.len() == 1 { "" } else { "S" }
                                )
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_size(px(13.5))
                            .line_height(rems(1.45))
                            .text_color(color(TEXT))
                            .child(markdown(
                                format!("copilot-answer-text-{id}"),
                                more.clone(),
                                window,
                                cx,
                            )),
                    )
                    .into_any_element(),
            ),
            FollowUp::Correction(line) if !line.is_empty() => Some(
                v_flex()
                    .debug_selector(|| "copilot-answer-correction".to_owned())
                    .w_full()
                    .gap(px(6.))
                    .p(px(12.))
                    .rounded(px(10.))
                    .bg(color(0xf871711a))
                    .border_1()
                    .border_color(color(0xf8717140))
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color(DANGER))
                            .child("CORRECTION \u{00b7} SAY THIS INSTEAD"),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_size(px(15.))
                            .line_height(rems(1.45))
                            .text_color(color(TEXT))
                            .child(markdown(
                                format!("copilot-answer-text-{id}"),
                                line.clone(),
                                window,
                                cx,
                            )),
                    )
                    .into_any_element(),
            ),
            FollowUp::Confirmed => Some(
                h_flex()
                    .debug_selector(|| "copilot-answer-confirmed".to_owned())
                    .gap(px(6.))
                    .text_size(px(11.5))
                    .text_color(color(DIM))
                    .child(icon("check", 12., ACCENT))
                    .child("Checked \u{00b7} nothing to add")
                    .into_any_element(),
            ),
            _ => None,
        };
        let sources = answer.sources.clone();
        // Where the line's facts come from, while the line shown is the
        // draft that reported it.
        let grounding = answer
            .grounding
            .filter(|_| has_instant && !rewritten && !corrected);
        v_flex()
            .id(("copilot-answer", id))
            .w_full()
            .gap(px(10.))
            .p(px(14.))
            .rounded(px(14.))
            .bg(color(if latest { GLASS_RAISED } else { 0xffffff06 }))
            .border_1()
            .border_color(color(if latest { 0xffffff17 } else { HAIRLINE }))
            .child(header)
            .when_some(hero_view, |card, hero_view| {
                card.child(
                    h_flex()
                        .debug_selector(|| "copilot-answer-hero".to_owned())
                        .w_full()
                        .items_start()
                        .gap(px(12.))
                        .child(
                            div()
                                .w(px(3.))
                                .h_full()
                                .min_h(px(20.))
                                .flex_none()
                                .rounded(px(2.))
                                .bg(color(if corrected { DIM } else { ACCENT })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(if has_instant || rewritten { 15.5 } else { 14. }))
                                .line_height(rems(1.45))
                                .text_color(color(if corrected { MUTED } else { TEXT }))
                                .child(hero_view),
                        ),
                )
            })
            .when_some(grounding, |card, grounding| {
                card.child(
                    h_flex()
                        .debug_selector(|| "copilot-answer-grounding".to_owned())
                        .pl(px(15.))
                        .text_size(px(11.))
                        .child(grounding_mark(grounding)),
                )
            })
            .when(rewriting_now, |card| {
                card.child(
                    h_flex()
                        .debug_selector(|| "copilot-rewriting".to_owned())
                        .gap(px(8.))
                        .text_size(px(12.))
                        .text_color(color(DIM))
                        .child(
                            div()
                                .size(px(5.))
                                .rounded_full()
                                .bg(color(ACCENT).opacity(0.3 + 0.7 * pulse(900.))),
                        )
                        .child("Rewriting"),
                )
            })
            .when_some(rewrite_row, |card, row| card.child(row))
            .when_some(status_row, |card, row| card.child(row))
            .when_some(follow_up_view, |card, view| card.child(view))
            .when(!sources.is_empty(), |card| {
                card.child(
                    h_flex().flex_wrap().gap(px(6.)).children(
                        sources
                            .into_iter()
                            .enumerate()
                            .take(6)
                            .map(|(index, source)| {
                                let url = source.url.clone();
                                div()
                                    .id(("copilot-source", id * 100 + index as u64))
                                    .h(px(24.))
                                    .px(px(9.))
                                    .flex()
                                    .items_center()
                                    .gap(px(5.))
                                    .rounded(px(12.))
                                    .bg(color(0xffffff0a))
                                    .text_size(px(11.))
                                    .text_color(color(MUTED))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(color(HOVER)).text_color(color(TEXT)))
                                    .child(icon("globe", 10., DIM))
                                    .child(source_host(&source.url))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                        this.act(CopilotAction::OpenUrl(url.clone()));
                                    }))
                            }),
                    ),
                )
            })
    }

    /// One-click rewrites of the line to say, and the versions made so far.
    fn render_rewrites(
        &self,
        answer: &AnswerCardModel,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = answer.id;
        let busy = answer.rewriting.is_some();
        let versions = answer.versions.len();
        let shown = answer.shown;
        let step = |selector: &'static str, glyph: &'static str, enabled: bool, forward: bool| {
            div()
                .debug_selector(move || selector.to_owned())
                .id(selector)
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .text_size(px(13.))
                .text_color(color(if enabled { MUTED } else { 0xffffff26 }))
                .when(enabled, |button| {
                    button
                        .cursor_pointer()
                        .hover(|style| style.bg(color(HOVER)).text_color(color(TEXT)))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.step_version(id, forward, cx);
                        }))
                })
                .child(glyph)
        };
        h_flex()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .pl(px(15.))
            .children(
                RewriteStyle::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, style)| {
                        chip(("copilot-rewrite", index), answer.rewriting == Some(style))
                            .debug_selector(move || format!("copilot-rewrite-{index}"))
                            .h(px(24.))
                            .px(px(9.))
                            .text_size(px(11.5))
                            .child(style.label())
                            .when(!busy, |chip| {
                                chip.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.rewrite(id, style, cx);
                                }))
                            })
                    }),
            )
            .when(versions > 0, |row| {
                row.child(
                    h_flex()
                        .ml_auto()
                        .gap(px(2.))
                        .items_center()
                        .child(step(
                            "copilot-version-previous",
                            "\u{2039}",
                            !busy && shown > 0,
                            false,
                        ))
                        .child(
                            div()
                                .debug_selector(|| "copilot-version-label".to_owned())
                                .text_size(px(11.))
                                .text_color(color(DIM))
                                .child(format!("{}/{}", shown + 1, versions + 1)),
                        )
                        .child(step(
                            "copilot-version-next",
                            "\u{203a}",
                            !busy && shown < versions,
                            true,
                        )),
                )
            })
    }

    /// The other side's people, each a chip that opens the name editor.
    fn render_people(
        &self,
        labels: &SpeakerLabels,
        lines: &[TranscriptLine],
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let voices = labels.voices(lines);
        if voices.is_empty() {
            return None;
        }
        let several = labels.several_voices();
        let mut row = h_flex()
            .flex_none()
            .w_full()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .px(px(16.))
            .py(px(10.))
            .border_b_1()
            .border_color(color(HAIRLINE))
            .child(
                div()
                    .mr(px(2.))
                    .text_size(px(11.))
                    .text_color(color(DIM))
                    .child("People"),
            );
        for voice in voices {
            let tint = if several {
                VOICES[usize::from(voice) % VOICES.len()]
            } else {
                ACCENT
            };
            let name = labels.voice_name(voice).map(str::to_owned);
            let editing = self.naming == Some(voice);
            row = row.child(
                chip(("copilot-person", usize::from(voice)), editing)
                    .debug_selector(move || format!("copilot-person-{voice}"))
                    .h(px(24.))
                    .px(px(9.))
                    .child(div().size(px(6.)).rounded_full().bg(color(tint)))
                    .child(
                        div()
                            .text_color(color(TEXT))
                            .child(name.clone().unwrap_or_else(|| labels.voice_label(voice))),
                    )
                    .when(name.is_none(), |chip| {
                        chip.child(div().text_color(color(DIM)).child("Who?"))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.begin_naming(voice, window, cx);
                    })),
            );
        }
        let editor = self.naming.map(|voice| {
            let suggestions = self.model.name_suggestions();
            let named = self.model.speaker_names.contains_key(&voice);
            v_flex()
                .flex_none()
                .w_full()
                .gap(px(8.))
                .px(px(16.))
                .py(px(10.))
                .bg(color(0xffffff06))
                .border_b_1()
                .border_color(color(HAIRLINE))
                .child(
                    h_flex()
                        .w_full()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(12.))
                                .text_color(color(MUTED))
                                .child(format!("Who is {}?", labels.voice_label(voice))),
                        )
                        .child(
                            div()
                                .debug_selector(|| "copilot-name-input".to_owned())
                                .id("copilot-name-input")
                                .flex_1()
                                .min_w_0()
                                .h(px(30.))
                                .px(px(2.))
                                .flex()
                                .items_center()
                                .rounded(px(8.))
                                .bg(color(0xffffff0a))
                                .border_1()
                                .border_color(color(0xe6a48a80))
                                .text_size(px(13.))
                                .child(
                                    Input::new(&self.name_input)
                                        .appearance(false)
                                        .bordered(false),
                                ),
                        )
                        .child(
                            chip("copilot-name-save", true)
                                .debug_selector(|| "copilot-name-save".to_owned())
                                .child("Save")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let name = this.name_input.read(cx).value().to_string();
                                    this.name_voice(voice, &name, cx);
                                })),
                        )
                        .when(named, |row| {
                            row.child(
                                chip("copilot-name-clear", false)
                                    .debug_selector(|| "copilot-name-clear".to_owned())
                                    .child("Forget")
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.name_voice(voice, "", cx);
                                    })),
                            )
                        }),
                )
                .when(!suggestions.is_empty(), |editor| {
                    editor.child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(color(DIM))
                                    .child("On screen"),
                            )
                            .children(suggestions.into_iter().enumerate().map(|(index, name)| {
                                chip(("copilot-name-suggestion", index), false)
                                    .debug_selector(move || {
                                        format!("copilot-name-suggestion-{index}")
                                    })
                                    .h(px(24.))
                                    .child(name.clone())
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.name_voice(voice, &name, cx);
                                    }))
                            })),
                    )
                })
        });
        Some(
            v_flex()
                .flex_none()
                .w_full()
                .child(row)
                .children(editor)
                .into_any_element(),
        )
    }

    fn render_transcript(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let lines = self.model.transcript.clone();
        let labels = self.model.speaker_labels();
        let people = self.render_people(&labels, &lines, cx);
        let speaking_them = self.model.speaking_them;
        let speaking_me = self.model.speaking_me;
        let partials: Vec<(Speaker, String)> = [
            (Speaker::Them, self.model.partial_them.clone()),
            (Speaker::Me, self.model.partial_me.clone()),
        ]
        .into_iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();
        let mut content = v_flex().w_full().gap(px(12.)).p(px(16.));
        if lines.is_empty() && partials.is_empty() && self.model.notes.is_empty() {
            content = content.child(
                div()
                    .pt(px(80.))
                    .w_full()
                    .text_center()
                    .text_size(px(13.))
                    .text_color(color(DIM))
                    .child("The transcript appears here as people speak."),
            );
        }
        let several_voices = has_several_voices(&lines);
        // What the user typed to keep sits where it was typed.
        let mut notes = self.model.notes.clone().into_iter().peekable();
        let note_row = |at_ms: u64, text: String| {
            h_flex()
                .debug_selector(|| "copilot-note".to_owned())
                .w_full()
                .items_start()
                .gap(px(10.))
                .child(
                    v_flex()
                        .w(px(64.))
                        .flex_none()
                        .gap(px(1.))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color(MUTED))
                                .child("Note"),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(color(DIM))
                                .child(format_offset(at_ms)),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px(px(8.))
                        .py(px(4.))
                        .rounded(px(8.))
                        .bg(color(0xffffff0a))
                        .text_size(px(13.))
                        .line_height(rems(1.4))
                        .child(text),
                )
        };
        for line in lines {
            while let Some((at_ms, text)) = notes.next_if(|(at_ms, _)| *at_ms < line.at_ms) {
                content = content.child(note_row(at_ms, text));
            }
            let translation = self.model.translations.get(&line.id).cloned();
            let tint = match (line.speaker, line.voice) {
                (Speaker::Me, _) => ME,
                (Speaker::Them, Some(voice)) if several_voices => {
                    VOICES[usize::from(voice) % VOICES.len()]
                }
                (Speaker::Them, _) => ACCENT,
            };
            let speaker = div()
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color(tint))
                .truncate()
                .child(short_label(&labels, &self.model.speaker_names, &line));
            // The other side's labels open the name editor.
            let speaker = match labels.voice_of(&line) {
                Some(voice) => speaker
                    .id(("copilot-speaker", line.id))
                    .debug_selector(move || format!("copilot-speaker-{}", line.id))
                    .cursor_pointer()
                    .hover(|style| style.underline())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.begin_naming(voice, window, cx);
                    }))
                    .into_any_element(),
                None => speaker.into_any_element(),
            };
            content = content.child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap(px(10.))
                    .child(
                        v_flex()
                            .w(px(64.))
                            .flex_none()
                            .gap(px(1.))
                            .child(speaker)
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(color(DIM))
                                    .child(format_offset(line.at_ms)),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .line_height(rems(1.4))
                                    .child(line.text.clone()),
                            )
                            .when_some(translation, |column, translation| {
                                column.child(
                                    div()
                                        .debug_selector(|| "copilot-translation".to_owned())
                                        .text_size(px(12.5))
                                        .line_height(rems(1.4))
                                        .text_color(color(ACCENT))
                                        .child(translation),
                                )
                            }),
                    ),
            );
        }
        for (at_ms, text) in notes {
            content = content.child(note_row(at_ms, text));
        }
        // Words still being transcribed, replaced by the final line.
        for (speaker, text) in partials {
            let (label, tint) = match speaker {
                Speaker::Me => ("You", ME),
                Speaker::Them => ("Them", ACCENT),
            };
            content = content.child(
                h_flex()
                    .debug_selector(move || format!("copilot-partial-{speaker:?}").to_lowercase())
                    .w_full()
                    .items_start()
                    .gap(px(10.))
                    .child(
                        div()
                            .w(px(64.))
                            .flex_none()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color(tint).opacity(0.7))
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.5))
                            .line_height(rems(1.4))
                            .italic()
                            .text_color(color(DIM))
                            .child(text),
                    ),
            );
        }
        if speaking_them || speaking_me {
            let who = match (speaking_them, speaking_me) {
                (true, true) => "Both speaking",
                (true, false) => "Them speaking",
                _ => "You're speaking",
            };
            content = content.child(
                h_flex()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .text_color(color(DIM))
                    .child(
                        div()
                            .size(px(6.))
                            .rounded_full()
                            .bg(color(ACCENT).opacity(0.3 + 0.7 * pulse(900.))),
                    )
                    .child(who),
            );
        }
        v_flex().size_full().children(people).child(
            div()
                .id("copilot-transcript-body")
                .flex_1()
                .min_h_0()
                .w_full()
                .overflow_y_scroll()
                .track_scroll(&self.transcript_scroll)
                .child(content),
        )
    }

    fn render_footer(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Catching up comes second: it is the one most needed in a hurry.
        let actions: Vec<QuickAction> = [
            QuickAction::WhatToSay,
            QuickAction::CatchUp,
            QuickAction::FollowUps,
            QuickAction::Recap,
            QuickAction::FactCheck,
            QuickAction::ExplainScreen,
        ]
        .into_iter()
        .filter(|action| {
            *action != QuickAction::ExplainScreen || self.model.settings.screen_context
        })
        .collect();
        let catch_up_keys = self.model.shortcuts.catch_up.clone();
        let typing = self.typing;
        v_flex()
            .flex_none()
            .w_full()
            .gap(px(10.))
            .px(px(12.))
            .pt(px(10.))
            .pb(px(12.))
            .border_t_1()
            .border_color(color(HAIRLINE))
            .child(
                h_flex()
                    .id("copilot-quick-row")
                    .w_full()
                    .overflow_x_scroll()
                    .gap(px(6.))
                    .children(actions.into_iter().map(|action| {
                        let keys = (action == QuickAction::CatchUp && !catch_up_keys.is_empty())
                            .then(|| catch_up_keys.clone());
                        chip(("copilot-quick", action as usize), false)
                            .debug_selector(move || format!("copilot-quick-{}", action as usize))
                            .child(action.label())
                            .when_some(keys, |chip, keys| chip.child(keycaps(&keys)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                                this.act(CopilotAction::Quick(action));
                            }))
                    })),
            )
            .child(
                h_flex()
                    .debug_selector(|| "copilot-ask".to_owned())
                    .id("copilot-ask")
                    .w_full()
                    .h(px(42.))
                    .pl(px(14.))
                    .pr(px(6.))
                    .gap(px(8.))
                    .rounded(px(21.))
                    .bg(color(0xffffff0a))
                    .border_1()
                    .border_color(color(if typing { 0xe6a48a80 } else { HAIRLINE }))
                    .cursor_text()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, window, cx| {
                            this.start_typing(window, cx);
                        }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.5))
                            .child(Input::new(&self.ask).appearance(false).bordered(false)),
                    )
                    .when(!typing, |row| row.child(keycaps(&self.model.shortcuts.ask)))
                    .child(
                        div()
                            .debug_selector(|| "copilot-send".to_owned())
                            .id("copilot-send")
                            .size(px(30.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(color(ACCENT))
                            .cursor_pointer()
                            .hover(|style| style.bg(color(0xf0b8a0ff)))
                            .child(icon("arrow-up", 15., 0x1a1110ff))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                let question = this.ask.read(cx).value().trim().to_owned();
                                if question.is_empty() {
                                    this.act(CopilotAction::Assist);
                                } else {
                                    this.ask
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                    this.act(CopilotAction::Ask(question));
                                }
                                this.tab = Tab::Live;
                                this.stop_typing(true, cx);
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_ending(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.model.notes_path.is_some() {
            return self.render_wrap_up(window, cx).into_any_element();
        }
        self.render_writing_notes().into_any_element()
    }

    /// The end of a session: the summary, the follow-up email ready to copy,
    /// and the notes one click away.
    fn render_wrap_up(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let style = TextViewStyle {
            is_dark: true,
            highlight_theme: HighlightTheme::default_dark(),
            ..TextViewStyle::default()
        }
        .paragraph_gap(rems(0.55));
        let topic = self.model.insights.topic.trim().to_owned();
        let summary = self.model.wrap_up.clone();
        let follow_up = self.model.follow_up.clone();
        let mut content = v_flex().w_full().gap(px(16.)).p(px(18.)).child(
            h_flex()
                .gap(px(12.))
                .child(
                    div()
                        .size(px(36.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(color(ACCENT_SOFT))
                        .child(icon("check", 18., ACCENT)),
                )
                .child(
                    v_flex()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Meeting wrapped up"),
                        )
                        .child(div().text_size(px(12.)).text_color(color(MUTED)).child(
                            if topic.is_empty() {
                                "Notes saved with the full transcript".to_owned()
                            } else {
                                topic
                            },
                        )),
                ),
        );
        if let Some(summary) = summary {
            content = content.child(
                div()
                    .debug_selector(|| "copilot-wrap-up-summary".to_owned())
                    .w_full()
                    .text_size(px(13.5))
                    .line_height(rems(1.45))
                    .child(
                        TextView::markdown("copilot-wrap-up-summary", summary, window, cx)
                            .style(style.clone())
                            .selectable(true),
                    ),
            );
        }
        if let Some(email) = follow_up.clone() {
            content = content.child(
                v_flex()
                    .debug_selector(|| "copilot-follow-up".to_owned())
                    .w_full()
                    .gap(px(8.))
                    .p(px(14.))
                    .rounded(px(12.))
                    .bg(color(GLASS_RAISED))
                    .border_1()
                    .border_color(color(HAIRLINE))
                    .child(section_label("Follow-up email"))
                    .child(
                        div()
                            .w_full()
                            .text_size(px(13.5))
                            .line_height(rems(1.45))
                            .child(
                                TextView::markdown("copilot-follow-up-text", email, window, cx)
                                    .style(style)
                                    .selectable(true),
                            ),
                    ),
            );
        }
        let footer = h_flex()
            .w_full()
            .flex_none()
            .gap(px(8.))
            .px(px(16.))
            .py(px(12.))
            .border_t_1()
            .border_color(color(HAIRLINE))
            .when_some(follow_up, |row, email| {
                row.child(
                    chip("copilot-draft-email", true)
                        .debug_selector(|| "copilot-draft-email".to_owned())
                        .child(icon("mail", 12., ACCENT))
                        .child("Draft email")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                            this.act(CopilotAction::DraftEmail);
                        })),
                )
                .child(
                    chip("copilot-copy-follow-up", false)
                        .debug_selector(|| "copilot-copy-follow-up".to_owned())
                        .child(icon("copy", 12., MUTED))
                        .child("Copy email")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                            this.act(CopilotAction::CopyText(email.clone()));
                        })),
                )
            })
            .child(
                chip("copilot-open-notes", false)
                    .debug_selector(|| "copilot-open-notes".to_owned())
                    .child(icon("file-text", 12., MUTED))
                    .child("Open notes")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                        this.act(CopilotAction::OpenNotes);
                    })),
            )
            .child(
                chip("copilot-open-notes-folder", false)
                    .child(icon("folder", 12., MUTED))
                    .child("Notes folder")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                        this.act(CopilotAction::OpenNotesFolder);
                    })),
            )
            .child(div().flex_1())
            .child(
                chip("copilot-done", true)
                    .debug_selector(|| "copilot-done".to_owned())
                    .child("Done")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.act(CopilotAction::End);
                        cx.notify();
                    })),
            );
        v_flex()
            .debug_selector(|| "copilot-wrap-up".to_owned())
            .size_full()
            .child(
                div()
                    .id("copilot-wrap-up-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(content),
            )
            .child(footer)
    }

    fn render_writing_notes(&self) -> impl IntoElement + use<> {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(12.))
            .px(px(40.))
            .child(
                div()
                    .size(px(48.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(color(ACCENT_SOFT))
                    .child(icon("file-text", 22., ACCENT)),
            )
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Writing your meeting notes\u{2026}"),
            )
            .child(
                div()
                    .max_w(px(520.))
                    .text_center()
                    .text_size(px(12.5))
                    .text_color(color(MUTED))
                    .child(
                        "Summarizing key points, decisions, action items, and a follow-up email.",
                    ),
            )
    }

    /// The in-call sheet: only what a call may need changed on the spot.
    /// Everything else is in the main window's Settings, one click away.
    fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let settings = self.model.settings.clone();
        let text_area = |id: &'static str, input: &Entity<InputState>, cx: &mut Context<Self>| {
            div()
                .debug_selector(move || id.to_owned())
                .id(id)
                .w_full()
                .px(px(10.))
                .py(px(6.))
                .rounded(px(10.))
                .bg(color(0xffffff08))
                .border_1()
                .border_color(color(HAIRLINE))
                .text_size(px(13.))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.start_settings_typing(cx)),
                )
                .child(Input::new(input).appearance(false).bordered(false))
        };
        let hint =
            |text: &'static str| div().text_size(px(11.5)).text_color(color(DIM)).child(text);
        let spoken = settings.language.trim().to_lowercase();
        // The choice that changes the copilot most, first and on its own.
        let ask_keys = shortcut_text(&self.model.shortcuts.assist);
        let answer_mode = |id: &'static str,
                           title: &'static str,
                           detail: String,
                           automatic: bool,
                           cx: &mut Context<Self>| {
            let selected = settings.auto_answer == automatic;
            h_flex()
                .debug_selector(move || id.to_owned())
                .id(id)
                .min_w(px(200.))
                .flex_1()
                .items_start()
                .gap(px(10.))
                .p(px(12.))
                .rounded(px(12.))
                .border_1()
                .cursor_pointer()
                .when(selected, |card| {
                    card.bg(color(ACCENT_SOFT)).border_color(color(ACCENT))
                })
                .when(!selected, |card| {
                    card.bg(color(0xffffff08))
                        .border_color(color(HAIRLINE))
                        .hover(|style| style.bg(color(HOVER)))
                })
                .child(
                    div()
                        .mt(px(1.))
                        .size(px(16.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_2()
                        .border_color(color(if selected { ACCENT } else { DIM }))
                        .when(selected, |mark| {
                            mark.child(
                                div()
                                    .debug_selector(move || format!("{id}-selected"))
                                    .size(px(8.))
                                    .rounded_full()
                                    .bg(color(ACCENT)),
                            )
                        }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(13.5))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(color(TEXT))
                                .child(title),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(color(MUTED))
                                .child(detail),
                        ),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.update_settings(|settings| settings.auto_answer = automatic);
                    cx.notify();
                }))
        };
        let content = v_flex()
            .w_full()
            .gap(px(16.))
            .p(px(16.))
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("When to answer"))
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap(px(8.))
                            .child(answer_mode(
                                "copilot-answer-mode-auto",
                                "Automatically",
                                "Answers appear by themselves when the other side asks you something."
                                    .to_owned(),
                                true,
                                cx,
                            ))
                            .child(answer_mode(
                                "copilot-answer-mode-ask",
                                "When I ask",
                                format!(
                                    "Answers only when you press {ask_keys} or Ask AI. Fastest, since nothing waits for them to stop talking."
                                ),
                                false,
                                cx,
                            )),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section_label("Goal for this meeting"))
                    .child(text_area("copilot-goal", &self.goal, cx))
                    .child(hint(
                        "Each part becomes a checklist item that ticks itself off. Cleared when the meeting ends.",
                    ))
                    .child(
                        h_flex().child(
                            chip("copilot-save-settings", true)
                                .debug_selector(|| "copilot-save-settings".to_owned())
                                .child("Save goal")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.save_text_settings(cx);
                                    this.stop_typing(true, cx);
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Meeting type"))
                    .child(h_flex().flex_wrap().gap(px(6.)).children(CopilotMode::ALL.into_iter().map(
                        |mode| {
                            chip(("copilot-mode", mode as usize), mode == settings.mode)
                                .debug_selector(move || format!("copilot-mode-{}", mode as usize))
                                .child(mode.label())
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.update_settings(|settings| settings.mode = mode);
                                    cx.notify();
                                }))
                        },
                    ))),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Spoken language"))
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(px(6.))
                            .child(
                                chip("copilot-language-auto", spoken.is_empty())
                                    .debug_selector(|| "copilot-language-auto".to_owned())
                                    .child("Automatic")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.update_settings(|settings| settings.language.clear());
                                        cx.notify();
                                    })),
                            )
                            .children(LANGUAGES.into_iter().enumerate().map(|(index, (code, name))| {
                                chip(("copilot-language", index), spoken == code)
                                    .debug_selector(move || format!("copilot-language-{code}"))
                                    .child(name)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.update_settings(|settings| {
                                            settings.language = code.to_owned();
                                        });
                                        cx.notify();
                                    }))
                            })),
                    )
                    .child(hint(
                        "Automatic suits most calls. Pick one when names or accents are misheard; it applies from the next meeting.",
                    )),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Your documents"))
                    .child(
                        div()
                            .debug_selector(|| "copilot-knowledge-status".to_owned())
                            .text_size(px(12.))
                            .text_color(color(MUTED))
                            .child(knowledge_status(&self.model.knowledge)),
                    )
                    .when(!self.model.knowledge.unreadable.is_empty(), |column| {
                        column.child(
                            div()
                                .debug_selector(|| "copilot-knowledge-unreadable".to_owned())
                                .text_size(px(11.5))
                                .text_color(color(ACCENT))
                                .child(format!(
                                    "Can't read yet: {}. Save them as Word (.docx), Markdown, or text.",
                                    self.model.knowledge.unreadable.join(", ")
                                )),
                        )
                    })
                    .child(
                        h_flex().child(
                            chip("copilot-open-knowledge", true)
                                .debug_selector(|| "copilot-open-knowledge".to_owned())
                                .child(icon("folder", 12., ACCENT))
                                .child("Open knowledge folder")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                                    this.act(CopilotAction::OpenKnowledgeFolder);
                                })),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(
                        h_flex().child(
                            chip("copilot-more-settings", false)
                                .debug_selector(|| "copilot-more-settings".to_owned())
                                .child(icon("sliders", 12., MUTED))
                                .child("More settings")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                                    this.act(CopilotAction::OpenUrl(
                                        crate::COPILOT_SETTINGS_LINK.to_owned(),
                                    ));
                                })),
                        ),
                    )
                    .child(hint(
                        "The answer engine, models, audio, privacy, and connections are in AgentDictate's Settings, under Meeting Copilot.",
                    )),
            );
        div()
            .debug_selector(|| "copilot-settings-body".to_owned())
            .id("copilot-settings-body")
            .size_full()
            .overflow_y_scroll()
            .child(content)
    }
}

impl Render for CopilotWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_host(cx);
        if !self.panel_open && self.shape.borrow().panel.is_some() {
            self.shape.borrow_mut().panel = None;
            apply_shape(&self.shape, &self.host, cx);
        }
        let bar = if self.notes_ready_shown() {
            self.render_notes_ready(cx).into_any_element()
        } else {
            self.render_bar(cx).into_any_element()
        };
        let panel = self.panel_open.then(|| self.render_panel(window, cx));
        div()
            .id("copilot-root")
            .size_full()
            .font_family(FONT)
            .text_color(color(TEXT))
            .text_size(px(13.))
            .on_key_down(cx.listener(Self::on_key_down))
            .child(bar)
            .children(panel)
    }
}

#[cfg(test)]
mod tests {
    use super::covers_question;

    #[test]
    fn a_reworded_question_is_covered_by_its_answer() {
        assert!(covers_question(
            "James, how long do you keep deleted records around?",
            "How long do you keep deleted records?"
        ));
        assert!(!covers_question(
            "What does pricing look like for forty seats?",
            "Do you support single sign-on?"
        ));
    }
}
