//! The Meeting Copilot section of the main window's Settings, so the copilot
//! can be set up without starting a meeting. Choices save at once; text
//! saves with its button. A running meeting keeps its settings until the
//! next one starts.

use std::{cell::RefCell, sync::Arc};

use agentdictate_core::{CopilotMode, CopilotProvider, CopilotSettings, ModelChoice};
use gpui::{AppContext, Context, Entity, Window, prelude::*, px};
use gpui_component::{
    Selectable, Sizable,
    button::ButtonVariants,
    h_flex,
    input::{Input, InputState},
    v_flex,
};

use crate::ThemeTokens;
use crate::action::action_button;

use super::{SettingsShell, enabled_label, gpui_color};

type LoadCopilotSettings = dyn Fn() -> CopilotSettings + Send + Sync;
type SaveCopilotSettings = dyn Fn(&CopilotSettings) -> Result<(), String> + Send + Sync;

/// Reads and writes the copilot's settings file for the main window.
#[derive(Clone)]
pub struct CopilotSettingsStore {
    pub load: Arc<LoadCopilotSettings>,
    pub save: Arc<SaveCopilotSettings>,
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

const SAVED: &str = "Saved. Applies when your next meeting starts.";

#[derive(Default)]
pub(super) struct CopilotSettingsState {
    settings: Option<CopilotSettings>,
    openai_key: Option<Entity<InputState>>,
    anthropic_key: Option<Entity<InputState>>,
    profile: Option<Entity<InputState>>,
    instructions: Option<Entity<InputState>>,
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
        Self {
            settings: Some(settings),
            openai_key: Some(openai_key),
            anthropic_key: Some(anthropic_key),
            profile: Some(profile),
            instructions: Some(instructions),
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
        self.update_copilot(cx, |settings| {
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
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    let CopilotSectionModel {
        settings,
        openai_key,
        anthropic_key,
        profile,
        instructions,
        feedback,
    } = model;
    let provider = settings.provider;
    let key = match provider {
        CopilotProvider::OpenaiApi => Some(("OpenAI API key", openai_key)),
        CopilotProvider::AnthropicApi => Some(("Anthropic API key", anthropic_key)),
        CopilotProvider::ChatgptSubscription | CopilotProvider::ClaudeCode => None,
    };
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

    let toggles: [(&'static str, &'static str, bool, fn(&mut CopilotSettings)); 8] = [
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
    if let Some((label, input)) = key {
        column = column.child(
            row(theme)
                .debug_selector(|| "copilot-settings-key".to_owned())
                .child(label_block(label, "Stored on this computer", theme))
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
                        .child(save_button("copilot-settings-save-key", cx)),
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
            "Meeting type",
            "Shapes how answers are written",
            mode_chips,
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
    column
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
