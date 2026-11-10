//! The Meeting Copilot section of the main window's Settings, so the copilot
//! can be set up without starting a meeting. Choices save at once; text
//! saves with its button. A running meeting keeps its settings until the
//! next one starts.

use std::{cell::RefCell, sync::Arc};

use agentdictate_core::{
    BrainSource, CopilotEffort, CopilotMode, CopilotProvider, CopilotSettings, LANGUAGES,
    ModelChoice, RewriteEngine,
};
use gpui::{AppContext, Context, Entity, Window, prelude::*, px};
use gpui_component::{
    Selectable, Sizable,
    button::ButtonVariants,
    h_flex,
    highlighter::HighlightTheme,
    input::{Input, InputState},
    text::{TextView, TextViewStyle},
    v_flex,
};

use crate::ThemeTokens;
use crate::action::action_button;

use super::{SettingsShell, enabled_label, gpui_color};

type LoadCopilotSettings = dyn Fn() -> CopilotSettings + Send + Sync;
type SaveCopilotSettings = dyn Fn(&CopilotSettings) -> Result<(), String> + Send + Sync;
type AskDocuments = dyn Fn(&str) -> Result<String, String> + Send + Sync;

/// Reads and writes the copilot's settings file for the main window.
#[derive(Clone)]
pub struct CopilotSettingsStore {
    pub load: Arc<LoadCopilotSettings>,
    pub save: Arc<SaveCopilotSettings>,
    /// Answers a question from the knowledge folder alone, in Markdown.
    pub ask_documents: Arc<AskDocuments>,
}

thread_local! {
    /// Per UI thread, so each window's tests stay independent.
    static STORE: RefCell<Option<CopilotSettingsStore>> = const { RefCell::new(None) };
}

/// Lets the main window edit copilot settings; call on the UI thread before
/// it opens.
pub fn set_copilot_settings_store(store: CopilotSettingsStore) {
    STORE.with(|slot| *slot.borrow_mut() = Some(store));
}

fn store() -> Option<CopilotSettingsStore> {
    STORE.with(|slot| slot.borrow().clone())
}

/// A switch: label, detail, whether it is on, and how to flip it.
type Toggle = (&'static str, &'static str, bool, fn(&mut CopilotSettings));

const SAVED: &str = "Saved. Applies when your next meeting starts.";

#[derive(Default)]
pub(super) struct CopilotSettingsState {
    settings: Option<CopilotSettings>,
    openai_key: Option<Entity<InputState>>,
    anthropic_key: Option<Entity<InputState>>,
    profile: Option<Entity<InputState>>,
    instructions: Option<Entity<InputState>>,
    goal: Option<Entity<InputState>>,
    user_name: Option<Entity<InputState>>,
    calendar: Option<Entity<InputState>>,
    webhook: Option<Entity<InputState>>,
    /// A question to try against the knowledge folder.
    docs_question: Option<Entity<InputState>>,
    /// The answer to it, or why there is none, in Markdown.
    docs_answer: Option<String>,
    docs_asking: bool,
    feedback: Option<String>,
}

impl CopilotSettingsState {
    /// Loads the copilot settings when a store was provided.
    pub(super) fn open(window: &mut Window, cx: &mut Context<SettingsShell>) -> Self {
        let Some(store) = store() else {
            return Self::default();
        };
        let settings = (store.load)();
        let openai_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-\u{2026}")
                .masked(true)
                .default_value(settings.openai_api_key.clone())
        });
        let anthropic_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("sk-ant-\u{2026}")
                .masked(true)
                .default_value(settings.anthropic_api_key.clone())
        });
        let profile = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(3)
                .placeholder("Your role, company, product, goals for calls\u{2026}")
                .default_value(settings.profile.clone())
        });
        let instructions = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(2)
                .placeholder("e.g. Keep answers under 50 words. Use metric units.")
                .default_value(settings.custom_instructions.clone())
        });
        let goal = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("e.g. Get a second meeting, ask about budget")
                .default_value(settings.meeting_goal.clone())
        });
        let user_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Your first name, as people say it")
                .default_value(settings.user_name.clone())
        });
        let calendar = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Private .ics address from Outlook or Google Calendar")
                .masked(true)
                .default_value(settings.calendar_ics.clone())
        });
        let webhook = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://\u{2026}")
                .masked(true)
                .default_value(settings.webhook_url.clone())
        });
        let docs_question = cx.new(|cx| {
            InputState::new(window, cx).placeholder("e.g. What does the Team plan cost per seat?")
        });
        Self {
            settings: Some(settings),
            openai_key: Some(openai_key),
            anthropic_key: Some(anthropic_key),
            profile: Some(profile),
            instructions: Some(instructions),
            goal: Some(goal),
            user_name: Some(user_name),
            calendar: Some(calendar),
            webhook: Some(webhook),
            docs_question: Some(docs_question),
            docs_answer: None,
            docs_asking: false,
            feedback: None,
        }
    }

    pub(super) fn model(&self) -> Option<CopilotSectionModel> {
        Some(CopilotSectionModel {
            settings: self.settings.clone()?,
            openai_key: self.openai_key.clone()?,
            anthropic_key: self.anthropic_key.clone()?,
            profile: self.profile.clone()?,
            instructions: self.instructions.clone()?,
            goal: self.goal.clone()?,
            user_name: self.user_name.clone()?,
            calendar: self.calendar.clone()?,
            webhook: self.webhook.clone()?,
            docs_question: self.docs_question.clone()?,
            docs_answer: self.docs_answer.clone(),
            docs_asking: self.docs_asking,
            feedback: self.feedback.clone(),
        })
    }
}

pub(super) struct CopilotSectionModel {
    settings: CopilotSettings,
    openai_key: Entity<InputState>,
    anthropic_key: Entity<InputState>,
    profile: Entity<InputState>,
    instructions: Entity<InputState>,
    goal: Entity<InputState>,
    user_name: Entity<InputState>,
    calendar: Entity<InputState>,
    webhook: Entity<InputState>,
    docs_question: Entity<InputState>,
    docs_answer: Option<String>,
    docs_asking: bool,
    feedback: Option<String>,
}

impl SettingsShell {
    /// Applies a choice to the copilot settings and saves it at once.
    fn update_copilot(
        &mut self,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut CopilotSettings),
    ) {
        let Some(settings) = self.copilot.settings.as_mut() else {
            return;
        };
        change(settings);
        self.copilot.feedback = Some(save(settings));
        cx.notify();
    }

    /// Saves the copilot's text fields: API keys, About you, instructions.
    fn save_copilot_text(&mut self, cx: &mut Context<Self>) {
        let read = |input: &Option<Entity<InputState>>, cx: &Context<Self>| {
            input
                .as_ref()
                .map(|input| input.read(cx).value().trim().to_owned())
        };
        let openai = read(&self.copilot.openai_key, cx);
        let anthropic = read(&self.copilot.anthropic_key, cx);
        let profile = read(&self.copilot.profile, cx);
        let instructions = read(&self.copilot.instructions, cx);
        let goal = read(&self.copilot.goal, cx);
        let user_name = read(&self.copilot.user_name, cx);
        let calendar = read(&self.copilot.calendar, cx);
        let webhook = read(&self.copilot.webhook, cx);
        self.update_copilot(cx, |settings| {
            if let Some(goal) = goal {
                settings.meeting_goal = goal;
            }
            if let Some(user_name) = user_name {
                settings.user_name = user_name;
            }
            if let Some(calendar) = calendar {
                settings.calendar_ics = calendar;
            }
            if let Some(webhook) = webhook {
                settings.webhook_url = webhook;
            }
            if let Some(key) = openai {
                settings.openai_api_key = key;
            }
            if let Some(key) = anthropic {
                settings.anthropic_api_key = key;
            }
            if let Some(profile) = profile {
                settings.profile = profile;
            }
            if let Some(instructions) = instructions {
                settings.custom_instructions = instructions;
            }
        });
    }
}

impl SettingsShell {
    /// Tries a question against the knowledge folder, so the user sees what
    /// the copilot would say, and from which document, before a call.
    fn ask_copilot_documents(&mut self, cx: &mut Context<Self>) {
        let Some(store) = store() else {
            return;
        };
        let question = self
            .copilot
            .docs_question
            .as_ref()
            .map(|input| input.read(cx).value().trim().to_owned())
            .unwrap_or_default();
        if question.is_empty() || self.copilot.docs_asking {
            return;
        }
        self.copilot.docs_asking = true;
        self.copilot.docs_answer = None;
        cx.notify();
        let task = cx.background_spawn(async move { (store.ask_documents)(&question) });
        cx.spawn(async move |shell, cx| {
            let result = task.await;
            if let Some(shell) = shell.upgrade() {
                shell
                    .update(cx, |shell, cx| {
                        shell.copilot.docs_asking = false;
                        shell.copilot.docs_answer = Some(match result {
                            Ok(answer) => answer,
                            Err(error) => format!("Could not answer: {error}"),
                        });
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }
}

fn save(settings: &CopilotSettings) -> String {
    match store() {
        Some(store) => match (store.save)(settings) {
            Ok(()) => SAVED.to_owned(),
            Err(error) => format!("Could not save: {error}"),
        },
        None => "Copilot settings are not available here".to_owned(),
    }
}

/// The models offered for a provider, keeping the current pick listed.
fn models(provider: CopilotProvider, current: &str) -> Vec<ModelChoice> {
    let mut models = provider.suggested_models();
    if !current.is_empty() && !models.iter().any(|model| model.id == current) {
        models.insert(0, ModelChoice::new(current, current));
    }
    models
}

pub(super) fn section(
    model: CopilotSectionModel,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    let CopilotSectionModel {
        settings,
        openai_key,
        anthropic_key,
        profile,
        instructions,
        goal,
        user_name,
        calendar,
        webhook,
        docs_question,
        docs_answer,
        docs_asking,
        feedback,
    } = model;
    let provider = settings.provider;
    // The OpenAI key also transcribes the call, so it is always offered.
    let mut keys = Vec::new();
    if provider == CopilotProvider::AnthropicApi {
        keys.push((
            "copilot-settings-key",
            "Anthropic API key",
            "Answers with Claude; stored on this computer",
            anthropic_key,
            "copilot-settings-save-key",
        ));
    }
    keys.push((
        if provider == CopilotProvider::OpenaiApi {
            "copilot-settings-key"
        } else {
            "copilot-settings-openai-key"
        },
        "OpenAI API key",
        if provider == CopilotProvider::OpenaiApi {
            "Answers, transcribes calls and dictation; stored on this computer"
        } else {
            "Transcribes calls and dictation; stored on this computer"
        },
        openai_key,
        "copilot-settings-save-openai-key",
    ));
    let answer_model = settings.answer_model();
    let fast_model = settings.fast_model();

    let provider_chips = CopilotProvider::ALL
        .into_iter()
        .map(|choice| {
            chip(
                format!("copilot-settings-provider-{}", choice as usize),
                choice.label(),
                choice == provider,
                cx,
                move |settings| settings.provider = choice,
            )
        })
        .collect::<Vec<_>>();
    let answer_chips = models(provider, &answer_model)
        .into_iter()
        .enumerate()
        .map(|(index, choice)| {
            let id = choice.id.clone();
            chip(
                format!("copilot-settings-answer-{index}"),
                choice.label.clone(),
                choice.id == answer_model,
                cx,
                move |settings| settings.set_answer_model(id.clone()),
            )
        })
        .collect::<Vec<_>>();
    let fast_chips = models(provider, &fast_model)
        .into_iter()
        .enumerate()
        .map(|(index, choice)| {
            let id = choice.id.clone();
            chip(
                format!("copilot-settings-fast-{index}"),
                choice.label.clone(),
                choice.id == fast_model,
                cx,
                move |settings| settings.set_fast_model(id.clone()),
            )
        })
        .collect::<Vec<_>>();
    let mode_chips = CopilotMode::ALL
        .into_iter()
        .map(|mode| {
            chip(
                format!("copilot-settings-mode-{}", mode as usize),
                mode.label(),
                mode == settings.mode,
                cx,
                move |settings| settings.mode = mode,
            )
        })
        .collect::<Vec<_>>();

    let effort_chips = CopilotEffort::ALL
        .into_iter()
        .map(|effort| {
            chip(
                format!("copilot-settings-effort-{}", effort as usize),
                effort.label(),
                effort == settings.effort,
                cx,
                move |settings| settings.effort = effort,
            )
        })
        .collect::<Vec<_>>();
    let rewrite_chips = RewriteEngine::ALL
        .into_iter()
        .enumerate()
        .map(|(index, engine)| {
            chip(
                format!("copilot-settings-rewrite-{index}"),
                engine.label(),
                engine == settings.rewrite_engine,
                cx,
                move |settings| settings.rewrite_engine = engine,
            )
        })
        .collect::<Vec<_>>();
    let brain_chips = BrainSource::ALL
        .into_iter()
        .enumerate()
        .map(|(index, source)| {
            chip(
                format!("copilot-settings-brain-{index}"),
                source.label(),
                source == settings.brain_source,
                cx,
                move |settings| settings.brain_source = source,
            )
        })
        .collect::<Vec<_>>();
    let spoken = settings.language.trim().to_lowercase();
    let mut language_chips = vec![chip(
        "copilot-settings-language-auto".to_owned(),
        "Automatic",
        spoken.is_empty(),
        cx,
        |settings| settings.language.clear(),
    )];
    language_chips.extend(LANGUAGES.into_iter().map(|(code, name)| {
        chip(
            format!("copilot-settings-language-{code}"),
            name,
            spoken == code,
            cx,
            move |settings| settings.language = code.to_owned(),
        )
    }));
    let translate = settings.translate_to.trim().to_lowercase();
    let mut translate_chips = vec![chip(
        "copilot-settings-translate-off".to_owned(),
        "Off",
        translate.is_empty(),
        cx,
        |settings| settings.translate_to.clear(),
    )];
    translate_chips.extend(LANGUAGES.into_iter().map(|(code, name)| {
        chip(
            format!("copilot-settings-translate-{code}"),
            name,
            translate == code,
            cx,
            move |settings| settings.translate_to = code.to_owned(),
        )
    }));

    let toggles: [Toggle; 13] = [
        (
            "Answer questions automatically",
            "Show an answer when someone asks you something",
            settings.auto_answer,
            |s| s.auto_answer = !s.auto_answer,
        ),
        (
            "Instant answers",
            "A fast draft appears first while a fuller answer follows",
            settings.instant_answers,
            |s| s.instant_answers = !s.instant_answers,
        ),
        (
            "Search the web",
            "Fuller answers may look things up",
            settings.web_search,
            |s| s.web_search = !s.web_search,
        ),
        (
            "Save meeting notes",
            "Notes and a follow-up email land on the Meetings page",
            settings.save_notes,
            |s| s.save_notes = !s.save_notes,
        ),
        (
            "Open with the dictation shortcut in meetings",
            "The dictation shortcut starts the copilot during a call",
            settings.launch_from_dictation_hotkey,
            |s| s.launch_from_dictation_hotkey = !s.launch_from_dictation_hotkey,
        ),
        (
            "Hide from screen sharing",
            "Others never see the copilot when you share your screen",
            settings.hide_from_screen_share,
            |s| s.hide_from_screen_share = !s.hide_from_screen_share,
        ),
        (
            "Listen to my microphone",
            "Hear your side of the call",
            settings.capture_microphone,
            |s| s.capture_microphone = !s.capture_microphone,
        ),
        (
            "Listen to system audio",
            "Hear the other side of the call",
            settings.capture_system_audio,
            |s| s.capture_system_audio = !s.capture_system_audio,
        ),
        (
            "Open when a meeting starts",
            "The copilot opens by itself when a call begins, without the shortcut",
            settings.auto_start,
            |s| s.auto_start = !s.auto_start,
        ),
        (
            "Compact panel",
            "Show only the line to say",
            settings.compact,
            |s| s.compact = !s.compact,
        ),
        (
            "Live insights",
            "Rolling topic, checklist, and what you promised",
            settings.live_insights,
            |s| s.live_insights = !s.live_insights,
        ),
        (
            "Use my screen",
            "Typed questions include a screenshot; the copilot never appears in it",
            settings.screen_context,
            |s| s.screen_context = !s.screen_context,
        ),
        (
            "Remember past meetings",
            "Answers and the pre-call brief draw on your earlier meetings",
            settings.use_past_meetings,
            |s| s.use_past_meetings = !s.use_past_meetings,
        ),
    ];

    let mut column = v_flex()
        .w_full()
        .debug_selector(|| "settings-group-copilot".to_owned())
        .child(
            v_flex()
                .gap_1()
                .pb_3()
                .child(
                    gpui::div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .child("Meeting Copilot"),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(gpui_color(theme.text_muted))
                        .child("How the meeting copilot answers. Applies when your next meeting starts."),
                ),
        )
        .child(chip_row(
            "Answer engine",
            provider.detail(),
            provider_chips,
            theme,
        ));
    for (selector, label, detail, input, save) in keys {
        column = column.child(
            row(theme)
                .debug_selector(move || selector.to_owned())
                .child(label_block(label, detail, theme))
                .child(
                    h_flex()
                        .w(px(360.))
                        .max_w(gpui::relative(1.))
                        .gap_2()
                        .child(
                            gpui::div()
                                .min_w_0()
                                .flex_1()
                                .child(Input::new(&input).small()),
                        )
                        .child(save_button(save, cx)),
                ),
        );
    }
    column = column
        .child(chip_row(
            "Researched answers",
            "The model behind full answers",
            answer_chips,
            theme,
        ))
        .child(chip_row(
            "Instant answers",
            "The quick model that drafts first",
            fast_chips,
            theme,
        ))
        .child(chip_row(
            "Rewrites",
            "What rewrites the line to say",
            rewrite_chips,
            theme,
        ))
        .child(chip_row(
            "Speed",
            "How long full answers may think",
            effort_chips,
            theme,
        ))
        .child(chip_row(
            "Meeting type",
            "Shapes answers, the checklist, and the notes",
            mode_chips,
            theme,
        ))
        .child(text_block(
            "Goal for your next meeting",
            "Each part becomes a checklist item that ticks itself off; cleared when the meeting ends",
            goal,
            theme,
        ))
        .child(chip_row(
            "Spoken language",
            "Automatic suits most calls; pick one when names or accents are misheard",
            language_chips,
            theme,
        ))
        .child(chip_row(
            "Translate what they say into",
            "Their lines in another language get a translation in the transcript",
            translate_chips,
            theme,
        ))
        .child(chip_row(
            "Brain",
            "Company knowledge used alongside your knowledge folder",
            brain_chips,
            theme,
        ));
    for (index, (label, detail, enabled, toggle)) in toggles.into_iter().enumerate() {
        column = column.child(
            row(theme).child(label_block(label, detail, theme)).child(
                action_button(("copilot-settings-toggle", index))
                    .debug_selector(move || format!("copilot-settings-toggle-{index}"))
                    .small()
                    .selected(enabled)
                    .label(enabled_label(enabled))
                    .on_click(cx.listener(move |shell, _, _, cx| {
                        shell.update_copilot(cx, toggle);
                    })),
            ),
        );
    }
    let asked = docs_answer.map(|answer| {
        let style = TextViewStyle {
            is_dark: true,
            highlight_theme: HighlightTheme::default_dark(),
            ..TextViewStyle::default()
        };
        gpui::div()
            .debug_selector(|| "copilot-settings-docs-answer".to_owned())
            .w_full()
            .text_sm()
            .text_color(gpui_color(theme.text))
            .child(
                TextView::markdown("copilot-settings-docs-answer-text", answer, window, cx)
                    .style(style)
                    .selectable(true),
            )
    });
    column
        .child(
            v_flex()
                .w_full()
                .gap_2()
                .border_b_1()
                .border_color(gpui_color(theme.border))
                .py_3()
                .child(label_block(
                    "Ask your documents",
                    "Try a question before a call: see the answer and which document it comes from",
                    theme,
                ))
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            gpui::div()
                                .debug_selector(|| "copilot-settings-docs-question".to_owned())
                                .min_w_0()
                                .flex_1()
                                .child(Input::new(&docs_question).small().w_full()),
                        )
                        .child(
                            action_button("copilot-settings-docs-ask")
                                .debug_selector(|| "copilot-settings-docs-ask".to_owned())
                                .small()
                                .primary()
                                .label(if docs_asking { "Asking\u{2026}" } else { "Ask" })
                                .on_click(cx.listener(|shell, _, _, cx| {
                                    shell.ask_copilot_documents(cx);
                                })),
                        ),
                )
                .when_some(asked, |column, answer| column.child(answer)),
        )
        .child(text_block(
            "Your name",
            "When you are only listening and someone says it, the copilot alerts you",
            user_name,
            theme,
        ))
        .child(text_block(
            "Calendar",
            "A private .ics address names the meeting and who is in it before anyone speaks",
            calendar,
            theme,
        ))
        .child(text_block(
            "Send finished meetings to",
            "A webhook for your CRM, Notion, Zapier, or Make; it receives the summary, commitments, and transcript",
            webhook,
            theme,
        ))
        .child(text_block(
            "About you",
            "Background answers can draw on",
            profile,
            theme,
        ))
        .child(text_block(
            "Standing instructions",
            "Applied to every answer",
            instructions,
            theme,
        ))
        .child(
            h_flex()
                .py_3()
                .gap_3()
                .items_center()
                .child(save_button("copilot-settings-save-text", cx))
                .when_some(feedback, |row, feedback| {
                    row.child(
                        gpui::div()
                            .debug_selector(|| "copilot-settings-feedback".to_owned())
                            .text_xs()
                            .text_color(gpui_color(theme.text_muted))
                            .child(feedback),
                    )
                }),
        )
}

fn row(theme: ThemeTokens) -> gpui::Div {
    h_flex()
        .min_h(px(52.))
        .flex_wrap()
        .justify_between()
        .gap_6()
        .border_b_1()
        .border_color(gpui_color(theme.border))
        .py_2()
}

fn label_block(label: &'static str, detail: &'static str, theme: ThemeTokens) -> gpui::Div {
    v_flex()
        .min_w(px(220.))
        .flex_1()
        .gap_0p5()
        .child(gpui::div().text_sm().child(label))
        .child(
            gpui::div()
                .text_xs()
                .text_color(gpui_color(theme.text_muted))
                .child(detail),
        )
}

fn chip_row(
    label: &'static str,
    detail: &'static str,
    chips: impl IntoIterator<Item = gpui_component::button::Button>,
    theme: ThemeTokens,
) -> gpui::Div {
    v_flex()
        .w_full()
        .gap_2()
        .border_b_1()
        .border_color(gpui_color(theme.border))
        .py_3()
        .child(label_block(label, detail, theme))
        .child(h_flex().flex_wrap().gap_2().children(chips))
}

fn text_block(
    label: &'static str,
    detail: &'static str,
    input: Entity<InputState>,
    theme: ThemeTokens,
) -> gpui::Div {
    v_flex()
        .w_full()
        .gap_2()
        .border_b_1()
        .border_color(gpui_color(theme.border))
        .py_3()
        .child(label_block(label, detail, theme))
        .child(Input::new(&input).small().w_full())
}

fn chip(
    id: String,
    label: impl Into<gpui::SharedString>,
    selected: bool,
    cx: &mut Context<SettingsShell>,
    change: impl Fn(&mut CopilotSettings) + Clone + 'static,
) -> gpui_component::button::Button {
    let selector = id.clone();
    action_button(gpui::ElementId::Name(id.into()))
        .debug_selector(move || selector.clone())
        .small()
        .selected(selected)
        .label(label.into())
        .on_click(cx.listener(move |shell, _, _, cx| {
            let change = change.clone();
            shell.update_copilot(cx, move |settings| change(settings));
        }))
}

fn save_button(
    selector: &'static str,
    cx: &mut Context<SettingsShell>,
) -> gpui_component::button::Button {
    action_button(selector)
        .debug_selector(move || selector.to_owned())
        .small()
        .primary()
        .label("Save")
        .on_click(cx.listener(|shell, _, _, cx| shell.save_copilot_text(cx)))
}
