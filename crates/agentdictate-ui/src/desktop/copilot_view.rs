//! The meeting copilot: a floating control bar with a live assistance panel.

use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agentdictate_core::{
    AnswerStatus, CopilotAction, CopilotEffort, CopilotEvent, CopilotMode, CopilotProvider,
    CopilotSettings, ListenState, ModelChoice, QuickAction, Situation, Speaker, format_offset,
    model_pace,
};
use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, Div, Entity, FontWeight, Hsla, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Render, ScrollHandle, SharedString,
    Stateful, Subscription, Window, WindowControlArea, canvas, div, prelude::*, px, rems, svg,
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
const DANGER: u32 = 0xf87171ff;
const SHADOW_MARGIN: f32 = 14.0;
const PILL_HEIGHT: f32 = 38.0;
const PANEL_RADIUS: f32 = 18.0;
const NOTICE_TTL: Duration = Duration::from_secs(4);

fn color(hex: u32) -> Hsla {
    gpui::rgba(hex).into()
}

fn icon(name: &str, size: f32, tint: u32) -> impl IntoElement + use<> {
    svg()
        .path(SharedString::from(format!("icons/{name}.svg")))
        .size(px(size))
        .flex_none()
        .text_color(color(tint))
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

/// A model chip's label with its measured pace, like "GPT-6 Luna  Fastest".
fn model_label(model: &ModelChoice) -> impl IntoElement + use<> {
    h_flex()
        .gap(px(6.))
        .child(model.label.clone())
        .when_some(model_pace(&model.id), |row, pace| {
            row.child(
                div()
                    .text_size(px(10.5))
                    .text_color(color(DIM))
                    .child(pace.label()),
            )
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
    profile: Entity<InputState>,
    instructions: Entity<InputState>,
    anthropic_key: Entity<InputState>,
    openai_key: Entity<InputState>,
    body_scroll: ScrollHandle,
    transcript_scroll: ScrollHandle,
    expanded: HashSet<u64>,
    window_size: Option<(f32, f32)>,
    capture_excluded: bool,
    notice_at: Option<Instant>,
    shape: Rc<RefCell<ShapeTracker>>,
    _subscriptions: Vec<Subscription>,
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
                .placeholder("Ask about your screen or conversation\u{2026}")
                .clean_on_escape()
        });
        let profile = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(3)
                .placeholder("Your role, company, product, goals for this call\u{2026}")
        });
        let instructions = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(2)
                .placeholder("e.g. Keep answers under 50 words. Use metric units.")
        });
        let anthropic_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-ant-\u{2026}")
                .masked(true)
        });
        let openai_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-\u{2026}")
                .masked(true)
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
            profile,
            instructions,
            anthropic_key,
            openai_key,
            body_scroll: ScrollHandle::new(),
            transcript_scroll: ScrollHandle::new(),
            expanded: HashSet::new(),
            window_size: None,
            capture_excluded,
            notice_at: None,
            shape: Rc::default(),
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
        let settings = self.model.settings.clone();
        self.profile.update(cx, |input, cx| {
            input.set_value(settings.profile.clone(), window, cx)
        });
        self.instructions.update(cx, |input, cx| {
            input.set_value(settings.custom_instructions.clone(), window, cx)
        });
        self.anthropic_key.update(cx, |input, cx| {
            input.set_value(settings.anthropic_api_key.clone(), window, cx)
        });
        self.openai_key.update(cx, |input, cx| {
            input.set_value(settings.openai_api_key.clone(), window, cx)
        });
    }

    pub fn apply(&mut self, event: CopilotEvent, window: &mut Window, cx: &mut Context<Self>) {
        let had_notice = self.model.notice.clone();
        let settings_event = matches!(event, CopilotEvent::Settings(_));
        let answer_started = matches!(event, CopilotEvent::AnswerStarted { .. });
        let insights = matches!(event, CopilotEvent::Insights(_));
        let ending = matches!(
            event,
            CopilotEvent::Ending | CopilotEvent::NotesSaved { .. }
        );
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
        if insights && !self.panel_open && !self.user_collapsed {
            self.panel_open = true;
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
        let profile = self.profile.read(cx).value().to_string();
        let instructions = self.instructions.read(cx).value().to_string();
        let anthropic = self.anthropic_key.read(cx).value().trim().to_owned();
        let openai = self.openai_key.read(cx).value().trim().to_owned();
        self.update_settings(|settings| {
            settings.profile = profile;
            settings.custom_instructions = instructions;
            settings.anthropic_api_key = anthropic;
            settings.openai_api_key = openai;
        });
        self.model.notice = Some("Settings saved".into());
        self.notice_at = Some(Instant::now());
    }

    fn desired_size(&self) -> (f32, f32) {
        let height = COPILOT_TOP_MARGIN
            + COPILOT_BAR_HEIGHT
            + if self.panel_open {
                COPILOT_PANEL_GAP + COPILOT_PANEL_HEIGHT
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

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            if self.settings_open {
                self.settings_open = false;
            }
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
        let listen_content: AnyElement =
            match &self.model.listen {
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
                    .child(icon("sparkles", 13., ACCENT))
                    .child(
                        div()
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
            .child(icon_button("copilot-end", "x", MUTED).on_click(cx.listener(
                |this, _: &ClickEvent, _, cx| {
                    this.act(CopilotAction::End);
                    this.stop_typing(true, cx);
                    cx.notify();
                },
            )))
            .child(shape_probe(&self.shape, &self.host, false));
        h_flex()
            .w_full()
            .h(px(COPILOT_BAR_HEIGHT))
            .pt(px(COPILOT_TOP_MARGIN))
            .justify_center()
            .child(pill)
    }

    fn render_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        // After the meeting, the transcript stays one tab away from the wrap-up.
        let body: AnyElement = if self.settings_open {
            self.render_settings(cx).into_any_element()
        } else if self.tab == Tab::Transcript {
            self.render_transcript().into_any_element()
        } else if self.model.ending || self.model.notes_path.is_some() {
            self.render_ending(window, cx).into_any_element()
        } else {
            self.render_live(window, cx).into_any_element()
        };
        let show_footer =
            !self.settings_open && !self.model.ending && self.model.notes_path.is_none();
        div()
            .w_full()
            .px(px(SHADOW_MARGIN))
            .pt(px(COPILOT_PANEL_GAP))
            .child(
                v_flex()
                    .relative()
                    .w_full()
                    .h(px(COPILOT_PANEL_HEIGHT))
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
                    .child(self.render_header(cx))
                    .child(div().flex_1().min_h_0().w_full().child(body))
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
        let mut content = v_flex().w_full().gap(px(14.)).p(px(14.));
        if let Some(answer) = latest {
            content = content.child(self.render_answer(&answer, true, window, cx));
        }
        if let Some(question) = insights.question.clone()
            && !self
                .model
                .answers
                .iter()
                .any(|answer| answer.title.eq_ignore_ascii_case(&question))
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
        if self.model.answers.is_empty() && !has_insights {
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

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let untaught =
            self.model.knowledge.documents == 0 && self.model.settings.profile.trim().is_empty();
        let shortcut = self
            .model
            .shortcuts
            .assist
            .iter()
            .map(|key| {
                if key == "\u{21b5}" {
                    "Enter"
                } else {
                    key.as_str()
                }
            })
            .collect::<Vec<_>>()
            .join("+");
        let headline = match &self.model.listen {
            ListenState::Listening => "Listening to your meeting",
            ListenState::Starting => "Connecting to your audio",
            ListenState::Paused => "Listening is paused",
            ListenState::Unavailable(_) => "Audio is unavailable",
        };
        let detail = match &self.model.listen {
            ListenState::Unavailable(message) => message.clone(),
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
            .when(untaught, |column| {
                column.child(
                    chip("copilot-teach", false)
                        .debug_selector(|| "copilot-teach".to_owned())
                        .mt(px(6.))
                        .child(icon("book", 12., ACCENT))
                        .child("Teach it about you and your business")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.settings_open = true;
                            this.panel_open = true;
                            this.load_text_settings(window, cx);
                            cx.notify();
                        })),
                )
            })
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
        let copy_text = if answer.body.trim().is_empty() {
            answer.instant.clone()
        } else {
            answer.body.clone()
        };
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
        let instant = answer.instant.clone();
        let has_instant = !instant.trim().is_empty();
        let researched_label = if answer.sources.is_empty() {
            "RESEARCHED".to_owned()
        } else {
            format!(
                "RESEARCHED \u{00b7} {} SOURCE{}",
                answer.sources.len(),
                if answer.sources.len() == 1 { "" } else { "S" }
            )
        };
        // With an instant draft on screen, the researched lane's progress is
        // phrased as follow-up work rather than a wait.
        let status = match (&answer.status, has_instant) {
            (AnswerStatus::Thinking, true) => Some("Researching a verified answer".to_owned()),
            _ => status,
        };
        let status_row = status.map(|label| {
            let dots: Vec<f32> = (0..3)
                .map(|index| 0.3 + 0.7 * pulse(1_100. + f64::from(index) * 180.))
                .collect();
            h_flex()
                .gap(px(8.))
                .text_size(px(12.5))
                .text_color(color(if failed { DANGER } else { MUTED }))
                .when(active, |row| {
                    row.child(h_flex().gap(px(3.)).children(dots.into_iter().map(|alpha| {
                        div()
                            .size(px(5.))
                            .rounded_full()
                            .bg(color(ACCENT).opacity(alpha))
                    })))
                })
                .when(matches!(answer.status, AnswerStatus::Searching(_)), |row| {
                    row.child(icon("globe", 12., MUTED))
                })
                .child(div().flex_1().min_w_0().child(label))
        });
        let sources = answer.sources.clone();
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
            .when(has_instant, |card| {
                card.child(
                    h_flex()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(DIM))
                        .child(icon("sparkles", 10., ACCENT))
                        .child("INSTANT"),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(px(14.))
                        .line_height(rems(1.45))
                        .text_color(color(TEXT))
                        .child(
                            TextView::markdown(
                                SharedString::from(format!("copilot-instant-text-{id}")),
                                instant.clone(),
                                window,
                                cx,
                            )
                            .style(style.clone())
                            .selectable(true),
                        ),
                )
                .child(div().w_full().h(px(1.)).bg(color(HAIRLINE)))
            })
            .when_some(status_row, |card, row| card.child(row))
            .when(has_instant && !body.is_empty(), |card| {
                card.child(
                    h_flex()
                        .gap(px(6.))
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color(DIM))
                        .child(icon("globe", 10., ACCENT))
                        .child(researched_label.clone()),
                )
            })
            .when(!body.is_empty(), |card| {
                card.child(
                    div()
                        .w_full()
                        .text_size(px(14.))
                        .line_height(rems(1.45))
                        .text_color(color(TEXT))
                        .child(
                            TextView::markdown(
                                SharedString::from(format!("copilot-answer-text-{id}")),
                                body.clone(),
                                window,
                                cx,
                            )
                            .style(style)
                            .selectable(true),
                        ),
                )
            })
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

    fn render_transcript(&mut self) -> impl IntoElement + use<> {
        let lines = self.model.transcript.clone();
        let speaking_them = self.model.speaking_them;
        let speaking_me = self.model.speaking_me;
        let mut content = v_flex().w_full().gap(px(12.)).p(px(16.));
        if lines.is_empty() {
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
        for line in lines {
            let tint = match line.speaker {
                Speaker::Me => ME,
                Speaker::Them => ACCENT,
            };
            content = content.child(
                h_flex()
                    .w_full()
                    .items_start()
                    .gap(px(10.))
                    .child(
                        v_flex()
                            .w(px(44.))
                            .flex_none()
                            .gap(px(1.))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(color(tint))
                                    .child(line.speaker.display()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(color(DIM))
                                    .child(format_offset(line.at_ms)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(13.5))
                            .line_height(rems(1.4))
                            .child(line.text.clone()),
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
        div()
            .id("copilot-transcript-body")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.transcript_scroll)
            .child(content)
    }

    fn render_footer(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let actions: Vec<QuickAction> = QuickAction::ALL
            .into_iter()
            .filter(|action| {
                *action != QuickAction::ExplainScreen || self.model.settings.screen_context
            })
            .collect();
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
                    .gap(px(6.))
                    .children(actions.into_iter().map(|action| {
                        chip(("copilot-quick", action as usize), false)
                            .debug_selector(move || format!("copilot-quick-{}", action as usize))
                            .child(action.label())
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
                    chip("copilot-copy-follow-up", true)
                        .debug_selector(|| "copilot-copy-follow-up".to_owned())
                        .child(icon("copy", 12., ACCENT))
                        .child("Copy follow-up email")
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

    fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let settings = self.model.settings.clone();
        let current_model = settings.answer_model();
        let current_fast_model = settings.fast_model();
        let models = self.model.models.clone();
        let toggle = |id: &'static str,
                      label: &'static str,
                      detail: &'static str,
                      enabled: bool,
                      cx: &mut Context<Self>,
                      change: fn(&mut CopilotSettings)| {
            h_flex()
                .debug_selector(move || id.to_owned())
                .id(id)
                .w_full()
                .gap(px(12.))
                .px(px(12.))
                .py(px(9.))
                .rounded(px(10.))
                .cursor_pointer()
                .hover(|style| style.bg(color(0xffffff08)))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(1.))
                        .child(div().text_size(px(13.)).child(label))
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(color(DIM))
                                .child(detail),
                        ),
                )
                .child(
                    div()
                        .w(px(34.))
                        .h(px(20.))
                        .flex_none()
                        .rounded(px(10.))
                        .bg(color(if enabled { ACCENT } else { 0xffffff1f }))
                        .relative()
                        .child(
                            div()
                                .absolute()
                                .top(px(2.))
                                .left(px(if enabled { 16. } else { 2. }))
                                .size(px(16.))
                                .rounded_full()
                                .bg(color(if enabled { 0x1a1110ff } else { 0xd4d4d8ff })),
                        ),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.update_settings(change);
                    cx.notify();
                }))
        };
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
        let needs_anthropic = settings.provider == CopilotProvider::AnthropicApi;
        let needs_openai = settings.provider == CopilotProvider::OpenaiApi;
        let content = v_flex()
            .w_full()
            .gap(px(16.))
            .p(px(16.))
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Answer engine"))
                    .child(h_flex().flex_wrap().gap(px(6.)).children(
                        CopilotProvider::ALL.into_iter().map(|provider| {
                            chip(("copilot-provider", provider as usize), provider == settings.provider)
                                .debug_selector(move || format!("copilot-provider-{}", provider as usize))
                                .child(provider.label())
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.update_settings(|settings| settings.provider = provider);
                                    this.model.models = provider.suggested_models();
                                    cx.notify();
                                }))
                        }),
                    ))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(color(DIM))
                            .child(settings.provider.detail()),
                    ),
            )
            .when(needs_anthropic, |column| {
                column.child(
                    v_flex()
                        .gap(px(6.))
                        .child(section_label("Anthropic API key"))
                        .child(text_area("copilot-anthropic-key", &self.anthropic_key, cx)),
                )
            })
            .when(needs_openai, |column| {
                column.child(
                    v_flex()
                        .gap(px(6.))
                        .child(section_label("OpenAI API key"))
                        .child(text_area("copilot-openai-key", &self.openai_key, cx)),
                )
            })
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Researched answers"))
                    .child(h_flex().flex_wrap().gap(px(6.)).children(
                        models.iter().cloned().enumerate().map(|(index, model)| {
                            let id = model.id.clone();
                            chip(("copilot-model", index), model.id == current_model)
                                .child(model_label(&model))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let id = id.clone();
                                    this.update_settings(|settings| settings.set_answer_model(id));
                                    cx.notify();
                                }))
                        }),
                    )),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Instant answers"))
                    .child(h_flex().flex_wrap().gap(px(6.)).children(
                        models.into_iter().enumerate().map(|(index, model)| {
                            let id = model.id.clone();
                            chip(("copilot-fast-model", index), model.id == current_fast_model)
                                .child(model_label(&model))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let id = id.clone();
                                    this.update_settings(|settings| settings.set_fast_model(id));
                                    cx.notify();
                                }))
                        }),
                    )),
            )
            .child(
                h_flex()
                    .gap(px(24.))
                    .items_start()
                    .child(
                        v_flex()
                            .gap(px(8.))
                            .child(section_label("Speed"))
                            .child(h_flex().gap(px(6.)).children(CopilotEffort::ALL.into_iter().map(
                                |effort| {
                                    chip(("copilot-effort", effort as usize), effort == settings.effort)
                                        .child(effort.label())
                                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                            this.update_settings(|settings| settings.effort = effort);
                                            cx.notify();
                                        }))
                                },
                            ))),
                    ),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Meeting type"))
                    .child(h_flex().flex_wrap().gap(px(6.)).children(CopilotMode::ALL.into_iter().map(
                        |mode| {
                            chip(("copilot-mode", mode as usize), mode == settings.mode)
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
                    .gap(px(2.))
                    .child(section_label("Behavior"))
                    .child(toggle(
                        "copilot-web",
                        "Search the web",
                        "Look up current facts while answering.",
                        settings.web_search,
                        cx,
                        |settings| settings.web_search = !settings.web_search,
                    ))
                    .child(toggle(
                        "copilot-instant",
                        "Instant answers",
                        "A fast model drafts a reply in about a second while the main model researches.",
                        settings.instant_answers,
                        cx,
                        |settings| settings.instant_answers = !settings.instant_answers,
                    ))
                    .child(toggle(
                        "copilot-screen",
                        "Use my screen",
                        "Ask AI and typed questions include a screenshot. The copilot never appears in it.",
                        settings.screen_context,
                        cx,
                        |settings| settings.screen_context = !settings.screen_context,
                    ))
                    .child(toggle(
                        "copilot-hidden",
                        "Hide from screen sharing",
                        "Invisible in Zoom, Teams, and Meet shares, recordings, and screenshots.",
                        settings.hide_from_screen_share,
                        cx,
                        |settings| settings.hide_from_screen_share = !settings.hide_from_screen_share,
                    ))
                    .child(toggle(
                        "copilot-auto",
                        "Answer questions automatically",
                        "When they ask you something, an answer starts streaming right away.",
                        settings.auto_answer,
                        cx,
                        |settings| settings.auto_answer = !settings.auto_answer,
                    ))
                    .child(toggle(
                        "copilot-insights",
                        "Live insights",
                        "Rolling topic, summary, and suggested prompts.",
                        settings.live_insights,
                        cx,
                        |settings| settings.live_insights = !settings.live_insights,
                    ))
                    .child(toggle(
                        "copilot-launch",
                        "Open with the dictation shortcut in meetings",
                        "Your dictation shortcut opens the copilot when a call is detected.",
                        settings.launch_from_dictation_hotkey,
                        cx,
                        |settings| {
                            settings.launch_from_dictation_hotkey =
                                !settings.launch_from_dictation_hotkey;
                        },
                    ))
                    .child(toggle(
                        "copilot-mic",
                        "Listen to my microphone",
                        "Transcribes what you say as \u{201c}You\u{201d}.",
                        settings.capture_microphone,
                        cx,
                        |settings| settings.capture_microphone = !settings.capture_microphone,
                    ))
                    .child(toggle(
                        "copilot-system",
                        "Listen to system audio",
                        "Transcribes the other participants as \u{201c}Them\u{201d}.",
                        settings.capture_system_audio,
                        cx,
                        |settings| settings.capture_system_audio = !settings.capture_system_audio,
                    ))
                    .child(toggle(
                        "copilot-notes",
                        "Save meeting notes",
                        "Writes a summary and transcript when you end the session.",
                        settings.save_notes,
                        cx,
                        |settings| settings.save_notes = !settings.save_notes,
                    )),
            )
            .child(
                v_flex()
                    .gap(px(8.))
                    .child(section_label("Your knowledge"))
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
                    )
                    .child(toggle(
                        "copilot-past-meetings",
                        "Remember past meetings",
                        "Answers can draw on notes from your earlier meetings.",
                        settings.use_past_meetings,
                        cx,
                        |settings| settings.use_past_meetings = !settings.use_past_meetings,
                    )),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section_label("About you"))
                    .child(text_area("copilot-profile", &self.profile, cx)),
            )
            .child(
                v_flex()
                    .gap(px(6.))
                    .child(section_label("Standing instructions"))
                    .child(text_area("copilot-instructions", &self.instructions, cx)),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        chip("copilot-save-settings", true)
                            .debug_selector(|| "copilot-save-settings".to_owned())
                            .child("Save")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.save_text_settings(cx);
                                this.stop_typing(true, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        chip("copilot-notes-folder", false)
                            .child(icon("file-text", 12., MUTED))
                            .child("Meeting notes folder")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, _| {
                                this.act(CopilotAction::OpenNotesFolder);
                            })),
                    ),
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
        let bar = self.render_bar(cx);
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
