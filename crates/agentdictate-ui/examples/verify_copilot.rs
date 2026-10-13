//! Native copilot rendering probe. Run only through test-windows-copilot.ps1,
//! which places the window on an isolated, inactive Windows desktop and
//! captures it for inspection.
#[cfg(all(windows, feature = "desktop"))]
fn main() {
    use agentdictate_core::{
        AnswerSource, AnswerStatus, CopilotEvent, CopilotInsights, CopilotSettings, ListenState,
        Speaker, TranscriptLine,
    };
    use agentdictate_ui::{
        AgentDictateAssets, COPILOT_BAR_HEIGHT, COPILOT_PANEL_GAP, COPILOT_PANEL_HEIGHT,
        COPILOT_TOP_MARGIN, COPILOT_WIDTH, CopilotViewModel, CopilotWindow, CopilotWindowHost,
    };
    use gpui::{
        Application, Bounds, WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
        point, prelude::*, px, size,
    };
    use gpui_component::Root;
    use std::{rc::Rc, sync::Arc, time::Duration};

    struct FixtureHost;
    impl CopilotWindowHost for FixtureHost {
        fn resize(&self, _width: f32, _height: f32) {}
        fn set_visible(&self, _visible: bool) {}
        fn begin_typing(&self) {}
        fn end_typing(&self, _restore_focus: bool) {}
        fn set_capture_excluded(&self, _excluded: bool) {}
    }

    let mut args = std::env::args().skip(1);
    assert_eq!(args.next().as_deref(), Some("--isolated-desktop"));
    let state = args.next().unwrap_or_else(|| "live".to_owned());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let mut model = CopilotViewModel::new(CopilotSettings::default(), now - 754_000);
    let mut feed = |event| {
        model.apply(event);
    };
    if state != "empty" {
        feed(CopilotEvent::Listen(ListenState::Listening));
        for (index, level) in [
            0.1, 0.3, 0.55, 0.8, 0.6, 0.35, 0.7, 0.9, 0.5, 0.25, 0.4, 0.65, 0.3, 0.15,
        ]
        .into_iter()
        .enumerate()
        {
            let _ = index;
            feed(CopilotEvent::Levels {
                me: level * 0.6,
                them: level,
            });
        }
        let lines = [
            (
                Speaker::Them,
                "Thanks for making time. We're evaluating dictation tools for about two hundred engineers.",
                4_000,
            ),
            (
                Speaker::Me,
                "Great, happy to walk you through it. What matters most for your team?",
                11_000,
            ),
            (
                Speaker::Them,
                "Accuracy with technical vocabulary, and it has to work with our terminals and IDEs.",
                17_500,
            ),
            (
                Speaker::Me,
                "That's exactly where AgentDictate shines, replacements and vocabulary are built in.",
                26_000,
            ),
            (
                Speaker::Them,
                "What's the price for two hundred seats on the annual plan?",
                34_000,
            ),
        ];
        for (index, (speaker, text, at_ms)) in lines.into_iter().enumerate() {
            feed(CopilotEvent::Transcript(TranscriptLine {
                id: index as u64 + 1,
                speaker,
                text: text.to_owned(),
                at_ms,
            }));
        }
        feed(CopilotEvent::Insights(CopilotInsights {
            topic: "Enterprise pricing for 200 engineers".into(),
            summary: vec![
                "Prospect needs strong technical vocabulary support".into(),
                "Must work in terminals and IDEs".into(),
                "Now asking about annual pricing for 200 seats".into(),
            ],
            question: None,
            suggestions: vec![
                "Suggest a discount framing".into(),
                "Compare to competitors".into(),
                "Ask about rollout timeline".into(),
            ],
        }));
        feed(CopilotEvent::AnswerStarted {
            id: 1,
            title: "Recap so far".into(),
            used_screen: false,
            model: "gpt-6.1-sol".into(),
            automatic: false,
        });
        feed(CopilotEvent::AnswerDelta {
            id: 1,
            text: "- Evaluating for **~200 engineers**\n- Needs technical vocabulary and terminal support".into(),
        });
        feed(CopilotEvent::AnswerStatus {
            id: 1,
            status: AnswerStatus::Done,
        });
        feed(CopilotEvent::AnswerStarted {
            id: 2,
            title: "What's the price for two hundred seats on the annual plan?".into(),
            used_screen: false,
            model: "gpt-6.1-sol".into(),
            automatic: true,
        });
        feed(CopilotEvent::InstantDelta {
            id: 2,
            text: "\u{201c}Great question. For a team your size you'd be on our annual volume tier, which brings the per-seat price down meaningfully. Let me give you the exact number.\u{201d}".into(),
        });
        feed(CopilotEvent::InstantFinished { id: 2 });
        if state == "streaming" {
            feed(CopilotEvent::AnswerStatus {
                id: 2,
                status: AnswerStatus::Searching("AgentDictate team plan annual pricing".into()),
            });
        } else {
            feed(CopilotEvent::AnswerDelta {
                id: 2,
                text: "**Say:** \u{201c}For 200 seats on annual billing you'd be at **$18 per seat per month**, about **$43,200 a year**, and that includes SSO and priority support.\u{201d}\n\n- Annual billing saves **20%** versus monthly ($22.50)\n- Volume tier starts at 150 seats, so they already qualify\n- Offer a **30-day pilot** with 25 seats to de-risk the rollout".into(),
            });
            feed(CopilotEvent::AnswerSource {
                id: 2,
                source: AnswerSource {
                    title: "Pricing".into(),
                    url: "https://www.agentdictate.example/pricing".into(),
                },
            });
            feed(CopilotEvent::AnswerSource {
                id: 2,
                source: AnswerSource {
                    title: "Volume discounts".into(),
                    url: "https://docs.agentdictate.example/billing/volume".into(),
                },
            });
            feed(CopilotEvent::AnswerStatus {
                id: 2,
                status: AnswerStatus::Done,
            });
        }
    }
    if state == "overview" {
        // The Settings window's Overview card that starts the copilot.
        use agentdictate_core::{WorkflowPhase, WorkflowSnapshot};
        use agentdictate_ui::{AgentDictateWindowFrame, Route, SettingsShell, ShellViewModel};
        Application::new()
            .with_assets(AgentDictateAssets)
            .run(|cx| {
                gpui_component::init(cx);
                gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
                let model = ShellViewModel::from_snapshot(
                    Route::Overview,
                    WorkflowSnapshot {
                        phase: WorkflowPhase::Ready,
                    },
                );
                cx.open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(40.), px(40.)),
                            size(px(1_100.), px(760.)),
                        ))),
                        focus: false,
                        ..Default::default()
                    },
                    move |window, cx| {
                        let shell = cx.new(|cx| {
                            SettingsShell::connected(
                                model,
                                agentdictate_core::Settings::default(),
                                false,
                                Arc::new(|_| Ok(())),
                                window,
                                cx,
                            )
                        });
                        let frame = cx.new(|_| AgentDictateWindowFrame::new(shell));
                        cx.new(|cx| Root::new(frame, window, cx))
                    },
                )
                .expect("overview fixture opens");
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(2_000))
                        .await;
                    println!("READY");
                    cx.background_executor()
                        .timer(Duration::from_secs(20))
                        .await;
                    cx.update(|cx| cx.quit())
                })
                .detach();
            });
        return;
    }
    let collapsed = state == "collapsed";
    let height = COPILOT_TOP_MARGIN
        + COPILOT_BAR_HEIGHT
        + if collapsed {
            0.
        } else {
            COPILOT_PANEL_GAP + COPILOT_PANEL_HEIGHT
        }
        + 14.;
    Application::new()
        .with_assets(AgentDictateAssets)
        .run(move |cx| {
            gpui_component::init(cx);
            gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
            let window = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(40.), px(40.)),
                            size(px(COPILOT_WIDTH), px(height)),
                        ))),
                        titlebar: None,
                        focus: false,
                        show: true,
                        kind: WindowKind::PopUp,
                        is_movable: true,
                        window_background: WindowBackgroundAppearance::Transparent,
                        ..Default::default()
                    },
                    move |window, cx| {
                        let view = cx.new(|cx| {
                            CopilotWindow::new(
                                model,
                                Arc::new(|_| {}),
                                Rc::new(FixtureHost),
                                window,
                                cx,
                            )
                        });
                        view.update(cx, |view, cx| match state.as_str() {
                            "settings" => view.show_settings(window, cx),
                            "transcript" => view.show_transcript(cx),
                            "collapsed" => view.set_panel_open(false, cx),
                            _ => view.set_panel_open(true, cx),
                        });
                        cx.new(|cx| Root::new(view, window, cx))
                    },
                )
                .expect("copilot fixture opens");
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(1_500))
                    .await;
                let _ = window.update(cx, |_, window, _| window.refresh());
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                println!("READY");
                cx.background_executor()
                    .timer(Duration::from_secs(20))
                    .await;
                cx.update(|cx| cx.quit())
            })
            .detach();
        });
}

#[cfg(not(all(windows, feature = "desktop")))]
fn main() {}
