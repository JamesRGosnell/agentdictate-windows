#![cfg(feature = "test-support")]

//! Headless meeting copilot interaction contracts.

use super::support::DesktopHarness;

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use agentdictate_core::{
    AlertKind, AnswerStatus, ChecklistItem, CopilotAction, CopilotAlert, CopilotEvent,
    CopilotInsights, CopilotProvider, CopilotSettings, KnowledgeSummary, ListenState, QuickAction,
    RewriteStyle, Situation, Speaker, TranscriptLine,
};
use agentdictate_ui::{
    CopilotShape, CopilotViewModel, CopilotWindow, CopilotWindowHost, test_support,
};
use gpui::{
    Bounds, Entity, Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent, TestAppContext,
    VisualTestContext, WindowBounds, WindowOptions, point, prelude::*, px, size,
};
use gpui_component::Root;

struct RecordingHost {
    calls: Rc<RefCell<Vec<String>>>,
    shapes: Rc<RefCell<Vec<Vec<CopilotShape>>>>,
}

impl CopilotWindowHost for RecordingHost {
    fn resize(&self, width: f32, height: f32) {
        self.calls
            .borrow_mut()
            .push(format!("resize {width}x{height}"));
    }
    fn set_visible(&self, visible: bool) {
        self.calls.borrow_mut().push(format!("visible {visible}"));
    }
    fn begin_typing(&self) {
        self.calls.borrow_mut().push("begin typing".into());
    }
    fn end_typing(&self, restore_focus: bool) {
        self.calls
            .borrow_mut()
            .push(format!("end typing {restore_focus}"));
    }
    fn set_capture_excluded(&self, excluded: bool) {
        self.calls
            .borrow_mut()
            .push(format!("capture excluded {excluded}"));
    }
    fn set_shape(&self, shapes: &[CopilotShape]) {
        self.shapes.borrow_mut().push(shapes.to_vec());
    }
}

struct Harness {
    view: Entity<CopilotWindow>,
    actions: Arc<Mutex<Vec<CopilotAction>>>,
    host: Rc<RefCell<Vec<String>>>,
    shapes: Rc<RefCell<Vec<Vec<CopilotShape>>>>,
    cx: &'static mut VisualTestContext,
}

impl DesktopHarness for Harness {
    fn visual_context(&mut self) -> &mut VisualTestContext {
        self.cx
    }
}

impl Harness {
    /// A copilot whose user has already dismissed the first-run welcome.
    fn open(cx: &mut TestAppContext) -> Self {
        Self::open_with(
            cx,
            CopilotSettings {
                onboarded: true,
                ..CopilotSettings::default()
            },
        )
    }

    fn open_with(cx: &mut TestAppContext, settings: CopilotSettings) -> Self {
        test_support::initialize_copilot(cx);
        let actions = Arc::new(Mutex::new(Vec::new()));
        let host = Rc::new(RefCell::new(Vec::new()));
        let sink_actions = Arc::clone(&actions);
        let host_calls = Rc::clone(&host);
        let shapes = Rc::new(RefCell::new(Vec::new()));
        let host_shapes = Rc::clone(&shapes);
        let slot = Rc::new(RefCell::new(None));
        let view_slot = Rc::clone(&slot);
        let window = cx.update(|cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(0.), px(0.)),
                        size(px(720.), px(640.)),
                    ))),
                    ..Default::default()
                },
                move |window, cx| {
                    let mut model = CopilotViewModel::new(settings, 0);
                    model.listen = ListenState::Listening;
                    let view = cx.new(|cx| {
                        CopilotWindow::new(
                            model,
                            Arc::new(move |action| sink_actions.lock().unwrap().push(action)),
                            Rc::new(RecordingHost {
                                calls: host_calls,
                                shapes: host_shapes,
                            }),
                            window,
                            cx,
                        )
                    });
                    *view_slot.borrow_mut() = Some(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("headless copilot window opens")
        });
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        cx.run_until_parked();
        let view = slot.borrow_mut().take().expect("copilot view");
        Self {
            view,
            actions,
            host,
            shapes,
            cx,
        }
    }

    fn send(&mut self, event: CopilotEvent) {
        self.view
            .update_in(self.cx, |view, window, cx| view.apply(event, window, cx));
        self.cx.run_until_parked();
    }

    /// Scrolls the settings sheet until `selector` is centered.
    fn reveal_setting(&mut self, selector: &'static str) {
        let viewport = self.bounds("copilot-settings-body");
        let control = self.bounds(selector);
        self.cx
            .simulate_mouse_move(viewport.center(), None::<MouseButton>, Modifiers::none());
        self.cx.simulate_event(ScrollWheelEvent {
            position: viewport.center(),
            delta: ScrollDelta::Pixels(point(px(0.), viewport.center().y - control.center().y)),
            ..Default::default()
        });
        self.cx.run_until_parked();
    }

    fn actions(&self) -> Vec<CopilotAction> {
        self.actions.lock().unwrap().clone()
    }

    fn host_calls(&self) -> Vec<String> {
        self.host.borrow().clone()
    }

    fn last_shape(&self) -> Vec<CopilotShape> {
        self.shapes.borrow().last().cloned().unwrap_or_default()
    }
}

#[gpui::test]
fn only_the_bar_and_panel_are_painted_over_other_windows(cx: &mut TestAppContext) {
    let harness = Harness::open(cx);
    // GPUI Component's Root fills the window with the theme background; the
    // copilot window is transparent around its rounded bar and panel.
    harness.cx.update(|_, cx| {
        assert_eq!(gpui_component::Theme::global(cx).background.a, 0.0);
    });
}

#[gpui::test]
fn the_session_starts_as_the_bar_alone(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    assert!(harness.has("copilot-assist"));
    assert!(!harness.has("copilot-ask"));
    assert_eq!(harness.host_calls(), vec!["resize 720x78"]);
}

#[gpui::test]
fn clicks_beside_the_bar_and_panel_pass_through(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    // The bar alone is clickable, and it is compact.
    let bar = harness.bounds("copilot-bar");
    let shape = harness.last_shape();
    assert_eq!(shape.len(), 1);
    assert_eq!(shape[0].x, f32::from(bar.origin.x));
    assert_eq!(shape[0].width, f32::from(bar.size.width));
    assert_eq!(shape[0].height, 38.0);
    assert!(shape[0].width < 600.0, "bar is {} wide", shape[0].width);
    // Opening the panel adds it to the clickable shape; closing removes it.
    harness.click("copilot-panel");
    assert_eq!(harness.last_shape().len(), 2);
    harness.click("copilot-panel");
    assert_eq!(harness.last_shape().len(), 1);
}

#[gpui::test]
fn ask_ai_requests_assistance_and_answers_open_the_panel(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-assist");
    assert_eq!(harness.actions(), vec![CopilotAction::Assist]);
    harness.send(CopilotEvent::AnswerStarted {
        id: 1,
        title: "What should I say?".into(),
        used_screen: true,
        model: "gpt-6-sol".into(),
        automatic: false,
    });
    harness.send(CopilotEvent::AnswerDelta {
        id: 1,
        text: "Say **hello**.".into(),
    });
    harness.send(CopilotEvent::AnswerStatus {
        id: 1,
        status: AnswerStatus::Done,
    });
    assert!(harness.has("copilot-ask"));
    assert!(
        harness
            .host_calls()
            .iter()
            .any(|call| call == "resize 720x564")
    );
}

#[gpui::test]
fn quick_actions_and_detected_questions_reach_the_engine(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Pricing".into(),
        summary: vec!["Discussing seats".into()],
        question: Some("What is the annual price?".into()),
        suggestions: Vec::new(),
        situation: Situation::Unclear,
        ..CopilotInsights::default()
    }));
    assert!(!harness.has("copilot-situation"));
    // Insights open the panel once, unless the user collapsed it.
    harness.click(&*Box::leak(
        format!("copilot-quick-{}", QuickAction::Recap as usize).into_boxed_str(),
    ));
    harness.click("copilot-detected-question");
    assert_eq!(
        harness.actions(),
        vec![
            CopilotAction::Quick(QuickAction::Recap),
            CopilotAction::AnswerQuestion("What is the annual price?".into()),
        ]
    );
}

#[gpui::test]
fn the_panel_says_when_a_recording_pauses_automatic_answers(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Scaling a service business".into(),
        summary: vec!["Two hosts discuss pricing".into()],
        situation: Situation::Media,
        ..CopilotInsights::default()
    }));
    assert!(harness.has("copilot-situation"));
}

#[gpui::test]
fn ending_shows_a_wrap_up_with_the_follow_up_ready_to_copy(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Ending);
    assert!(
        !harness.has("copilot-wrap-up"),
        "notes are still being written"
    );
    harness.send(CopilotEvent::NotesSaved {
        path: "C:/notes/2026-10-13 0900 Pilot.md".into(),
        summary: Some("Agreed on a **30-day pilot**.".into()),
        follow_up: Some("Hi Sam,\n\nThanks for today.".into()),
        quiet: false,
    });
    assert!(harness.has("copilot-wrap-up"));
    assert!(harness.has("copilot-wrap-up-summary"));
    assert!(harness.has("copilot-follow-up"));
    harness.click("copilot-copy-follow-up");
    harness.click("copilot-open-notes");
    harness.click("copilot-done");
    assert_eq!(
        harness.actions(),
        vec![
            CopilotAction::CopyText("Hi Sam,\n\nThanks for today.".into()),
            CopilotAction::OpenNotes,
            CopilotAction::End,
        ]
    );
}

#[gpui::test]
fn the_wrap_up_drafts_the_follow_up_email(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Ending);
    harness.send(CopilotEvent::NotesSaved {
        path: "C:/notes/meeting.md".into(),
        summary: Some("Agreed on a pilot.".into()),
        follow_up: Some("Subject: Pilot\n\nHi Sam,".into()),
        quiet: false,
    });
    harness.click("copilot-draft-email");
    harness.click("copilot-copy-follow-up");
    assert_eq!(
        harness.actions(),
        vec![
            CopilotAction::DraftEmail,
            CopilotAction::CopyText("Subject: Pilot\n\nHi Sam,".into()),
        ]
    );
}

#[gpui::test]
fn settings_show_the_knowledge_and_what_cannot_be_read(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-settings");
    assert!(!harness.has("copilot-knowledge-unreadable"));
    harness.send(CopilotEvent::Knowledge(KnowledgeSummary {
        documents: 2,
        meetings: 5,
        words: 4_200,
        unreadable: vec!["deck.pdf".into()],
    }));
    assert!(harness.has("copilot-knowledge-status"));
    assert!(harness.has("copilot-knowledge-unreadable"));
    harness.reveal_setting("copilot-open-knowledge");
    harness.click("copilot-open-knowledge");
    assert_eq!(harness.actions(), vec![CopilotAction::OpenKnowledgeFolder]);
}

#[gpui::test]
fn the_brain_selector_saves_each_source_and_only_requests_leadlord_setup_when_selected(
    cx: &mut TestAppContext,
) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-settings");
    harness.reveal_setting("copilot-brain-1");
    harness.click("copilot-brain-1");
    assert!(
        matches!(harness.actions().last(), Some(CopilotAction::UpdateSettings(settings)) if settings.brain_source == agentdictate_core::BrainSource::PersonalHq)
    );
    assert!(!harness.has("copilot-leadlord-cli"));
    assert!(!harness.has("copilot-leadlord-token"));
    harness.reveal_setting("copilot-brain-2");
    harness.click("copilot-brain-2");
    assert!(
        matches!(harness.actions().last(), Some(CopilotAction::UpdateSettings(settings)) if settings.brain_source == agentdictate_core::BrainSource::Leadlord)
    );
    harness.reveal_setting("copilot-leadlord-cli");
    assert!(harness.has("copilot-leadlord-cli"));
    harness.reveal_setting("copilot-leadlord-token");
    harness.click("copilot-leadlord-token");
    harness.cx.simulate_input("synthetic-company-token");
    harness.reveal_setting("copilot-save-settings");
    harness.click("copilot-save-settings");
    assert!(
        matches!(harness.actions().last(), Some(CopilotAction::UpdateSettings(settings)) if settings.leadlord_bws_access_token == "synthetic-company-token")
    );
}

#[gpui::test]
fn an_empty_copilot_leaves_context_setup_in_settings(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-panel");
    assert!(harness.has("copilot-live-body"));
    assert!(!harness.has("copilot-teach"));
}

#[gpui::test]
fn a_copilot_that_knows_the_user_does_not_ask_to_be_taught(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Knowledge(KnowledgeSummary {
        documents: 1,
        ..KnowledgeSummary::default()
    }));
    harness.click("copilot-panel");
    assert!(harness.has("copilot-live-body"));
    assert!(!harness.has("copilot-teach"));
}

/// Streams an answer with an instant draft and a researched follow-up.
fn stream_answer(harness: &mut Harness, researched: &str) {
    harness.send(CopilotEvent::AnswerStarted {
        id: 7,
        title: "What does it cost?".into(),
        used_screen: false,
        model: "gpt-6-sol".into(),
        automatic: true,
    });
    harness.send(CopilotEvent::InstantDelta {
        id: 7,
        text: "\u{201c}It's $49 per seat on the growth plan.\u{201d}".into(),
    });
    harness.send(CopilotEvent::InstantFinished { id: 7 });
    harness.send(CopilotEvent::AnswerDelta {
        id: 7,
        text: researched.into(),
    });
    harness.send(CopilotEvent::AnswerStatus {
        id: 7,
        status: AnswerStatus::Done,
    });
}

#[gpui::test]
fn the_researched_answer_adds_to_the_line_to_say(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    stream_answer(&mut harness, "- Annual billing saves 20%");
    assert!(harness.has("copilot-answer-hero"));
    assert!(harness.has("copilot-answer-more"));
    assert!(!harness.has("copilot-answer-correction"));
}

#[gpui::test]
fn a_wrong_draft_is_corrected_in_place(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    stream_answer(
        &mut harness,
        "**Correction:** It's $59 per seat since October.",
    );
    assert!(harness.has("copilot-answer-hero"));
    assert!(harness.has("copilot-answer-correction"));
    assert!(!harness.has("copilot-answer-more"));
}

#[gpui::test]
fn a_complete_draft_is_marked_as_checked(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    stream_answer(&mut harness, "Nothing to add.");
    assert!(harness.has("copilot-answer-confirmed"));
    assert!(!harness.has("copilot-answer-more"));
}

#[gpui::test]
fn the_line_to_say_is_rewritten_in_one_click_and_versions_compared(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    stream_answer(&mut harness, "Nothing to add.");
    harness.click("copilot-rewrite-0");
    assert_eq!(
        harness.actions(),
        vec![CopilotAction::Rewrite {
            id: 7,
            style: RewriteStyle::Shorter,
            text: "\u{201c}It's $49 per seat on the growth plan.\u{201d}".into(),
        }]
    );
    assert!(harness.has("copilot-rewriting"));
    harness.send(CopilotEvent::RewriteDelta {
        id: 7,
        text: "$49 a seat.".into(),
    });
    harness.send(CopilotEvent::RewriteFinished { id: 7, error: None });
    assert!(harness.has("copilot-version-label"));

    // Earlier versions stay one click away, and the engine keeps the choice.
    harness.click("copilot-version-previous");
    harness.click("copilot-version-next");
    assert_eq!(
        harness.actions()[1..],
        [
            CopilotAction::KeepVersion { id: 7, text: None },
            CopilotAction::KeepVersion {
                id: 7,
                text: Some("$49 a seat.".into()),
            },
        ]
    );
    // A rewrite of the rewrite starts from what is shown.
    harness.click("copilot-rewrite-1");
    assert_eq!(
        harness.actions()[3],
        CopilotAction::Rewrite {
            id: 7,
            style: RewriteStyle::Casual,
            text: "$49 a seat.".into(),
        }
    );
    harness.send(CopilotEvent::RewriteFinished {
        id: 7,
        error: Some("Rewrite stopped".into()),
    });
    // A failed rewrite leaves the earlier line in place, ready again.
    harness.click("copilot-rewrite-2");
    assert!(matches!(
        &harness.actions()[4],
        CopilotAction::Rewrite { style: RewriteStyle::Specific, text, .. } if text == "$49 a seat."
    ));
}

#[gpui::test]
fn the_first_run_hint_is_dismissible_without_context_setup(cx: &mut TestAppContext) {
    let mut harness = Harness::open_with(cx, CopilotSettings::default());
    harness.click("copilot-panel");
    assert!(harness.has("copilot-welcome"));
    assert_eq!(harness.host_calls().last().unwrap(), "resize 720x304");
    assert!(!harness.has("copilot-welcome-knowledge"));
    assert!(!harness.has("copilot-welcome-about"));
    harness.click("copilot-welcome-done");
    let actions = harness.actions();
    assert_eq!(actions.len(), 1);
    assert!(matches!(
        &actions[0],
        CopilotAction::UpdateSettings(settings) if settings.onboarded
    ));
}

#[gpui::test]
fn the_small_first_run_panel_grows_when_live_content_arrives(cx: &mut TestAppContext) {
    let mut harness = Harness::open_with(cx, CopilotSettings::default());
    harness.click("copilot-panel");
    assert_eq!(harness.host_calls().last().unwrap(), "resize 720x304");
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Trial options".into(),
        ..CopilotInsights::default()
    }));
    let height = last_resize_height(&harness);
    assert!(height > 304.0 && height < FULL_HEIGHT, "{height}");
    // The panel resizes over several frames, and headless debug bounds can
    // outlive the frame that drew them, so check what replaced the welcome.
    assert!(harness.has("copilot-insights"));
}

/// The window height with the panel at its full size.
const FULL_HEIGHT: f32 = 564.0;

fn last_resize_height(harness: &Harness) -> f32 {
    harness
        .host_calls()
        .iter()
        .rev()
        .find_map(|call| call.strip_prefix("resize 720x"))
        .and_then(|height| height.parse().ok())
        .expect("the window was resized")
}

#[gpui::test]
fn the_live_panel_fits_its_content_up_to_full_height(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Designing effective agent loops".into(),
        summary: vec!["Agent loops need fast, accurate feedback.".into()],
        ..CopilotInsights::default()
    }));
    let brief = last_resize_height(&harness);
    assert!(brief < FULL_HEIGHT - 100.0, "{brief}");
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Designing effective agent loops".into(),
        summary: (0..40)
            .map(|index| format!("Point {index} about the loop."))
            .collect(),
        ..CopilotInsights::default()
    }));
    assert_eq!(last_resize_height(&harness), FULL_HEIGHT);
}

#[gpui::test]
fn an_answer_takes_the_full_panel_from_its_first_word(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Pricing".into(),
        ..CopilotInsights::default()
    }));
    assert!(last_resize_height(&harness) < FULL_HEIGHT);
    harness.send(CopilotEvent::AnswerStarted {
        id: 1,
        title: "What does it cost?".into(),
        used_screen: false,
        model: "gpt-6-luna".into(),
        automatic: true,
    });
    assert_eq!(last_resize_height(&harness), FULL_HEIGHT);
}

fn them(id: u64, voice: u8, text: &str, at_ms: u64) -> TranscriptLine {
    TranscriptLine {
        id,
        speaker: Speaker::Them,
        text: text.into(),
        at_ms,
        voice: Some(voice),
    }
}

#[gpui::test]
fn the_other_sides_people_are_named_by_click_or_from_the_screen(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Transcript(them(
        1,
        0,
        "Thanks for joining.",
        0,
    )));
    harness.send(CopilotEvent::Transcript(them(
        2,
        1,
        "Glad to be here.",
        2_000,
    )));
    harness.send(CopilotEvent::Speakers {
        names: [(0, "Sam Lee".to_owned())].into(),
        seen: vec!["Sam Lee".into(), "Joey Tran".into()],
    });
    harness
        .view
        .update(harness.cx, |view, cx| view.show_transcript(cx));
    harness.cx.run_until_parked();
    assert!(harness.has("copilot-person-0") && harness.has("copilot-person-1"));

    // Clicking Them B's line offers the names on screen nobody has yet.
    harness.click("copilot-speaker-2");
    assert!(
        harness
            .host_calls()
            .iter()
            .any(|call| call == "begin typing")
    );
    harness.click("copilot-name-suggestion-0");
    assert_eq!(
        harness.actions(),
        vec![CopilotAction::NameSpeaker {
            voice: 1,
            name: "Joey Tran".into(),
        }]
    );

    // A name can be forgotten and typed instead.
    harness.click("copilot-person-0");
    harness.click("copilot-name-clear");
    harness.click("copilot-person-0");
    harness.cx.simulate_input("Samuel");
    harness.cx.simulate_keystrokes("enter");
    harness.cx.run_until_parked();
    assert_eq!(
        harness.actions()[1..],
        [
            CopilotAction::NameSpeaker {
                voice: 0,
                name: String::new(),
            },
            CopilotAction::NameSpeaker {
                voice: 0,
                name: "Samuel".into(),
            },
        ]
    );
    assert!(
        harness
            .host_calls()
            .iter()
            .any(|call| call == "end typing true")
    );
}

#[gpui::test]
fn typing_a_question_activates_the_window_and_returns_focus(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::FocusAsk);
    assert!(
        harness
            .host_calls()
            .iter()
            .any(|call| call == "begin typing")
    );
    harness.cx.simulate_input("Who founded Anthropic?");
    harness.cx.simulate_keystrokes("enter");
    harness.cx.run_until_parked();
    assert_eq!(
        harness.actions(),
        vec![CopilotAction::Ask("Who founded Anthropic?".into())]
    );
    assert!(
        harness
            .host_calls()
            .iter()
            .any(|call| call == "end typing true")
    );
}

#[gpui::test]
fn hide_and_the_toggle_shortcut_change_window_visibility(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-hide");
    harness.send(CopilotEvent::ToggleVisibility);
    let calls = harness.host_calls();
    assert!(calls.iter().any(|call| call == "visible false"));
    assert!(calls.iter().any(|call| call == "visible true"));
}

#[gpui::test]
fn settings_toggles_and_provider_choices_are_saved(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-settings");
    harness.reveal_setting("copilot-web");
    harness.click("copilot-web");
    harness.reveal_setting("copilot-provider-0");
    harness.click(&*Box::leak(
        format!("copilot-provider-{}", CopilotProvider::ClaudeCode as usize).into_boxed_str(),
    ));
    let saved: Vec<CopilotSettings> = harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            CopilotAction::UpdateSettings(settings) => Some(*settings),
            _ => None,
        })
        .collect();
    assert_eq!(saved.len(), 2);
    assert!(!saved[0].web_search);
    assert_eq!(saved[1].provider, CopilotProvider::ClaudeCode);
    assert!(!saved[1].web_search);
}

#[gpui::test]
fn rewrites_can_go_through_claude(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-settings");
    harness.reveal_setting("copilot-rewrite-engine-1");
    harness.click("copilot-rewrite-engine-1");
    assert!(matches!(
        harness.actions().last(),
        Some(CopilotAction::UpdateSettings(settings))
            if settings.rewrite_engine == agentdictate_core::RewriteEngine::ClaudeSonnet
    ));
}

#[gpui::test]
fn a_transcription_problem_shows_on_the_bar(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    assert!(!harness.has("copilot-problem"));
    harness.send(CopilotEvent::Problem(Some(
        "ChatGPT transcription limit reached. Try again later.".into(),
    )));
    assert!(harness.has("copilot-problem"));
}

#[gpui::test]
fn ending_the_session_takes_a_confirming_second_click(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-end");
    assert!(harness.actions().is_empty());
    harness.click("copilot-end");
    assert_eq!(harness.actions(), vec![CopilotAction::End]);
    // The bar goes at once rather than waiting for the notes.
    assert!(
        harness
            .host_calls()
            .iter()
            .any(|call| call == "visible false")
    );
}

#[gpui::test]
fn turning_off_screen_share_hiding_updates_the_host(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-settings");
    harness.reveal_setting("copilot-hidden");
    harness.click("copilot-hidden");
    assert!(
        harness
            .host_calls()
            .iter()
            .any(|call| call == "capture excluded false")
    );
}

#[gpui::test]
fn words_heard_so_far_show_until_the_final_line_replaces_them(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness
        .view
        .update(harness.cx, |view, cx| view.show_transcript(cx));
    harness.send(CopilotEvent::PartialTranscript {
        speaker: Speaker::Them,
        text: "So what does pricing look".into(),
    });
    assert!(harness.has("copilot-partial-them"));
    assert!(!harness.has("copilot-partial-me"));
    harness.send(CopilotEvent::Transcript(them(
        1,
        0,
        "So what does pricing look like?",
        0,
    )));
    assert!(!harness.has("copilot-partial-them"));
}

#[gpui::test]
fn a_meeting_ended_by_hand_says_its_notes_are_ready_on_the_bar_alone(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::NotesSaved {
        path: "C:/notes/meeting.md".into(),
        summary: Some("Agreed on a pilot.".into()),
        follow_up: Some("Subject: Pilot\n\nHi Sam,".into()),
        quiet: true,
    });
    assert!(harness.has("copilot-notes-ready"));
    assert!(!harness.has("copilot-wrap-up"), "nothing covers the screen");
    harness.click("copilot-notes-ready-email");
    harness.click("copilot-notes-ready-open");
    // Headless debug bounds can outlive their frame, so check what
    // replaced the bar instead of its absence.
    assert!(harness.has("copilot-wrap-up"));
    assert!(harness.has("copilot-bar"));
    assert_eq!(
        harness.actions(),
        vec![CopilotAction::DraftEmail, CopilotAction::KeepOpen]
    );
}

#[gpui::test]
fn compact_mode_shows_only_the_line_to_say(cx: &mut TestAppContext) {
    let mut harness = Harness::open_with(
        cx,
        CopilotSettings {
            onboarded: true,
            compact: true,
            ..CopilotSettings::default()
        },
    );
    // Insights alone do not open the compact panel.
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Pricing".into(),
        summary: vec!["Discussing seats".into()],
        ..CopilotInsights::default()
    }));
    assert!(!harness.has("copilot-compact"));
    harness.send(CopilotEvent::AnswerStarted {
        id: 7,
        title: "What does it cost?".into(),
        used_screen: false,
        model: "gpt-6-sol".into(),
        automatic: true,
    });
    harness.send(CopilotEvent::InstantDelta {
        id: 7,
        text: "It's $49 per seat.\n[from: mine]".into(),
    });
    assert!(harness.has("copilot-compact"));
    assert!(!harness.has("copilot-ask"), "no footer in compact mode");
    assert!(!harness.has("copilot-insights"));
    harness.click("copilot-compact-more");
    assert!(harness.has("copilot-ask"));
    assert!(harness.has("copilot-answer-grounding"));
    assert!(matches!(
        harness.actions().last(),
        Some(CopilotAction::UpdateSettings(settings)) if !settings.compact
    ));
}

#[gpui::test]
fn hearing_your_name_raises_an_alert_you_can_answer_from(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Alert(CopilotAlert {
        kind: AlertKind::Mention,
        text: "James, can you speak to the renewal numbers?".into(),
        context: "Them: Churn went up two points.".into(),
    }));
    assert!(harness.has("copilot-alert"));
    harness.click("copilot-alert-answer");
    assert_eq!(
        harness.actions(),
        vec![CopilotAction::AnswerQuestion(
            "James, can you speak to the renewal numbers?".into()
        )]
    );
    // A contradiction has nothing to answer, only to dismiss.
    harness.send(CopilotEvent::Alert(CopilotAlert {
        kind: AlertKind::Contradiction,
        text: "You said $39; your pricing sheet says $49.".into(),
        context: String::new(),
    }));
    assert!(harness.has("copilot-alert"));
    harness.click("copilot-alert-dismiss");
    assert_eq!(harness.actions().len(), 1, "dismissing asks for nothing");
}

#[gpui::test]
fn the_live_panel_shows_the_checklist_the_brief_and_promises(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.send(CopilotEvent::Checklist(vec![
        ChecklistItem {
            label: "Budget".into(),
            done: true,
        },
        ChecklistItem {
            label: "Next step agreed".into(),
            done: false,
        },
    ]));
    harness.send(CopilotEvent::Brief(
        "**Last time with Sam Lee** \u{00b7} 2026-11-03 \u{00b7} Pricing review".into(),
    ));
    harness.send(CopilotEvent::Promises(vec![
        "Send the pricing sheet by Friday".into(),
    ]));
    harness.send(CopilotEvent::Insights(CopilotInsights {
        topic: "Pricing".into(),
        summary: vec!["Discussing seats".into()],
        ..CopilotInsights::default()
    }));
    assert!(harness.has("copilot-checklist"));
    assert!(harness.has("copilot-brief"));
    assert!(harness.has("copilot-promises"));
    harness.click("copilot-brief-close");
}

#[gpui::test]
fn typed_notes_and_translations_show_in_the_transcript(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness
        .view
        .update(harness.cx, |view, cx| view.show_transcript(cx));
    harness.send(CopilotEvent::Transcript(them(
        1,
        0,
        "Bonjour, comment allez-vous",
        1_000,
    )));
    assert!(!harness.has("copilot-translation"));
    harness.send(CopilotEvent::Translation {
        line: 1,
        text: "Hello, how are you?".into(),
    });
    harness.send(CopilotEvent::Note {
        at_ms: 4_000,
        text: "they renew in March".into(),
    });
    assert!(harness.has("copilot-translation"));
    assert!(harness.has("copilot-note"));
}
