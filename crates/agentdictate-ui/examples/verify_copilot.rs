//! Native copilot rendering probe. Run only through test-windows-copilot.ps1,
//! which places the window on an isolated, inactive Windows desktop and
//! captures it for inspection.
#[cfg(all(windows, feature = "desktop"))]
fn main() {
    use agentdictate_core::{
        AnswerSource, AnswerStatus, CopilotEvent, CopilotInsights, CopilotSettings,
        KnowledgeSummary, ListenState, Situation, Speaker, TranscriptLine,
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
    // The "welcome" state shows the first-run welcome; others are past it.
    let settings = CopilotSettings {
        onboarded: state != "welcome",
        ..CopilotSettings::default()
    };
    let mut model = CopilotViewModel::new(settings, now - 754_000);
    let mut feed = |event| {
        model.apply(event);
    };
    if state != "empty" && state != "welcome" {
        feed(CopilotEvent::Knowledge(KnowledgeSummary {
            documents: 3,
            meetings: 4,
            words: 5_240,
            unreadable: vec!["Sales deck.pdf".into()],
        }));
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
            // Two people on the other side: the buyer and their engineer.
            let voice = (speaker == Speaker::Them).then_some(u8::from(index == 2));
            feed(CopilotEvent::Transcript(TranscriptLine {
                id: index as u64 + 1,
                speaker,
                text: text.to_owned(),
                at_ms,
                voice,
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
            situation: Situation::Conversation,
            ..CopilotInsights::default()
        }));
        // The buyer introduced herself; the engineer has no name yet.
        feed(CopilotEvent::Speakers {
            names: [(0, "Dana Ruiz".to_owned())].into(),
            seen: vec!["Dana Ruiz".into(), "Priya Shah".into()],
        });
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
                text: "\u{201c}For 200 seats on annual billing it's **$18 per seat per month**, about **$43,200 a year**, with SSO and priority support included.\u{201d}\n\n- Annual billing saves **20%** versus monthly ($22.50)\n- Volume tier starts at 150 seats, so they already qualify\n- Offer a **30-day pilot** with 25 seats to de-risk the rollout".into(),
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
    if state == "wrapup" {
        feed(CopilotEvent::Ending);
        feed(CopilotEvent::NotesSaved {
            path: "C:/Users/me/AppData/Local/AgentDictate/meetings/2026-10-13 0900 Enterprise pricing.md".into(),
            summary: Some("Acme is evaluating dictation for about 200 engineers and asked for annual pricing.\n\n**Key points**\n- Needs strong technical vocabulary and terminal support\n- 200 seats on annual billing: **$18 per seat per month**\n\n**Decisions**\n- Start with a 30-day pilot for 25 engineers\n\n**Action items**\n- Me: send the pilot agreement and pricing by Friday\n- Them: name a pilot lead".into()),
            follow_up: Some("Hi Sam,\n\nThanks for the time today. As discussed, we'll start with a **30-day pilot for 25 engineers**, then move to annual billing at $18 per seat per month.\n\nNext steps:\n- I'll send the pilot agreement and pricing by Friday\n- You'll name a pilot lead\n\nBest,\nJames".into()),
        });
    }
    if state == "overview" || state == "meetings" {
        // The Settings window's Overview card that starts the copilot, or its
        // Meetings page with one meeting open.
        use agentdictate_core::{WorkflowPhase, WorkflowSnapshot};
        use agentdictate_ui::{
            AgentDictateWindowFrame, MeetingDetailViewModel, MeetingNoteViewModel,
            MeetingsViewModel, Route, SettingsShell, ShellViewModel, WorkspaceViewModel,
        };
        let meetings = state == "meetings";
        Application::new()
            .with_assets(AgentDictateAssets)
            .run(move |cx| {
                gpui_component::init(cx);
                gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);
                let mut model = ShellViewModel::from_snapshot(
                    if meetings {
                        Route::Meetings
                    } else {
                        Route::Overview
                    },
                    WorkflowSnapshot {
                        phase: WorkflowPhase::Ready,
                    },
                );
                {
                    let note = MeetingNoteViewModel {
                        id: "2026-10-13 0900 Enterprise pricing.md".into(),
                        title: "Enterprise pricing for 200 engineers".into(),
                        started: "2026-10-13 09:00".into(),
                        duration: "32:14".into(),
                        overview: "Acme is evaluating dictation for about 200 engineers.".into(),
                        with: "Dana Ruiz, Priya Shah".into(),
                        search_text: String::new(),
                    };
                    let earlier = [
                        ("2026-10-12 1500 Hiring sync.md", "Hiring sync", "2026-10-12 15:00", "24:02", "Two backend roles open; offers by Friday.", "Marcus Webb"),
                        ("2026-10-10 1000 Budget review.md", "Q4 budget review", "2026-10-10 10:00", "41:37", "Q4 budget approved with a 10% buffer.", ""),
                    ]
                    .map(|(id, title, started, duration, overview, with)| MeetingNoteViewModel {
                        id: id.into(),
                        title: title.into(),
                        started: started.into(),
                        duration: duration.into(),
                        overview: overview.into(),
                        with: with.into(),
                        search_text: String::new(),
                    });
                    let mut notes = vec![note.clone()];
                    notes.extend(earlier);
                    model = model.with_workspace(WorkspaceViewModel {
                        meetings: MeetingsViewModel {
                            selected: meetings.then(|| MeetingDetailViewModel {
                                id: note.id.clone(),
                                title: note.title.clone(),
                                started: note.started.clone(),
                                duration: note.duration.clone(),
                                with: note.with.clone(),
                                summary: "Acme is evaluating dictation for about 200 engineers and asked for annual pricing.\n\n**Key points**\n- Needs strong technical vocabulary and terminal support\n- 200 seats on annual billing: **$18 per seat per month**\n\n**Decisions**\n- Start with a 30-day pilot for 25 engineers\n\n**Action items**\n- Me: send the pilot agreement by Friday\n- Them: name a pilot lead".into(),
                                follow_up: Some("Hi Sam,\n\nThanks for the time today. As discussed, we'll start with a **30-day pilot for 25 engineers**.\n\nBest,\nJames".into()),
                                answers: String::new(),
                                transcript: "**[0:04] Dana Ruiz:** Thanks for making time. We're evaluating dictation tools for about two hundred engineers.\n\n**[0:11] You:** Great, happy to walk you through it.".into(),
                            }),
                            notes,
                        },
                        ..WorkspaceViewModel::default()
                    });
                }
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
            // Match the production copilot: transparent around the bar.
            gpui_component::Theme::global_mut(cx).background = gpui::transparent_black();
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
                            "naming" => {
                                view.show_transcript(cx);
                                view.begin_naming(1, window, cx);
                            }
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
