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
    ListenState, QuickAction,
};
use agentdictate_ui::{CopilotViewModel, CopilotWindow, CopilotWindowHost, test_support};
use gpui::{
    Bounds, Entity, Modifiers, MouseButton, ScrollDelta, ScrollWheelEvent, TestAppContext,
    VisualTestContext, WindowBounds, WindowOptions, point, prelude::*, px, size,
};
use gpui_component::Root;

struct RecordingHost {
    calls: Rc<RefCell<Vec<String>>>,
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
}

struct Harness {
    view: Entity<CopilotWindow>,
    actions: Arc<Mutex<Vec<CopilotAction>>>,
    host: Rc<RefCell<Vec<String>>>,
    cx: &'static mut VisualTestContext,
}

impl DesktopHarness for Harness {
    fn visual_context(&mut self) -> &mut VisualTestContext {
        self.cx
    }
}

impl Harness {
    fn open(cx: &mut TestAppContext) -> Self {
        test_support::initialize(cx);
        let actions = Arc::new(Mutex::new(Vec::new()));
        let host = Rc::new(RefCell::new(Vec::new()));
        let sink_actions = Arc::clone(&actions);
        let host_calls = Rc::clone(&host);
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
                    let mut model = CopilotViewModel::new(CopilotSettings::default(), 0);
                    model.listen = ListenState::Listening;
                    let view = cx.new(|cx| {
                        CopilotWindow::new(
                            model,
                            Arc::new(move |action| sink_actions.lock().unwrap().push(action)),
                            Rc::new(RecordingHost { calls: host_calls }),
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
}

#[gpui::test]
fn the_session_starts_as_the_bar_alone(cx: &mut TestAppContext) {
    let mut harness = Harness::open(cx);
    assert!(harness.has("copilot-assist"));
    assert!(!harness.has("copilot-ask"));
    assert_eq!(harness.host_calls(), vec!["resize 720x88"]);
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
            .any(|call| call == "resize 720x634")
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
    }));
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
