use agentdictate_core::EmailDraft;
use gpui::{ClipboardItem, Context, Entity, SharedString, Window, prelude::*, px, rems};
use gpui_component::{
    Sizable, h_flex,
    highlighter::HighlightTheme,
    input::{Input, InputState},
    text::{TextView, TextViewStyle},
    v_flex,
};

use crate::action::action_button;
use crate::{
    MeetingDetailViewModel, MeetingNoteViewModel, MeetingsAnswer, MeetingsViewModel,
    OpenPromiseViewModel, PersonViewModel, Route, ThemeTokens, WorkspaceAction,
};

use super::{SettingsShell, gpui_color, single_line::single_line_clip};

/// What the Meetings page shows besides the meetings themselves.
pub(super) struct MeetingsPageState {
    pub(super) feedback: Option<String>,
    pub(super) search_input: Option<Entity<InputState>>,
    pub(super) query: String,
    pub(super) pending_destructive_action: Option<WorkspaceAction>,
}

/// Saved meetings newest first, or one meeting's notes: summary, follow-up
/// email, the copilot's answers, and the transcript. Scrolling belongs to the
/// shell's route-content container.
pub(super) fn surface(
    meetings: MeetingsViewModel,
    state: MeetingsPageState,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    match meetings.selected.clone() {
        Some(detail) => detail_view(
            detail,
            state.feedback,
            state.pending_destructive_action,
            theme,
            window,
            cx,
        ),
        None => {
            let total = meetings.notes.len();
            let found = meetings.search(&state.query);
            let extras = ListExtras {
                people: meetings.people(),
                open: meetings.open_promises(),
                trend: meetings.talk_trend(),
                answer: meetings.answer.clone(),
                feedback: state.feedback,
            };
            list_view(
                found,
                total,
                &state.query,
                state.search_input,
                extras,
                theme,
                window,
                cx,
            )
        }
    }
}

impl SettingsShell {
    /// Asks the question in the search box across every saved meeting.
    pub(super) fn ask_meetings(&mut self, cx: &mut Context<Self>) {
        let question = self.routes.meetings_query.trim().to_owned();
        if question.is_empty() {
            self.set_route_feedback_for(
                Route::Meetings,
                "Type a question about your meetings first.".to_owned(),
            );
            cx.notify();
            return;
        }
        self.set_route_feedback_for(Route::Meetings, "Reading your meetings\u{2026}".to_owned());
        self.emit_workspace_action(WorkspaceAction::AskMeetings { question }, cx);
        cx.notify();
    }
}

/// What the list shows besides the meetings: who they were with, what is
/// still owed, how the user has been speaking, and the latest answer.
struct ListExtras {
    people: Vec<PersonViewModel>,
    open: Vec<OpenPromiseViewModel>,
    trend: Option<String>,
    answer: Option<MeetingsAnswer>,
    feedback: Option<String>,
}

/// People shown as chips before the rest are left to search.
const MAX_PEOPLE_CHIPS: usize = 10;
/// Open commitments listed before the rest are left to their meetings.
const MAX_OPEN_PROMISES: usize = 8;

#[allow(clippy::too_many_arguments)]
fn list_view(
    notes: Vec<MeetingNoteViewModel>,
    total: usize,
    query: &str,
    search_input: Option<Entity<InputState>>,
    extras: ListExtras,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    let ListExtras {
        people,
        open,
        trend,
        answer,
        feedback,
    } = extras;
    let section = |title: &'static str| {
        gpui::div()
            .pt_2()
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(gpui_color(theme.text_muted))
            .child(title)
    };
    let people_input = search_input.clone();
    let people_row =
        (!people.is_empty()).then(|| {
            v_flex()
                .debug_selector(|| "meetings-people".to_owned())
                .w_full()
                .gap_2()
                .child(section("PEOPLE"))
                .child(
                    h_flex().flex_wrap().gap_2().children(
                        people.into_iter().take(MAX_PEOPLE_CHIPS).enumerate().map(
                            |(index, person)| {
                                let name = person.name.clone();
                                let input = people_input.clone();
                                let mut label = format!(
                                    "{} \u{00b7} {}",
                                    person.name,
                                    if person.meetings == 1 {
                                        "1 meeting".to_owned()
                                    } else {
                                        format!("{} meetings", person.meetings)
                                    }
                                );
                                if person.open > 0 {
                                    label.push_str(&format!(" \u{00b7} {} open", person.open));
                                }
                                action_button(("meetings-person", index))
                                    .debug_selector(move || format!("meetings-person-{index}"))
                                    .small()
                                    .label(label)
                                    .on_click(cx.listener(move |shell, _, window, cx| {
                                        // A person's page is the list filtered
                                        // to the meetings with them.
                                        shell.routes.meetings_query = name.clone();
                                        if let Some(input) = &input {
                                            input.update(cx, |input, cx| {
                                                input.set_value(name.clone(), window, cx);
                                            });
                                        }
                                        cx.notify();
                                    }))
                            },
                        ),
                    ),
                )
        });
    let more_open = open.len().saturating_sub(MAX_OPEN_PROMISES);
    let open_rows = (!open.is_empty()).then(|| {
        v_flex()
            .debug_selector(|| "meetings-open-promises".to_owned())
            .w_full()
            .gap_1()
            .child(section("YOU STILL OWE"))
            .children(open.into_iter().take(MAX_OPEN_PROMISES).map(|promise| {
                let done = WorkspaceAction::SetMeetingPromise {
                    id: promise.meeting_id.clone(),
                    index: promise.index,
                    done: true,
                };
                let selector = done.selector();
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_3()
                    .py_1()
                    .child(
                        action_button(SharedString::from(selector.clone()))
                            .debug_selector(move || selector)
                            .small()
                            .label("Done")
                            .on_click(cx.listener(move |shell, _, _, cx| {
                                shell.emit_workspace_action(done.clone(), cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(gpui::div().text_sm().child(promise.text))
                            .child(
                                gpui::div()
                                    .text_xs()
                                    .text_color(gpui_color(theme.text_muted))
                                    .child(promise.meeting),
                            ),
                    )
            }))
            .when(more_open > 0, |column| {
                column.child(
                    gpui::div()
                        .text_xs()
                        .text_color(gpui_color(theme.text_muted))
                        .child(format!("And {more_open} more, in their meetings.")),
                )
            })
    });
    let answer_card = answer.map(|answer| {
        let style = TextViewStyle {
            is_dark: true,
            highlight_theme: HighlightTheme::default_dark(),
            ..TextViewStyle::default()
        }
        .paragraph_gap(rems(0.6));
        let copy = answer.answer.clone();
        v_flex()
            .debug_selector(|| "meetings-answer".to_owned())
            .w_full()
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(gpui_color(theme.surface))
            .border_1()
            .border_color(gpui_color(theme.border))
            .child(
                h_flex()
                    .w_full()
                    .gap_3()
                    .child(
                        gpui::div()
                            .min_w_0()
                            .flex_1()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(answer.question),
                    )
                    .child(
                        action_button("meetings-answer-copy")
                            .debug_selector(|| "meetings-answer-copy".to_owned())
                            .small()
                            .label("Copy")
                            .on_click(cx.listener(move |shell, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                shell.set_route_feedback_for(
                                    Route::Meetings,
                                    "Copied the answer.".to_owned(),
                                );
                                cx.notify();
                            })),
                    ),
            )
            .child(
                gpui::div()
                    .w_full()
                    .text_sm()
                    .line_height(rems(1.5))
                    .text_color(gpui_color(theme.text))
                    .child(
                        TextView::markdown("meetings-answer-text", answer.answer, window, cx)
                            .style(style)
                            .selectable(true),
                    ),
            )
    });
    let searching = !query.trim().is_empty();
    let count = match (searching, notes.len(), total) {
        (true, 1, _) => "1 matching meeting".to_owned(),
        (true, found, _) => format!("{found} matching meetings"),
        (false, _, 1) => "1 saved meeting".to_owned(),
        (false, _, count) => format!("{count} saved meetings"),
    };
    let no_matches = searching && notes.is_empty();
    let query = query.trim().to_owned();
    let open_folder = WorkspaceAction::OpenMeetingsFolder;
    let open_folder_selector = open_folder.selector();
    v_flex()
        .debug_selector(|| "meetings-page".to_owned())
        .w_full()
        .min_w_0()
        .gap_2()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .h(px(48.))
                .justify_between()
                .gap_4()
                .border_b_1()
                .border_color(gpui_color(theme.border))
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_2()
                        .child(
                            gpui::div()
                                .size_2()
                                .flex_none()
                                .rounded_full()
                                .bg(gpui_color(crate::Color::rgb(251, 191, 36))),
                        )
                        .child(
                            gpui::div()
                                .flex_none()
                                .text_sm()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Meetings"),
                        )
                        .child(
                            single_line_clip("meetings-count", count)
                                .text_xs()
                                .text_color(gpui_color(theme.text_muted)),
                        ),
                )
                .child(
                    action_button(SharedString::from(open_folder_selector.clone()))
                        .debug_selector(move || open_folder_selector)
                        .small()
                        .label("Open folder")
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.emit_workspace_action(open_folder.clone(), cx);
                            cx.notify();
                        })),
                ),
        )
        .when(total > 0, |page| {
            page.when_some(search_input, |page, input| {
                page.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            gpui::div()
                                .debug_selector(|| "meetings-search-input".to_owned())
                                .min_w_0()
                                .flex_1()
                                .child(Input::new(&input).small().w_full()),
                        )
                        .child(
                            action_button("meetings-ask")
                                .debug_selector(|| "meetings-ask".to_owned())
                                .small()
                                .label("Ask")
                                .on_click(cx.listener(|shell, _, _, cx| shell.ask_meetings(cx))),
                        ),
                )
            })
        })
        .when_some(feedback, |page, feedback| {
            page.child(
                gpui::div()
                    .debug_selector(|| "meetings-feedback".to_owned())
                    .text_xs()
                    .text_color(gpui_color(theme.text_muted))
                    .child(feedback),
            )
        })
        .when_some(answer_card, |page, card| page.child(card))
        .when(!searching, |page| {
            page.when_some(open_rows, |page, rows| page.child(rows))
                .when_some(people_row, |page, row| page.child(row))
                .when_some(trend, |page, trend| {
                    page.child(
                        gpui::div()
                            .debug_selector(|| "meetings-talk-trend".to_owned())
                            .pt_1()
                            .text_xs()
                            .text_color(gpui_color(theme.text_muted))
                            .child(trend),
                    )
                })
        })
        .when(no_matches, |page| {
            page.child(
                gpui::div()
                    .debug_selector(|| "meetings-no-matches".to_owned())
                    .py_6()
                    .text_sm()
                    .text_color(gpui_color(theme.text_muted))
                    .child(format!(
                        "No meetings mention \u{201c}{query}\u{201d}. Search covers titles, people, summaries, and transcripts."
                    )),
            )
        })
        .when(total == 0, |page| {
            page.child(
                gpui::div()
                    .debug_selector(|| "meetings-empty".to_owned())
                    .py_6()
                    .text_sm()
                    .text_color(gpui_color(theme.text_muted))
                    .child(
                        "No meetings yet. Start Meeting Copilot during a call; when you end the \
session, its summary, follow-up email, and transcript appear here.",
                    ),
            )
        })
        .children(notes.into_iter().map(|note| meeting_row(note, theme, cx)))
}

pub(super) fn meeting_row(
    note: MeetingNoteViewModel,
    theme: ThemeTokens,
    cx: &mut Context<SettingsShell>,
) -> impl IntoElement + use<> {
    let action = WorkspaceAction::OpenMeeting {
        id: note.id.clone(),
    };
    let selector = action.selector();
    let metadata = note.metadata();
    let overview = match (note.with.trim(), note.overview.trim()) {
        ("", overview) => overview.to_owned(),
        (with, "") => format!("With {with}"),
        (with, overview) => format!("With {with} \u{00b7} {overview}"),
    };
    v_flex()
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector)
        .w_full()
        .min_w_0()
        .gap_1()
        .px_2()
        .py_3()
        .rounded_md()
        .border_b_1()
        .border_color(gpui_color(theme.border))
        .cursor_pointer()
        .hover(|style| style.bg(gpui_color(theme.surface_hovered)))
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .justify_between()
                .gap_4()
                .child(
                    single_line_clip(format!("meetings-title-{}", note.id), note.title)
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM),
                )
                .child(
                    gpui::div()
                        .flex_none()
                        .text_xs()
                        .text_color(gpui_color(theme.text_muted))
                        .child(metadata),
                ),
        )
        .when(!overview.is_empty(), |row| {
            row.child(
                single_line_clip(format!("meetings-overview-{}", note.id), overview)
                    .text_xs()
                    .text_color(gpui_color(theme.text_muted)),
            )
        })
        .on_click(cx.listener(move |shell, _, _, cx| {
            // A meeting opens at its top, on the Meetings page.
            shell
                .routes
                .entry(Route::Meetings)
                .scroll
                .set_offset(gpui::point(px(0.), px(0.)));
            shell.emit_workspace_action(action.clone(), cx);
            if shell.model.active_route != Route::Meetings {
                shell.select_route(Route::Meetings, false, cx);
            }
            cx.notify();
        }))
}

fn detail_view(
    detail: MeetingDetailViewModel,
    feedback: Option<String>,
    pending_destructive_action: Option<WorkspaceAction>,
    theme: ThemeTokens,
    window: &mut Window,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
    let style = TextViewStyle {
        is_dark: true,
        highlight_theme: HighlightTheme::default_dark(),
        ..TextViewStyle::default()
    }
    .paragraph_gap(rems(0.6));
    let markdown =
        |id: &str, text: String, window: &mut Window, cx: &mut Context<SettingsShell>| {
            // List items take their color from the container, not the window.
            gpui::div()
                .w_full()
                .text_sm()
                .line_height(rems(1.5))
                .text_color(gpui_color(theme.text))
                .child(
                    TextView::markdown(SharedString::from(id.to_owned()), text, window, cx)
                        .style(style.clone())
                        .selectable(true),
                )
        };
    let section = |title: &'static str| {
        gpui::div()
            .pt_2()
            .text_xs()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(gpui_color(theme.text_muted))
            .child(title)
    };
    let back = WorkspaceAction::CloseMeeting;
    let back_selector = back.selector();
    let open_file = WorkspaceAction::OpenMeetingFile {
        id: detail.id.clone(),
    };
    let open_file_selector = open_file.selector();
    let draft_email = WorkspaceAction::DraftMeetingEmail {
        id: detail.id.clone(),
    };
    let draft_email_selector = draft_email.selector();
    let delete = WorkspaceAction::DeleteMeeting {
        id: detail.id.clone(),
    };
    let delete_pending = pending_destructive_action.as_ref() == Some(&delete);
    let delete_selector = if delete_pending {
        format!("confirm-{}", delete.selector())
    } else {
        delete.selector()
    };
    let metadata = match detail.with.trim() {
        "" => detail.metadata(),
        with => format!("{} \u{00b7} With {with}", detail.metadata()),
    };
    let follow_up = detail.follow_up.clone();
    let draft = follow_up
        .as_deref()
        .map(|email| EmailDraft::from_follow_up(email, &detail.title));
    let export = detail.export_markdown();
    let promises = detail.promises.clone();
    let promise_id = detail.id.clone();
    let mut page = v_flex()
        .debug_selector(|| "meetings-detail".to_owned())
        .w_full()
        .gap_3()
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .child(
                    action_button(SharedString::from(back_selector.clone()))
                        .debug_selector(move || back_selector)
                        .small()
                        .label("All meetings")
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.emit_workspace_action(back.clone(), cx);
                            cx.notify();
                        })),
                )
                .child(gpui::div().flex_1())
                .when_some(draft, |row, draft| {
                    row.child(
                        action_button(SharedString::from(draft_email_selector.clone()))
                            .debug_selector(move || draft_email_selector)
                            .small()
                            .label("Draft email")
                            .on_click(cx.listener(move |shell, _, _, cx| {
                                // Mail apps cut long links: such an email
                                // waits on the clipboard to be pasted in.
                                let complete = draft.mailto().1;
                                if !complete {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        draft.body.clone(),
                                    ));
                                }
                                shell.emit_workspace_action_then(
                                    draft_email.clone(),
                                    if complete {
                                        "Opened a draft in your mail app. Nothing is sent until you send it."
                                            .to_owned()
                                    } else {
                                        "Opened a draft in your mail app; paste the email from your clipboard."
                                            .to_owned()
                                    },
                                    cx,
                                );
                                cx.notify();
                            })),
                    )
                })
                .when_some(follow_up.clone(), |row, email| {
                    row.child(
                        action_button("meetings-copy-follow-up")
                            .debug_selector(|| "meetings-copy-follow-up".to_owned())
                            .small()
                            .label("Copy email")
                            .on_click(cx.listener(move |shell, _, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(email.clone()));
                                shell.set_route_feedback_for(
                                    Route::Meetings,
                                    "Copied. Paste it into your email.".to_owned(),
                                );
                                cx.notify();
                            })),
                    )
                })
                .child(
                    action_button("meetings-copy-notes")
                        .debug_selector(|| "meetings-copy-notes".to_owned())
                        .small()
                        .label("Copy notes")
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(export.clone()));
                            shell.set_route_feedback_for(
                                Route::Meetings,
                                "Copied the notes without the transcript. Paste them into a document, your CRM, or a chat."
                                    .to_owned(),
                            );
                            cx.notify();
                        })),
                )
                .child(
                    action_button(SharedString::from(open_file_selector.clone()))
                        .debug_selector(move || open_file_selector)
                        .small()
                        .label("Open file")
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.emit_workspace_action(open_file.clone(), cx);
                            cx.notify();
                        })),
                )
                .child(
                    action_button(SharedString::from(delete_selector.clone()))
                        .debug_selector(move || delete_selector)
                        .small()
                        .label(if delete_pending {
                            "Confirm delete"
                        } else {
                            "Delete"
                        })
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.request_destructive_action(delete.clone(), cx);
                            cx.notify();
                        })),
                ),
        )
        .when_some(feedback, |page, feedback| {
            page.child(
                gpui::div()
                    .debug_selector(|| "meetings-feedback".to_owned())
                    .text_xs()
                    .text_color(gpui_color(theme.text_muted))
                    .child(feedback),
            )
        })
        .child(
            v_flex()
                .gap_1()
                .child(
                    gpui::div()
                        .debug_selector(|| "meetings-detail-title".to_owned())
                        .text_xl()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(detail.title.clone()),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(gpui_color(theme.text_muted))
                        .child(metadata),
                ),
        );
    if !detail.summary.is_empty() {
        page = page.child(
            markdown("meetings-summary", detail.summary, window, cx)
                .debug_selector(|| "meetings-summary".to_owned()),
        );
    }
    if !promises.is_empty() {
        page = page.child(
            v_flex()
                .debug_selector(|| "meetings-promises".to_owned())
                .w_full()
                .gap_1()
                .child(section("YOUR COMMITMENTS"))
                .children(
                    promises
                        .into_iter()
                        .enumerate()
                        .map(|(index, (text, done))| {
                            let toggle = WorkspaceAction::SetMeetingPromise {
                                id: promise_id.clone(),
                                index,
                                done: !done,
                            };
                            let selector = toggle.selector();
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .gap_3()
                                .py_1()
                                .child(
                                    action_button(SharedString::from(selector.clone()))
                                        .debug_selector(move || selector)
                                        .small()
                                        .label(if done { "Undo" } else { "Done" })
                                        .on_click(cx.listener(move |shell, _, _, cx| {
                                            shell.emit_workspace_action(toggle.clone(), cx);
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    gpui::div()
                                        .min_w_0()
                                        .flex_1()
                                        .text_sm()
                                        .when(done, |text| {
                                            text.line_through()
                                                .text_color(gpui_color(theme.text_muted))
                                        })
                                        .child(text),
                                )
                        }),
                ),
        );
    }
    if !detail.my_notes.is_empty() {
        page = page.child(section("YOUR NOTES")).child(
            markdown("meetings-my-notes", detail.my_notes, window, cx)
                .debug_selector(|| "meetings-my-notes".to_owned()),
        );
    }
    if !detail.scorecard.is_empty() {
        page = page.child(section("HOW YOU SPOKE")).child(
            markdown("meetings-scorecard", detail.scorecard, window, cx)
                .debug_selector(|| "meetings-scorecard".to_owned()),
        );
    }
    if let Some(email) = follow_up {
        page = page.child(
            v_flex()
                .debug_selector(|| "meetings-follow-up".to_owned())
                .w_full()
                .gap_2()
                .p_4()
                .rounded_lg()
                .bg(gpui_color(theme.surface))
                .border_1()
                .border_color(gpui_color(theme.border))
                .child(section("FOLLOW-UP EMAIL"))
                .child(markdown("meetings-follow-up-text", email, window, cx)),
        );
    }
    if !detail.answers.is_empty() {
        page = page.child(section("COPILOT ANSWERS")).child(markdown(
            "meetings-answers",
            detail.answers,
            window,
            cx,
        ));
    }
    page.child(section("TRANSCRIPT")).child(
        markdown("meetings-transcript", detail.transcript, window, cx)
            .debug_selector(|| "meetings-transcript".to_owned()),
    )
}
