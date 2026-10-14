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
    MeetingDetailViewModel, MeetingNoteViewModel, MeetingsViewModel, Route, ThemeTokens,
    WorkspaceAction,
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
            list_view(found, total, &state.query, state.search_input, theme, cx)
        }
    }
}

fn list_view(
    notes: Vec<MeetingNoteViewModel>,
    total: usize,
    query: &str,
    search_input: Option<Entity<InputState>>,
    theme: ThemeTokens,
    cx: &mut Context<SettingsShell>,
) -> gpui::Div {
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
                    gpui::div()
                        .debug_selector(|| "meetings-search-input".to_owned())
                        .w_full()
                        .min_w_0()
                        .child(Input::new(&input).small().w_full()),
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
