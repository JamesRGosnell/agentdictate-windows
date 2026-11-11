//! The Meeting Copilot section of the main window's Settings, so the copilot
//! can be set up without starting a meeting. It shows as a few titled
//! groups, with rarely changed settings behind a collapsed "Advanced" group.
//! Choices save at once; text saves with any Save button. A running meeting
//! keeps its settings until the next one starts.

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

use super::{
    SettingsShell, enabled_label, gpui_color,
    settings_page::{SettingsControlKind, control_slot, setting_label, settings_section},
};

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
    assemblyai_key: Option<Entity<InputState>>,
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
    /// Whether the rarely changed settings are shown; closed on each open.
    advanced_open: bool,
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
        let assemblyai_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("AssemblyAI key")
                .masked(true)
                .default_value(settings.assemblyai_api_key.clone())
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
            assemblyai_key: Some(assemblyai_key),
            profile: Some(profile),
            instructions: Some(instructions),
            goal: Some(goal),
            user_name: Some(user_name),
            calendar: Some(calendar),
            webhook: Some(webhook),
            docs_question: Some(docs_question),
            docs_answer: None,
            docs_asking: false,
            advanced_open: false,
            feedback: None,
        }
    }

    pub(super) fn model(&self) -> Option<CopilotSectionModel> {
        Some(CopilotSectionModel {
            settings: self.settings.clone()?,
            openai_key: self.openai_key.clone()?,
            anthropic_key: self.anthropic_key.clone()?,
            assemblyai_key: self.assemblyai_key.clone()?,
            profile: self.profile.clone()?,
            instructions: self.instructions.clone()?,
            goal: self.goal.clone()?,
            user_name: self.user_name.clone()?,
            calendar: self.calendar.clone()?,
            webhook: self.webhook.clone()?,
            docs_question: self.docs_question.clone()?,
            docs_answer: self.docs_answer.clone(),
            docs_asking: self.docs_asking,
            advanced_open: self.advanced_open,
            feedback: self.feedback.clone(),
        })
    }
}

pub(super) struct CopilotSectionModel {
    settings: CopilotSettings,
    openai_key: Entity<InputState>,
    anthropic_key: Entity<InputState>,
    assemblyai_key: Entity<InputState>,
    profile: Entity<InputState>,
    instructions: Entity<InputState>,
    goal: Entity<InputState>,
    user_name: Entity<InputState>,
    calendar: Entity<InputState>,
    webhook: Entity<InputState>,
    docs_question: Entity<InputState>,
    docs_answer: Option<String>,
    docs_asking: bool,
    advanced_open: bool,
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
        let assemblyai = read(&self.copilot.assemblyai_key, cx);
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
            if let Some(key) = assemblyai {
                settings.assemblyai_api_key = key;
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

/// Builds the copilot's settings as a few titled groups, most-changed first,
/// with the technical ones behind a collapsed "Advanced" group.
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
        assemblyai_key,
        profile,
        instructions,
        goal,
        user_name,
        calendar,
        webhook,
        docs_question,
        docs_answer,
        docs_asking,
        advanced_open,
        feedback,
    } = model;
    let provider = settings.provider;
    // The OpenAI key also transcribes the call, so it is always offered.
    let mut keys = Vec::new();
    if provider == CopilotProvider::AnthropicApi {
        keys.push((
            "copilot-settings-key",
            "Anthropic API key",
            "For Claude answers. Stored on this computer.",
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
            "For answers and transcription. Stored on this computer."
        } else {
            "For transcription. Stored on this computer."
        },
        openai_key,
        "copilot-settings-save-openai-key",
    ));
    // Optional: its model ends a speaker's turn sooner, so answers start
    // sooner.
    keys.push((
        "copilot-settings-assemblyai-key",
        "AssemblyAI API key",
        "Optional. Transcribes calls faster than OpenAI.",
        assemblyai_key,
        "copilot-settings-save-assemblyai-key",
    ));
    let key_rows = keys
        .into_iter()
        .map(|(selector, label, detail, input, save)| {
            input_save_row(selector, label, detail, &input, save, theme, cx)
        })
        .collect::<Vec<_>>();
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

    // The order fixes each switch's `copilot-settings-toggle-N` selector, so
    // new switches go at the end; where each one shows is decided below.
    let toggles: [Toggle; 13] = [
        (
            "Answer questions automatically",
            "Show an answer when someone asks you something",
            settings.auto_answer,
            |s| s.auto_answer = !s.auto_answer,
        ),
        (
            "Instant answers",
            "A quick draft shows while the full answer is written",
            settings.instant_answers,
            |s| s.instant_answers = !s.instant_answers,
        ),
        (
            "Search the web",
            "Full answers can look things up",
            settings.web_search,
            |s| s.web_search = !s.web_search,
        ),
        (
            "Save meeting notes",
            "Notes and a follow-up email go to the Meetings page",
            settings.save_notes,
            |s| s.save_notes = !s.save_notes,
        ),
        (
            "Open with the dictation shortcut",
            "During a call, the dictation shortcut opens the copilot",
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
            "No shortcut needed when a call begins",
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
            "The topic, a checklist, and what you promised",
            settings.live_insights,
            |s| s.live_insights = !s.live_insights,
        ),
        (
            "Use my screen",
            "Typed questions include a screenshot, without the copilot in it",
            settings.screen_context,
            |s| s.screen_context = !s.screen_context,
        ),
        (
            "Remember past meetings",
            "Answers and the pre-call brief use your earlier meetings",
            settings.use_past_meetings,
            |s| s.use_past_meetings = !s.use_past_meetings,
        ),
    ];
    let mut index = 0;
    let [
        auto_answer,
        instant_answers,
        web_search,
        save_notes,
        dictation_shortcut,
        hide_from_sharing,
        microphone,
        system_audio,
        auto_start,
        compact,
        live_insights,
        screen,
        past_meetings,
    ] = toggles.map(|toggle| {
        let row = toggle_row(index, toggle, theme, cx);
        index += 1;
        row
    });

    let engine = settings_section(
        "settings-group-copilot-engine",
        "Meeting Copilot",
        "Answers questions during your calls. Changes apply when your next meeting starts.",
        false,
        theme,
    )
    .child(chip_row(
        "Answer engine",
        provider.detail(),
        provider_chips,
        theme,
    ))
    .children(key_rows);

    let answers = settings_section(
        "settings-group-copilot-answers",
        "Answers",
        "When the copilot answers, and which models write the answers.",
        true,
        theme,
    )
    .child(chip_row(
        "Meeting type",
        "Shapes answers, the checklist, and the notes",
        mode_chips,
        theme,
    ))
    .child(auto_answer)
    .child(instant_answers)
    .child(web_search)
    .child(chip_row(
        "Full answer model",
        "Writes the researched answer",
        answer_chips,
        theme,
    ))
    .child(chip_row(
        "Instant answer model",
        "Writes the quick draft",
        fast_chips,
        theme,
    ));

    let during = settings_section(
        "settings-group-copilot-call",
        "During the call",
        "What the copilot hears and what it shows you.",
        true,
        theme,
    )
    .child(microphone)
    .child(system_audio)
    .child(chip_row(
        "Spoken language",
        "Pick one if names or accents are misheard",
        language_chips,
        theme,
    ))
    .child(chip_row(
        "Translate what they say",
        "Adds a translation under their lines in the transcript",
        translate_chips,
        theme,
    ))
    .child(live_insights)
    .child(screen)
    .child(compact)
    .child(hide_from_sharing);

    let around = settings_section(
        "settings-group-copilot-around",
        "Before and after the call",
        "How the copilot opens, your goal, and what it keeps.",
        true,
        theme,
    )
    .child(auto_start)
    .child(dictation_shortcut)
    .child(input_save_row(
        "copilot-settings-goal",
        "Goal for your next meeting",
        "Each part becomes a checklist item; cleared after the meeting",
        &goal,
        "copilot-settings-save-goal",
        theme,
        cx,
    ))
    .child(save_notes)
    .child(past_meetings);

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
    let about = settings_section(
        "settings-group-copilot-about",
        "You and your documents",
        "What answers can draw on.",
        true,
        theme,
    )
    .child(input_save_row(
        "copilot-settings-name",
        "Your name",
        "Alerts you when someone says it while you only listen",
        &user_name,
        "copilot-settings-save-name",
        theme,
        cx,
    ))
    .child(text_area_block(
        "About you",
        "Your role, company, and product",
        &profile,
        "copilot-settings-save-text",
        theme,
        cx,
    ))
    .child(
        v_flex()
            .w_full()
            .gap_2()
            .border_b_1()
            .border_color(gpui_color(theme.border))
            .py_3()
            .child(setting_label(
                "Ask your documents",
                "Try a question and see which document answers it",
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
    );

    let advanced = v_flex()
        .w_full()
        .debug_selector(|| "settings-group-copilot-advanced".to_owned())
        .child(
            h_flex()
                .w_full()
                .items_start()
                .justify_between()
                .gap_6()
                .pb_3()
                .child(
                    v_flex()
                        .min_w_0()
                        .flex_1()
                        .gap_1()
                        .child(
                            gpui::div()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Advanced"),
                        )
                        .child(
                            gpui::div()
                                .text_xs()
                                .text_color(gpui_color(theme.text_muted))
                                .child(
                                    "Thinking time, rewrites, company knowledge, custom \
                                     instructions, calendar, and webhook.",
                                ),
                        ),
                )
                .child(
                    action_button("copilot-settings-advanced")
                        .debug_selector(|| "copilot-settings-advanced".to_owned())
                        .small()
                        .label(if advanced_open { "Hide" } else { "Show" })
                        .on_click(cx.listener(|shell, _, _, cx| {
                            shell.copilot.advanced_open = !shell.copilot.advanced_open;
                            cx.notify();
                        })),
                ),
        )
        .when(advanced_open, |group| {
            group
                .child(chip_row(
                    "Thinking time",
                    "How long full answers may think",
                    effort_chips,
                    theme,
                ))
                .child(chip_row(
                    "Rewrite engine",
                    "Rewrites the line to say when you ask",
                    rewrite_chips,
                    theme,
                ))
                .child(chip_row(
                    "Company knowledge",
                    "Used alongside your knowledge folder",
                    brain_chips,
                    theme,
                ))
                .child(text_area_block(
                    "Custom instructions",
                    "Applied to every answer",
                    &instructions,
                    "copilot-settings-save-instructions",
                    theme,
                    cx,
                ))
                .child(input_save_row(
                    "copilot-settings-calendar",
                    "Calendar feed",
                    "A private .ics address; names the meeting and who attends",
                    &calendar,
                    "copilot-settings-save-calendar",
                    theme,
                    cx,
                ))
                .child(input_save_row(
                    "copilot-settings-webhook",
                    "Send finished meetings to",
                    "A webhook for your CRM, Notion, Zapier, or Make",
                    &webhook,
                    "copilot-settings-save-webhook",
                    theme,
                    cx,
                ))
        });

    v_flex()
        .w_full()
        .gap_6()
        .debug_selector(|| "settings-group-copilot".to_owned())
        .child(engine)
        .child(answers)
        .child(during)
        .child(around)
        .child(about)
        .child(advanced)
        .when_some(feedback, |column, feedback| {
            column.child(
                gpui::div()
                    .debug_selector(|| "copilot-settings-feedback".to_owned())
                    .text_xs()
                    .text_color(gpui_color(theme.text_muted))
                    .child(feedback),
            )
        })
}

/// A switch row, laid out like the page's other switches.
fn toggle_row(
    index: usize,
    (label, detail, enabled, toggle): Toggle,
    theme: ThemeTokens,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    h_flex()
        .min_h(px(52.))
        .justify_between()
        .gap_6()
        .border_b_1()
        .border_color(gpui_color(theme.border))
        .py_2()
        .child(setting_label(label, detail, theme))
        .child(
            action_button(("copilot-settings-toggle", index))
                .debug_selector(move || format!("copilot-settings-toggle-{index}"))
                .small()
                .selected(enabled)
                .label(enabled_label(enabled))
                .on_click(cx.listener(move |shell, _, _, cx| {
                    shell.update_copilot(cx, toggle);
                })),
        )
}

/// A one-line field with its own Save, laid out like the page's API key row.
fn input_save_row(
    selector: &'static str,
    label: &'static str,
    detail: &'static str,
    input: &Entity<InputState>,
    save: &'static str,
    theme: ThemeTokens,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    h_flex()
        .debug_selector(move || selector.to_owned())
        .min_h(px(54.))
        .items_start()
        .flex_wrap()
        .justify_between()
        .gap_6()
        .border_b_1()
        .border_color(gpui_color(theme.border))
        .py_2()
        .child(setting_label(label, detail, theme))
        .child(
            control_slot(selector, SettingsControlKind::Credential)
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    gpui::div()
                        .min_w_0()
                        .flex_1()
                        .child(Input::new(input).small()),
                )
                .child(save_button(save, cx)),
        )
}

/// A multi-line field across the page width, with its Save underneath.
fn text_area_block(
    label: &'static str,
    detail: &'static str,
    input: &Entity<InputState>,
    save: &'static str,
    theme: ThemeTokens,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    v_flex()
        .w_full()
        .gap_2()
        .border_b_1()
        .border_color(gpui_color(theme.border))
        .py_3()
        .child(setting_label(label, detail, theme))
        .child(Input::new(input).small().w_full())
        .child(h_flex().justify_end().child(save_button(save, cx)))
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
        .child(setting_label(label, detail, theme))
        .child(h_flex().flex_wrap().gap_2().children(chips))
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

/// Every Save stores all of the copilot's text fields at once.
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
