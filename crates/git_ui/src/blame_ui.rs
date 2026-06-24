use crate::{
    commit_context_menu::{CommitContext, build_commit_context_menu},
    commit_tooltip::{CommitAvatar, CommitTooltip},
    commit_view::CommitView,
    git_graph::OpenAtCommit,
};
use editor::{
    BlameRenderer, Editor,
    git::blame::BlameOptions,
    git::blame_colors::{ColorMode, author_color, date_color},
    hover_markdown_style,
};
use git::{
    GitHostingProviderRegistry, blame::BlameEntry, commit::ParsedCommitMessage,
    parse_git_remote_url, repository::CommitSummary,
};
use gpui::{
    Entity, Hsla, MouseButton, ScrollHandle, TextStyle, TextStyleRefinement, UnderlineStyle,
    WeakEntity, prelude::*,
};
use markdown::{Markdown, MarkdownElement};
use project::{git_store::Repository, project_settings::ProjectSettings};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use time::OffsetDateTime;
use ui::{CopyButton, Divider, prelude::*, tooltip_container};
use workspace::Workspace;

const GIT_BLAME_MAX_AUTHOR_CHARS_DISPLAYED: usize = 20;

pub struct GitBlameRenderer;

impl BlameRenderer for GitBlameRenderer {
    fn max_author_length(&self) -> usize {
        GIT_BLAME_MAX_AUTHOR_CHARS_DISPLAYED
    }

    fn render_blame_entry(
        &self,
        style: &TextStyle,
        blame_entry: BlameEntry,
        details: Option<ParsedCommitMessage>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        editor: Entity<Editor>,
        ix: usize,
        sha_color: Hsla,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.render_blame_entry_with_options(
            style,
            blame_entry,
            details,
            repository,
            workspace,
            editor,
            ix,
            sha_color,
            &BlameOptions::default(),
            None,
            window,
            cx,
        )
    }

    fn render_blame_entry_with_options(
        &self,
        style: &TextStyle,
        blame_entry: BlameEntry,
        details: Option<ParsedCommitMessage>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        editor: Entity<Editor>,
        ix: usize,
        sha_color: Hsla,
        options: &BlameOptions,
        date_range: Option<(i64, i64)>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        // S-ANN — when an author filter is active and this entry doesn't
        // match, render a single muted dot so the gutter remains readable
        // but other people's lines visually recede.
        if !options.author_filter.matches(&blame_entry) {
            return Some(render_muted_blame_entry(style, ix, cx));
        }

        let timestamp = if options.absolute_dates {
            blame_entry_absolute_timestamp(&blame_entry)
        } else {
            blame_entry_relative_timestamp(&blame_entry)
        };
        let short_commit_id = blame_entry.sha.display_short();
        let author_name = blame_entry.author.as_deref().unwrap_or("<no name>");
        let name = util::truncate_and_trailoff(author_name, GIT_BLAME_MAX_AUTHOR_CHARS_DISPLAYED);

        let resolved_color = match options.color_mode {
            ColorMode::None => sha_color,
            ColorMode::ByAuthor => blame_entry
                .author_mail
                .as_deref()
                .map(|email| author_color(email, cx))
                .unwrap_or(sha_color),
            ColorMode::ByDate => date_range
                .and_then(|(oldest, newest)| {
                    let theme = cx.theme();
                    let cold = theme.status().info;
                    let hot = theme.status().error;
                    let time = blame_entry.author_time?;
                    date_color(time, oldest, newest, cold, hot)
                })
                .unwrap_or(sha_color),
        };

        let avatar = if ProjectSettings::get_global(cx).git.blame.show_avatar {
            let author_email = blame_entry.author_mail.as_ref().map(|email| {
                SharedString::from(
                    email
                        .trim_start_matches('<')
                        .trim_end_matches('>')
                        .to_string(),
                )
            });
            Some(
                CommitAvatar::new(
                    &blame_entry.sha.to_string().into(),
                    author_email,
                    details.as_ref().and_then(|it| it.remote.as_ref()),
                )
                .render(window, cx),
            )
        } else {
            None
        };

        Some(
            div()
                .mr_2()
                .child(
                    h_flex()
                        .id(("blame", ix))
                        .w_full()
                        .gap_2()
                        .justify_between()
                        .font(style.font())
                        .line_height(style.line_height)
                        .text_color(cx.theme().status().hint)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().text_color(resolved_color).child(short_commit_id))
                                .children(avatar)
                                .child(name),
                        )
                        .child(timestamp)
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Right, {
                            let blame_entry = blame_entry.clone();
                            let details = details.clone();
                            let editor = editor.clone();
                            let repository = repository.clone();
                            let workspace = workspace.clone();
                            move |event, window, cx| {
                                cx.stop_propagation();

                                deploy_blame_entry_context_menu(
                                    &blame_entry,
                                    details.as_ref(),
                                    repository.clone(),
                                    workspace.clone(),
                                    editor.clone(),
                                    event.position,
                                    window,
                                    cx,
                                );
                            }
                        })
                        .on_click({
                            // S-ANN — left-click navigates to the commit in
                            // the Git Graph view (opens it if not present).
                            let blame_entry = blame_entry.clone();
                            move |_, window, cx| {
                                let sha = blame_entry.sha.to_string();
                                window.dispatch_action(Box::new(OpenAtCommit { sha }), cx);
                            }
                        })
                        .when(!editor.read(cx).has_mouse_context_menu(), |el| {
                            el.hoverable_tooltip(move |_window, cx| {
                                cx.new(|cx| {
                                    CommitTooltip::blame_entry(
                                        &blame_entry,
                                        details.clone(),
                                        repository.clone(),
                                        workspace.clone(),
                                        cx,
                                    )
                                })
                                .into()
                            })
                        }),
                )
                .into_any(),
        )
    }

    fn render_inline_blame_entry(
        &self,
        style: &TextStyle,
        blame_entry: BlameEntry,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let relative_timestamp = blame_entry_relative_timestamp(&blame_entry);
        let author = blame_entry.author.as_deref().unwrap_or_default();
        let summary_enabled = ProjectSettings::get_global(cx)
            .git
            .inline_blame
            .show_commit_summary;

        let text = match blame_entry.summary.as_ref() {
            Some(summary) if summary_enabled => {
                format!("{}, {} - {}", author, relative_timestamp, summary)
            }
            _ => format!("{}, {}", author, relative_timestamp),
        };

        Some(
            h_flex()
                .id("inline-blame")
                .w_full()
                .font(style.font())
                .text_color(cx.theme().status().hint)
                .line_height(style.line_height)
                .child(Icon::new(IconName::FileGit).color(Color::Hint))
                .child(text)
                .gap_2()
                .into_any(),
        )
    }

    fn render_blame_entry_popover(
        &self,
        blame: BlameEntry,
        scroll_handle: ScrollHandle,
        details: Option<ParsedCommitMessage>,
        markdown: Entity<Markdown>,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let commit_time = blame
            .committer_time
            .and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok())
            .unwrap_or(OffsetDateTime::now_utc());

        let sha = blame.sha.to_string().into();
        let author: SharedString = blame
            .author
            .clone()
            .unwrap_or("<no name>".to_string())
            .into();
        let author_email = blame.author_mail.as_deref().unwrap_or_default();
        let author_email_for_avatar = blame.author_mail.as_ref().map(|email| {
            SharedString::from(
                email
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string(),
            )
        });
        let avatar = CommitAvatar::new(
            &sha,
            author_email_for_avatar,
            details.as_ref().and_then(|it| it.remote.as_ref()),
        )
        .render(window, cx);

        let short_commit_id = sha
            .get(..git::SHORT_SHA_LENGTH)
            .map(|sha| sha.to_string().into())
            .unwrap_or_else(|| sha.clone());
        let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
        let absolute_timestamp = time_format::format_localized_timestamp(
            commit_time,
            OffsetDateTime::now_utc(),
            local_offset,
            time_format::TimestampFormat::MediumAbsolute,
        );
        let link_color = cx.theme().colors().text_accent;
        let markdown_style = {
            let mut style = hover_markdown_style(window, cx);
            style.link.refine(&TextStyleRefinement {
                color: Some(link_color),
                underline: Some(UnderlineStyle {
                    color: Some(link_color.opacity(0.4)),
                    thickness: px(1.0),
                    ..Default::default()
                }),
                ..Default::default()
            });
            style
        };

        let message = details
            .as_ref()
            .map(|_| {
                MarkdownElement::new(markdown.clone(), markdown_style)
                    .scroll_handle(scroll_handle.clone())
                    .into_any()
            })
            .unwrap_or("<no commit message>".into_any());

        let pull_request = details
            .as_ref()
            .and_then(|details| details.pull_request.clone());

        let ui_font_size = ThemeSettings::get_global(cx).ui_font_size(cx);
        let message_max_height = window.line_height() * 12 + (ui_font_size / 0.4);
        let commit_summary = CommitSummary {
            sha: sha.clone(),
            subject: details
                .as_ref()
                .and_then(|details| {
                    Some(
                        details
                            .message
                            .split('\n')
                            .next()?
                            .trim_end()
                            .to_string()
                            .into(),
                    )
                })
                .unwrap_or_default(),
            commit_timestamp: commit_time.unix_timestamp(),
            author_name: author.clone(),
            has_parent: false,
        };

        let sha_for_log = sha.to_string();
        Some(
            tooltip_container(cx, |this, cx| {
                this.occlude()
                    .on_mouse_move(|_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        v_flex()
                            .w(gpui::rems(30.))
                            .child(
                                h_flex()
                                    .pb_1()
                                    .gap_2()
                                    .overflow_x_hidden()
                                    .flex_wrap()
                                    .border_b_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .child(avatar)
                                    .child(author)
                                    .when(!author_email.is_empty(), |this| {
                                        this.child(
                                            div()
                                                .text_color(cx.theme().colors().text_muted)
                                                .child(author_email.to_owned()),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id("inline-blame-commit-message")
                                    .track_scroll(&scroll_handle)
                                    .py_1p5()
                                    .max_h(message_max_height)
                                    .overflow_y_scroll()
                                    .child(message),
                            )
                            .child(
                                h_flex()
                                    .text_color(cx.theme().colors().text_muted)
                                    .w_full()
                                    .justify_between()
                                    .pt_1()
                                    .border_t_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .child(absolute_timestamp)
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .when_some(pull_request, |this, pr| {
                                                this.child(
                                                    Button::new(
                                                        "pull-request-button",
                                                        format!("#{}", pr.number),
                                                    )
                                                    .color(Color::Muted)
                                                    .start_icon(
                                                        Icon::new(IconName::PullRequest)
                                                            .size(IconSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                    .on_click(move |_, _, cx| {
                                                        cx.stop_propagation();
                                                        cx.open_url(pr.url.as_str())
                                                    }),
                                                )
                                                .child(Divider::vertical())
                                            })
                                            // S-ANN — "Show in Log" jumps to
                                            // this commit in the Git Graph
                                            // view (opens it on demand).
                                            .child(
                                                Button::new("show-in-log-button", "Show in Log")
                                                    .color(Color::Muted)
                                                    .start_icon(
                                                        Icon::new(IconName::ListTree)
                                                            .size(IconSize::Small)
                                                            .color(Color::Muted),
                                                    )
                                                    .on_click(move |_, window, cx| {
                                                        window.dispatch_action(
                                                            Box::new(OpenAtCommit {
                                                                sha: sha_for_log.clone(),
                                                            }),
                                                            cx,
                                                        );
                                                        cx.stop_propagation();
                                                    }),
                                            )
                                            .child(Divider::vertical())
                                            .child(
                                                Button::new(
                                                    "commit-sha-button",
                                                    short_commit_id.clone(),
                                                )
                                                .color(Color::Muted)
                                                .start_icon(
                                                    Icon::new(IconName::FileGit)
                                                        .size(IconSize::Small)
                                                        .color(Color::Muted),
                                                )
                                                .on_click(move |_, window, cx| {
                                                    CommitView::open(
                                                        commit_summary.sha.clone().into(),
                                                        repository.downgrade(),
                                                        workspace.clone(),
                                                        None,
                                                        None,
                                                        window,
                                                        cx,
                                                    );
                                                    cx.stop_propagation();
                                                }),
                                            )
                                            .child(Divider::vertical())
                                            .child(
                                                CopyButton::new("copy-blame-sha", sha.to_string())
                                                    .tooltip_label("Copy SHA"),
                                            ),
                                    ),
                            ),
                    )
            })
            .into_any_element(),
        )
    }

    fn open_blame_commit(
        &self,
        blame_entry: BlameEntry,
        repository: Entity<Repository>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        CommitView::open(
            blame_entry.sha.to_string(),
            repository.downgrade(),
            workspace,
            None,
            None,
            window,
            cx,
        )
    }
}

fn deploy_blame_entry_context_menu(
    blame_entry: &BlameEntry,
    details: Option<&ParsedCommitMessage>,
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
    position: gpui::Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    // S-ANN — reuse the S-CTM commit context menu so the right-click
    // experience on the blame gutter is identical to the Git Graph row
    // menu (Copy / New Branch / New Tag / Checkout / Compare / Show /
    // External / Destructive submenu / Patch).
    let sha: SharedString = blame_entry.sha.to_string().into();
    let subject: SharedString = details
        .and_then(|d| {
            d.message
                .split('\n')
                .next()
                .map(|s| s.trim_end().to_string())
        })
        .map(SharedString::from)
        .unwrap_or_default();
    let provider = repository.read(cx).default_remote_url().and_then(|url| {
        let registry = GitHostingProviderRegistry::default_global(cx);
        parse_git_remote_url(registry, &url)
            .map(|(provider, _)| (provider.name(), provider.base_url().to_string()))
    });
    let work_dir = Some(
        repository
            .read(cx)
            .work_directory_abs_path
            .as_ref()
            .to_path_buf(),
    );

    let ctx = CommitContext {
        workspace,
        repository,
        sha,
        subject,
        provider,
        work_dir,
        member_id: None,
        // The blame gutter has no ref-decoration info, so the
        // branches/tags section stays hidden here.
        refs: Vec::new(),
        head_branch: None,
        local_branches: Vec::new(),
    };
    let context_menu = build_commit_context_menu(ctx, window, cx);

    editor.update(cx, move |editor, cx| {
        editor.hide_blame_popover(false, cx);
        editor.deploy_mouse_context_menu(position, context_menu, window, cx);
        cx.notify();
    });
}

/// S-ANN — render a single muted dot for a line whose author is filtered
/// out. Keeps the gutter visually aligned without distracting the user.
fn render_muted_blame_entry(style: &TextStyle, ix: usize, cx: &mut App) -> AnyElement {
    h_flex()
        .id(("blame-muted", ix))
        .w_full()
        .font(style.font())
        .line_height(style.line_height)
        .text_color(cx.theme().colors().text_disabled)
        .child("·")
        .into_any()
}

fn blame_entry_absolute_timestamp(blame_entry: &BlameEntry) -> String {
    match blame_entry.author_offset_date_time() {
        Ok(timestamp) => {
            let local_offset =
                time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
            time_format::format_localized_timestamp(
                timestamp,
                time::OffsetDateTime::now_utc(),
                local_offset,
                time_format::TimestampFormat::MediumAbsolute,
            )
        }
        Err(_) => "Error parsing date".to_string(),
    }
}

fn blame_entry_relative_timestamp(blame_entry: &BlameEntry) -> String {
    match blame_entry.author_offset_date_time() {
        Ok(timestamp) => {
            let local_offset =
                time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
            time_format::format_localized_timestamp(
                timestamp,
                time::OffsetDateTime::now_utc(),
                local_offset,
                time_format::TimestampFormat::Relative,
            )
        }
        Err(_) => "Error parsing date".to_string(),
    }
}
