#![cfg(feature = "test-support")]

//! Headless meeting copilot interaction contracts.

use super::support::DesktopHarness;

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

use agentdictate_core::{
    AnswerStatus, CopilotAction, CopilotEvent, CopilotInsights, CopilotProvider, CopilotSettings,
    KnowledgeSummary, ListenState, QuickAction, Situation, Speaker, TranscriptLine,
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
fn an_untaught_copilot_invites_the_user_to_add_their_context(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-panel");
    assert!(harness.has("copilot-teach"));
    harness.click("copilot-teach");
    assert!(harness.has("copilot-settings-body"));
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
fn the_first_run_welcome_explains_the_copilot_and_is_dismissed_once(cx: &mut TestAppContext) {
    let mut harness = Harness::open_with(cx, CopilotSettings::default());
    harness.click("copilot-panel");
    assert!(harness.has("copilot-welcome"));
    harness.click("copilot-welcome-knowledge");
    harness.click("copilot-welcome-done");
    let actions = harness.actions();
    assert_eq!(actions[0], CopilotAction::OpenKnowledgeFolder);
    assert!(matches!(
        &actions[1],
        CopilotAction::UpdateSettings(settings) if settings.onboarded
    ));
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
fn ending_the_session_is_one_click(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    harness.click("copilot-end");
    assert_eq!(harness.actions(), vec![CopilotAction::End]);
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
