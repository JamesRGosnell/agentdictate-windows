//! The Meeting Copilot section of the main window's Settings, so the copilot
//! can be set up without starting a meeting. It shows the seven settings
//! most people change, with the rest behind a collapsed "Advanced" group.
//! Choices save at once; text saves with any Save button. A running meeting
//! keeps its settings until the next one starts.

use std::{cell::RefCell, sync::Arc};

use agentdictate_core::{
    BrainSource, CopilotEffort, CopilotMode, CopilotProvider, CopilotSettings, CopilotShortcuts,
    LANGUAGES, ModelChoice, RewriteEngine, model_pace,
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

use crate::action::action_button;
use crate::{CopilotOpens, ThemeTokens};

use super::{
    SettingsShell,
    copilot_view::shortcut_text,
    enabled_label, gpui_color,
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
    leadlord_cli: Option<Entity<InputState>>,
    leadlord_token: Option<Entity<InputState>>,
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
        let leadlord_cli = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Path to Leadlord's hq-brain-index.ts")
                .default_value(settings.leadlord_brain_cli.clone())
        });
        let leadlord_token = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Bitwarden Secrets Manager access token")
                .masked(true)
                .default_value(settings.leadlord_bws_access_token.clone())
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
            leadlord_cli: Some(leadlord_cli),
            leadlord_token: Some(leadlord_token),
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
            leadlord_cli: self.leadlord_cli.clone()?,
            leadlord_token: self.leadlord_token.clone()?,
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
    leadlord_cli: Entity<InputState>,
    leadlord_token: Entity<InputState>,
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
        let leadlord_cli = read(&self.copilot.leadlord_cli, cx);
        let leadlord_token = read(&self.copilot.leadlord_token, cx);
        let user_name = read(&self.copilot.user_name, cx);
        let calendar = read(&self.copilot.calendar, cx);
        let webhook = read(&self.copilot.webhook, cx);
        self.update_copilot(cx, |settings| {
            if let Some(cli) = leadlord_cli {
                settings.leadlord_brain_cli = cli;
            }
            if let Some(token) = leadlord_token {
                settings.leadlord_bws_access_token = token;
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

/// A model's chip label with its measured pace, like "GPT-6 Luna · Fastest".
fn model_chip_label(model: &ModelChoice) -> String {
    match model_pace(&model.id) {
        Some(pace) => format!("{} \u{00b7} {}", model.label, pace.label()),
        None => model.label.clone(),
    }
}

/// Builds the copilot's settings: the seven things most people change, then
/// everything else behind a collapsed "Advanced" group in four parts.
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
        leadlord_cli,
        leadlord_token,
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
    // Only the key the chosen engine needs, plus the OpenAI key, which also
    // transcribes the call whatever engine answers.
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
                model_chip_label(&choice),
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
                model_chip_label(&choice),
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
    let opens = CopilotOpens::from_settings(&settings);
    let opens_chips = CopilotOpens::ALL
        .into_iter()
        .map(|choice| {
            chip(
                format!("copilot-settings-opens-{}", choice.slug()),
                choice.label(),
                choice == opens,
                cx,
                move |settings| choice.apply(settings),
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
    // new switches go at the end. Switches no longer offered keep their
    // place, unshown, so the others keep their numbers: answering
    // automatically (0) is "When to answer"; the dictation shortcut (4) and
    // opening when a meeting starts (8) are "Opens"; the microphone (6) is
    // "Don't transcribe my side" (13); system audio (7) is always on; and
    // the compact panel (9) is switched from the bar.
    let toggles: [Option<Toggle>; 14] = [
        None,
        Some((
            "Instant answers",
            "A quick draft shows while the full answer is written",
            settings.instant_answers,
            |s| s.instant_answers = !s.instant_answers,
        )),
        Some((
            "Search the web",
            "Full answers can look things up",
            settings.web_search,
            |s| s.web_search = !s.web_search,
        )),
        Some((
            "Save meeting notes",
            "Notes and a follow-up email go to the Meetings page",
            settings.save_notes,
            |s| s.save_notes = !s.save_notes,
        )),
        None,
        Some((
            "Hide from screen sharing",
            "Others never see the copilot when you share your screen",
            settings.hide_from_screen_share,
            |s| s.hide_from_screen_share = !s.hide_from_screen_share,
        )),
        None,
        None,
        None,
        None,
        Some((
            "Live insights",
            "The topic, a checklist, and what you promised; also pauses automatic \
             answers during a presentation or video",
            settings.live_insights,
            |s| s.live_insights = !s.live_insights,
        )),
        Some((
            "Use my screen",
            "Typed questions include a screenshot, without the copilot in it",
            settings.screen_context,
            |s| s.screen_context = !s.screen_context,
        )),
        Some((
            "Remember past meetings",
            "Answers and the pre-call brief use your earlier meetings",
            settings.use_past_meetings,
            |s| s.use_past_meetings = !s.use_past_meetings,
        )),
        Some((
            "Don't transcribe my side",
            "Only the other side is heard; answers no longer see what you said",
            !settings.capture_microphone,
            |s| s.capture_microphone = !s.capture_microphone,
        )),
    ];
    let mut index = 0;
    let [
        _auto_answer,
        instant_answers,
        web_search,
        save_notes,
        _dictation_shortcut,
        hide_from_sharing,
        _microphone,
        _system_audio,
        _auto_start,
        _compact,
        live_insights,
        screen,
        past_meetings,
        mute_my_side,
    ] = toggles.map(|toggle| {
        let row = toggle.map(|toggle| toggle_row(index, toggle, theme, cx));
        index += 1;
        row
    });

    // The one choice that changes the copilot most, first and on its own.
    let ask_keys = shortcut_text(&CopilotShortcuts::default().assist);
    let answer_mode = settings_section(
        "settings-group-copilot-mode",
        "Meeting Copilot",
        "Answers questions during your calls. Changes apply when your next meeting starts.",
        false,
        theme,
    )
    .child(
        gpui::div()
            .pb_2()
            .text_sm()
            .font_weight(gpui::FontWeight::MEDIUM)
            .child("When to answer"),
    )
    .child(
        h_flex()
            .w_full()
            .flex_wrap()
            .gap_3()
            .child(answer_mode_card(
                "copilot-settings-answer-mode-auto",
                "Automatically",
                "Answers appear by themselves when the other side asks you something.".to_owned(),
                settings.auto_answer,
                true,
                theme,
                cx,
            ))
            .child(answer_mode_card(
                "copilot-settings-answer-mode-ask",
                "When I ask",
                format!(
                    "Answers only when you press {ask_keys} or Ask AI. Fastest, \
                     since nothing waits for them to stop talking."
                ),
                !settings.auto_answer,
                false,
                theme,
                cx,
            )),
    );

    let engine = settings_section(
        "settings-group-copilot-engine",
        "Answer engine",
        "Which AI writes the answers, and the key it uses.",
        false,
        theme,
    )
    .child(chip_row("Engine", provider.detail(), provider_chips, theme))
    .children(key_rows);

    let meetings = settings_section(
        "settings-group-copilot-meetings",
        "Your meetings",
        "What kind of calls, how the copilot opens, and what it keeps.",
        true,
        theme,
    )
    .child(chip_row(
        "Meeting type",
        "Shapes answers, the checklist, and the notes",
        mode_chips,
        theme,
    ))
    .child(chip_row(
        "Opens",
        "When the copilot appears for a call",
        opens_chips,
        theme,
    ))
    .children(save_notes);

    let about = settings_section(
        "settings-group-copilot-about",
        "You",
        "Helps answers speak for you and know your work.",
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
    ));

    let advanced_body = advanced_open.then(|| {
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
        let answers = advanced_group("settings-group-copilot-advanced-answers", "Answers", theme)
            .children(instant_answers)
            .children(web_search)
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
            ))
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
            .child(text_area_block(
                "Custom instructions",
                "Applied to every answer",
                &instructions,
                "copilot-settings-save-instructions",
                theme,
                cx,
            ));
        let audio = advanced_group(
            "settings-group-copilot-advanced-audio",
            "Audio and language",
            theme,
        )
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
        .children(mute_my_side);
        let privacy = advanced_group(
            "settings-group-copilot-advanced-privacy",
            "Privacy and screen",
            theme,
        )
        .children(hide_from_sharing)
        .children(screen)
        .children(live_insights)
        .children(past_meetings);
        let leadlord = settings.brain_source == BrainSource::Leadlord;
        let connections = advanced_group(
            "settings-group-copilot-advanced-connections",
            "Connections",
            theme,
        )
        .child(input_save_row(
            "copilot-settings-assemblyai-key",
            "AssemblyAI API key",
            "Optional. Transcribes calls faster than OpenAI.",
            &assemblyai_key,
            "copilot-settings-save-assemblyai-key",
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
        .child(chip_row(
            "Company knowledge",
            "Used alongside your knowledge folder",
            brain_chips,
            theme,
        ))
        .when(leadlord, |group| {
            group
                .child(input_save_row(
                    "copilot-settings-leadlord-cli",
                    "Leadlord brain",
                    "Path to Leadlord's hq-brain-index.ts",
                    &leadlord_cli,
                    "copilot-settings-save-leadlord-cli",
                    theme,
                    cx,
                ))
                .child(input_save_row(
                    "copilot-settings-leadlord-token",
                    "Bitwarden access token",
                    "For the Leadlord brain. Stored on this computer.",
                    &leadlord_token,
                    "copilot-settings-save-leadlord-token",
                    theme,
                    cx,
                ))
        });
        let documents = v_flex()
            .w_full()
            .gap_2()
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
            .when_some(asked, |column, answer| column.child(answer));
        v_flex()
            .w_full()
            .gap_4()
            .child(answers)
            .child(audio)
            .child(privacy)
            .child(connections)
            .child(documents)
    });

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
                                    "Answers, audio and language, privacy and screen, and \
                                     connections. The defaults suit most people.",
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
        .children(advanced_body);

    v_flex()
        .w_full()
        .gap_6()
        .debug_selector(|| "settings-group-copilot".to_owned())
        .child(answer_mode)
        .child(engine)
        .child(meetings)
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

/// One labelled part of the "Advanced" group.
fn advanced_group(selector: &'static str, title: &'static str, theme: ThemeTokens) -> gpui::Div {
    v_flex()
        .w_full()
        .debug_selector(move || selector.to_owned())
        .child(
            gpui::div()
                .pb_1()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(gpui_color(theme.text_muted))
                .child(title),
        )
}

/// One of the two "When to answer" choices: a large card with a radio mark,
/// outlined in the accent colour while chosen. The chosen card's mark is
/// `{selector}-selected`.
fn answer_mode_card(
    selector: &'static str,
    title: &'static str,
    detail: String,
    selected: bool,
    automatic: bool,
    theme: ThemeTokens,
    cx: &mut Context<SettingsShell>,
) -> gpui::Stateful<gpui::Div> {
    let mark = gpui::div()
        .mt(px(2.))
        .size(px(16.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded_full()
        .border_2()
        .border_color(gpui_color(if selected {
            theme.accent
        } else {
            theme.text_muted
        }))
        .when(selected, |mark| {
            mark.child(
                gpui::div()
                    .debug_selector(move || format!("{selector}-selected"))
                    .size(px(8.))
                    .rounded_full()
                    .bg(gpui_color(theme.accent)),
            )
        });
    h_flex()
        .id(selector)
        .debug_selector(move || selector.to_owned())
        .min_w(px(240.))
        .flex_1()
        .items_start()
        .gap_3()
        .p_4()
        .rounded_lg()
        .border_2()
        .border_color(gpui_color(if selected {
            theme.accent
        } else {
            theme.border
        }))
        .bg(gpui_color(if selected {
            theme.surface_hovered
        } else {
            theme.surface
        }))
        .cursor_pointer()
        .hover(|card| card.bg(gpui_color(theme.surface_hovered)))
        .child(mark)
        .child(
            v_flex()
                .min_w_0()
                .flex_1()
                .gap_1()
                .child(
                    gpui::div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(gpui_color(theme.text_muted))
                        .child(detail),
                ),
        )
        .on_click(cx.listener(move |shell, _, _, cx| {
            shell.update_copilot(cx, |settings| settings.auto_answer = automatic);
        }))
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
