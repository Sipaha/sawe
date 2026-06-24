//! Header component for the S-DET commit view: avatar, author, date,
//! parsed message body with mentions.

use std::sync::Arc;

use git::repository::CommitDetails;
use git::{BuildCommitPermalinkParams, GitRemote, ParsedGitRemote};
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, IntoElement, ParentElement, Styled, Window,
};
use ui::{Disclosure, Tooltip, prelude::*};

use crate::commit_tooltip::CommitAvatar;
use crate::git_panel_settings::{CommitViewSettings, GitPanelSettings};
use settings::Settings as _;

use super::mentions::{MessageToken, parse_message_tokens};

/// State the [`render_header`] surface needs to render the S-AI-EXP
/// "Explain" button + expandable section.
#[derive(Clone)]
pub(crate) struct ExplainHeaderState {
    pub pending: bool,
    pub body: Option<SharedString>,
    pub error: Option<SharedString>,
    pub from_cache: bool,
    pub expanded: bool,
    pub disabled_reason: Option<&'static str>,
    pub on_click: Arc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>,
}

/// Render the IDEA-style header for the commit view.
///
/// `gutter_width` is plumbed in so the avatar column lines up with the
/// editor gutter. `extra_committer` is `Some((name, email))` only when
/// the committer differs from the author — the typical clean commit
/// uses the same identity for both, so we hide the second line.
pub(crate) fn render_header(
    commit: &CommitDetails,
    remote: Option<&GitRemote>,
    extra_committer: Option<(SharedString, SharedString)>,
    is_stash: bool,
    gutter_width: gpui::Pixels,
    explain: ExplainHeaderState,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let settings = GitPanelSettings::get_global(cx).commit_view.clone();

    let author_name = commit.author_name.clone();
    let author_email = commit.author_email.clone();
    let commit_sha = commit.sha.clone();

    let commit_date = time::OffsetDateTime::from_unix_timestamp(commit.commit_timestamp)
        .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let absolute_date = time_format::format_localized_timestamp(
        commit_date,
        time::OffsetDateTime::now_utc(),
        local_offset,
        time_format::TimestampFormat::MediumAbsolute,
    );
    let relative_date = time_format::format_localized_timestamp(
        commit_date,
        time::OffsetDateTime::now_utc(),
        local_offset,
        time_format::TimestampFormat::Relative,
    );

    let clipboard_has_sha = cx
        .read_from_clipboard()
        .and_then(|entry| entry.text())
        .map_or(false, |clipboard_text| {
            clipboard_text.trim() == commit_sha.as_ref()
        });

    let (copy_icon, copy_icon_color) = if clipboard_has_sha {
        (IconName::Check, Color::Success)
    } else {
        (IconName::Copy, Color::Muted)
    };

    let parsed_remote_arc = remote.map(|remote| {
        Arc::new(ParsedGitRemote {
            owner: remote.owner.as_ref().into(),
            repo: remote.repo.as_ref().into(),
        })
    });

    let avatar = render_avatar(
        &commit_sha,
        Some(author_email.clone()),
        remote,
        &author_name,
        &settings,
        window,
        cx,
    );

    let absolute_for_tooltip: SharedString = absolute_date.into();
    let date_button = Button::new("commit-date", relative_date)
        .style(ButtonStyle::Subtle)
        .label_size(LabelSize::Small)
        .color(Color::Muted)
        .tooltip(move |_, cx| Tooltip::simple(absolute_for_tooltip.clone(), cx));

    let explain_button = render_explain_button(is_stash, &explain);
    let explain_panel = render_explain_panel(&explain, cx);

    let top_row = h_flex()
        .py_2()
        .pr_2p5()
        .w_full()
        .justify_between()
        .child(
            h_flex()
                .gap_2()
                .child(h_flex().w(gutter_width).justify_center().child(avatar))
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(render_message_block(commit, &settings, parsed_remote_arc))
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(Label::new(author_name).size(LabelSize::Small))
                                .when(!author_email.is_empty(), |this| {
                                    this.child(
                                        Label::new("•")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted)
                                            .alpha(0.5),
                                    )
                                    .child(
                                        Label::new(author_email)
                                            .color(Color::Muted)
                                            .size(LabelSize::Small),
                                    )
                                })
                                .child(
                                    Label::new("•")
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .alpha(0.5),
                                )
                                .child(date_button),
                        )
                        .when_some(extra_committer, |this, (name, email)| {
                            this.child(
                                h_flex()
                                    .gap_1p5()
                                    .child(
                                        Label::new("Committed by")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                    .child(Label::new(name).size(LabelSize::Small))
                                    .when(!email.is_empty(), |this| {
                                        this.child(
                                            Label::new(email)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted),
                                        )
                                    }),
                            )
                        }),
                ),
        )
        .child(
            h_flex()
                .gap_1p5()
                .when_some(explain_button, |this, btn| this.child(btn))
                .when(!is_stash, |this| {
                    this.child(
                        Button::new("sha", "Commit SHA")
                            .start_icon(
                                Icon::new(copy_icon)
                                    .size(IconSize::Small)
                                    .color(copy_icon_color),
                            )
                            .tooltip({
                                let commit_sha = commit_sha.clone();
                                move |_, cx| {
                                    Tooltip::with_meta(
                                        "Copy Commit SHA",
                                        None,
                                        commit_sha.clone(),
                                        cx,
                                    )
                                }
                            })
                            .on_click({
                                let commit_sha = commit_sha.clone();
                                move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        commit_sha.to_string(),
                                    ));
                                }
                            }),
                    )
                }),
        );

    v_flex()
        .w_full()
        .border_b_1()
        .border_color(cx.theme().colors().border_variant)
        .child(top_row)
        .when_some(explain_panel, |this, el| this.child(el))
        .into_any_element()
}

/// "Explain" button rendered next to "Commit SHA" in the top row. Returns
/// `None` for stash entries — the AI prompt would be unhelpful since
/// the stash "commit" is just the working-tree snapshot.
fn render_explain_button(is_stash: bool, explain: &ExplainHeaderState) -> Option<AnyElement> {
    if is_stash {
        return None;
    }
    let label = if explain.pending {
        "Explaining…"
    } else if explain.body.is_some() {
        if explain.expanded {
            "Hide Explanation"
        } else {
            "Show Explanation"
        }
    } else {
        "Explain"
    };
    let mut button = Button::new("commit-explain", label)
        .start_icon(Icon::new(IconName::Sparkle).size(IconSize::Small))
        .label_size(LabelSize::Small)
        .loading(explain.pending)
        .disabled(explain.pending || explain.disabled_reason.is_some());

    button = if let Some(reason) = explain.disabled_reason {
        let reason_str: SharedString = SharedString::from(reason);
        button.tooltip(move |_, cx| Tooltip::simple(reason_str.clone(), cx))
    } else if explain.pending {
        button.tooltip(Tooltip::text("AI explanation in progress"))
    } else if explain.body.is_some() {
        button.tooltip(Tooltip::text("Toggle the AI-generated explanation panel"))
    } else {
        button.tooltip(Tooltip::text(
            "Generate a 2-3 sentence AI explanation of this commit",
        ))
    };

    let on_click = explain.on_click.clone();
    button = button.on_click(move |event, window, cx| {
        cx.stop_propagation();
        on_click(event, window, cx);
    });
    Some(button.into_any_element())
}

/// The expandable explanation block under the top row. Rendered only
/// when there's something to show (a body, an in-flight request, or a
/// recent error).
fn render_explain_panel(explain: &ExplainHeaderState, cx: &App) -> Option<AnyElement> {
    let has_body = explain.body.is_some();
    let has_error = explain.error.is_some();
    if !explain.pending && !has_body && !has_error {
        return None;
    }
    let on_click = explain.on_click.clone();
    let toggle = Disclosure::new("commit-explain-disclosure", explain.expanded)
        .on_click(move |event, window, cx| on_click(event, window, cx));

    let header_label = if explain.pending && !has_body {
        Label::new("Generating explanation…")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element()
    } else if has_error {
        Label::new("AI explanation unavailable")
            .size(LabelSize::Small)
            .color(Color::Error)
            .into_any_element()
    } else {
        Label::new("AI explanation")
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element()
    };

    let cache_badge = if explain.from_cache && has_body {
        Some(
            Label::new("from cache")
                .size(LabelSize::XSmall)
                .color(Color::Muted)
                .into_any_element(),
        )
    } else {
        None
    };

    let header_row = h_flex()
        .gap_1p5()
        .px_2()
        .py_1()
        .child(toggle)
        .child(
            Icon::new(IconName::Sparkle)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(header_label)
        .when_some(cache_badge, |this, badge| this.child(badge));

    let body_block = if explain.expanded {
        if let Some(body) = explain.body.as_ref() {
            Some(
                div()
                    .px_2()
                    .pb_2()
                    .text_sm()
                    .text_color(cx.theme().colors().text)
                    .child(body.clone())
                    .into_any_element(),
            )
        } else if let Some(err) = explain.error.as_ref() {
            Some(
                div()
                    .px_2()
                    .pb_2()
                    .text_sm()
                    .text_color(cx.theme().colors().text_muted)
                    .child(err.clone())
                    .into_any_element(),
            )
        } else if explain.pending {
            Some(
                div()
                    .px_2()
                    .pb_2()
                    .text_sm()
                    .text_color(cx.theme().colors().text_muted)
                    .child(SharedString::from("Asking the AI agent…"))
                    .into_any_element(),
            )
        } else {
            None
        }
    } else {
        None
    };

    Some(
        v_flex()
            .w_full()
            .bg(cx.theme().colors().element_background)
            .border_t_1()
            .border_color(cx.theme().colors().border_variant)
            .child(header_row)
            .when_some(body_block, |this, el| this.child(el))
            .into_any_element(),
    )
}

fn render_avatar(
    sha: &SharedString,
    author_email: Option<SharedString>,
    remote: Option<&GitRemote>,
    author_name: &SharedString,
    settings: &CommitViewSettings,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    if settings.fetch_avatars && remote.is_some() {
        return CommitAvatar::new(sha, author_email, remote)
            .size(rems_from_px(40.))
            .render(window, cx);
    }

    // Privacy-default: don't fetch Gravatar URLs unless explicitly opted in.
    // Render a character tile instead so the layout is identical.
    let initial = author_name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().next().unwrap_or(c))
        .unwrap_or('?');
    let size = rems_from_px(40.).to_pixels(window.rem_size());
    h_flex()
        .size(size)
        .justify_center()
        .items_center()
        .rounded_full()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .bg(cx.theme().colors().element_disabled)
        .child(
            Label::new(initial.to_string())
                .size(LabelSize::Default)
                .color(Color::Muted),
        )
        .into_any_element()
}

fn render_message_block(
    commit: &CommitDetails,
    settings: &CommitViewSettings,
    remote: Option<Arc<ParsedGitRemote>>,
) -> AnyElement {
    let raw = commit.message.as_ref().trim_end();
    let mut lines = raw.split('\n');
    let subject = lines.next().unwrap_or("").to_string();
    let body: String = lines.collect::<Vec<_>>().join("\n").trim().to_string();

    let parse_issues = settings.parse_issue_references;
    let subject_tokens = parse_message_tokens(&subject, parse_issues);
    let body_tokens = if body.is_empty() {
        Vec::new()
    } else {
        parse_message_tokens(&body, parse_issues)
    };

    let permalink_for_issue = move |number: &str| -> Option<String> {
        let remote = remote.as_ref()?;
        let host = remote.owner.as_ref();
        let repo = remote.repo.as_ref();
        // Build a generic issue permalink using the cached remote owner /
        // repo and a github-style path. Hosting providers that don't follow
        // this layout simply produce a 404 — we keep the link clickable so
        // the user can override.
        let _ = host;
        let _ = BuildCommitPermalinkParams { sha: "" };
        Some(format!(
            "https://github.com/{}/{}/issues/{}",
            host, repo, number
        ))
    };

    let mut subject_children: Vec<AnyElement> = Vec::with_capacity(subject_tokens.len());
    for token in subject_tokens {
        subject_children.push(render_token(token, &permalink_for_issue));
    }
    let mut body_children: Vec<AnyElement> = Vec::with_capacity(body_tokens.len());
    for token in body_tokens {
        body_children.push(render_token(token, &permalink_for_issue));
    }

    v_flex()
        .gap_1()
        .child(h_flex().flex_wrap().children(subject_children))
        .when(!body.is_empty(), |this| {
            this.child(h_flex().flex_wrap().children(body_children))
        })
        .into_any_element()
}

fn render_token(
    token: MessageToken,
    permalink_for_issue: &impl Fn(&str) -> Option<String>,
) -> AnyElement {
    match token {
        MessageToken::Text(text) => Label::new(text).into_any_element(),
        MessageToken::Url(url) => {
            let label = url.clone();
            let id = SharedString::from(format!("url-{}", url));
            Button::new(id, label)
                .style(ButtonStyle::Subtle)
                .color(Color::Accent)
                .label_size(LabelSize::Default)
                .on_click(move |_, _, cx| cx.open_url(&url))
                .into_any_element()
        }
        MessageToken::IssueRef(number) => {
            let label = format!("#{}", number);
            let url = permalink_for_issue(&number);
            let mut btn = Button::new(SharedString::from(format!("issue-{}", number)), label)
                .style(ButtonStyle::Subtle)
                .color(Color::Accent)
                .label_size(LabelSize::Default);
            if let Some(url) = url {
                btn = btn.on_click(move |_, _, cx| cx.open_url(&url));
            }
            btn.into_any_element()
        }
        MessageToken::JiraRef(key) => {
            // No click handler — a Jira target needs configuration that
            // the plan defers. The token still renders styled so the user
            // sees it as a recognised reference.
            Label::new(format!("[{}]", key))
                .color(Color::Accent)
                .into_any_element()
        }
    }
}
