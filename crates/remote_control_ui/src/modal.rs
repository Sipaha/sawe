use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use editor::Editor;
use futures::AsyncReadExt as _;
use gpui::{
    Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyContext,
    ParentElement, Render, SharedString, Styled, Task, WeakEntity, Window, div, px,
};
use http_client::{AsyncBody, HttpClient};
use remote_control::{RemoteControlStore, RemoteControlStoreEvent};
use ui::{
    Button, ButtonStyle, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize, TintColor,
    Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::qr_popover::QrPopover;

/// Address-detection endpoint. The `/ip` path always returns the caller's
/// public IP as plain text, regardless of `User-Agent`. The bare host
/// (`https://ifconfig.me`) content-negotiates and returns an HTML page
/// to any non-curl-like UA — pasting that into the address field was a
/// real bug observed in the wild.
const DETECT_ADDRESS_ENDPOINT: &str = "https://ifconfig.me/ip";

/// Workspace modal for editing Remote Control settings: address / port,
/// on/off toggle, authorized-client list. R-1 surfaces the state model
/// + persistence; the listener arrives in R-2.
pub struct RemoteControlModal {
    workspace: WeakEntity<Workspace>,
    address_editor: Entity<Editor>,
    port_editor: Entity<Editor>,
    new_client_editor: Entity<Editor>,
    detect_task: Option<Task<()>>,
    /// Inline error displayed under the address row when "Detect" fails or
    /// the user-typed value is invalid. Cleared on successful re-edit.
    inline_error: Option<SharedString>,
    focus_handle: FocusHandle,
    _store_subscription: Option<gpui::Subscription>,
}

impl RemoteControlModal {
    pub fn toggle(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let weak = workspace.weak_handle();
        workspace.toggle_modal(window, cx, move |window, cx| {
            RemoteControlModal::new(weak, window, cx)
        });
    }

    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let address_editor = cx.new(|cx| Editor::single_line(window, cx));
        let port_editor = cx.new(|cx| Editor::single_line(window, cx));
        let new_client_editor = cx.new(|cx| Editor::single_line(window, cx));
        let focus_handle = cx.focus_handle();
        let store_subscription = RemoteControlStore::try_global(cx)
            .map(|store| cx.subscribe(&store, |_, _, _: &RemoteControlStoreEvent, cx| cx.notify()));
        let mut this = Self {
            workspace,
            address_editor,
            port_editor,
            new_client_editor,
            detect_task: None,
            inline_error: None,
            focus_handle,
            _store_subscription: store_subscription,
        };
        this.sync_inputs_from_store(window, cx);
        this
    }

    fn sync_inputs_from_store(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = RemoteControlStore::try_global(cx) else {
            return;
        };
        let (address, port) = store.read_with(cx, |store, _| {
            (
                store.settings().server_address.clone().unwrap_or_default(),
                store.settings().server_port,
            )
        });
        self.address_editor.update(cx, |editor, cx| {
            if editor.text(cx) != address {
                editor.set_text(address, window, cx);
            }
        });
        let port_text = port.to_string();
        self.port_editor.update(cx, |editor, cx| {
            if editor.text(cx) != port_text {
                editor.set_text(port_text, window, cx);
            }
        });
    }

    fn save_address_from_editor(&mut self, cx: &mut Context<Self>) {
        let Some(store) = RemoteControlStore::try_global(cx) else {
            return;
        };
        let text = self.address_editor.read(cx).text(cx);
        let next = if text.trim().is_empty() {
            None
        } else {
            Some(text)
        };
        store.update(cx, |store, cx| store.set_address(next, cx));
        self.inline_error = None;
    }

    fn save_port_from_editor(&mut self, cx: &mut Context<Self>) {
        let Some(store) = RemoteControlStore::try_global(cx) else {
            return;
        };
        let text = self.port_editor.read(cx).text(cx);
        match text.trim().parse::<u16>() {
            Ok(port) => {
                store.update(cx, |store, cx| store.set_port(port, cx));
                self.inline_error = None;
            }
            Err(err) => {
                self.inline_error = Some(format!("Port must be a number 0-65535: {err}").into());
            }
        }
    }

    fn toggle_enabled(&mut self, cx: &mut Context<Self>) {
        let Some(store) = RemoteControlStore::try_global(cx) else {
            return;
        };
        // Persist the latest values from the input rows before flipping
        // the bit — otherwise the user types an address, hits the toggle,
        // and the change is lost.
        self.save_address_from_editor(cx);
        self.save_port_from_editor(cx);
        if self.inline_error.is_some() {
            return;
        }
        let next = !store.read(cx).settings().enabled;
        // Block enabling when no address is set; the listener (R-2) needs
        // one to bind / advertise. R-1 enforces this in UI to prevent
        // future-broken state from leaking into the JSON file.
        if next && store.read(cx).settings().server_address.is_none() {
            self.inline_error = Some("Server address is required to enable.".into());
            return;
        }
        store.update(cx, |store, cx| store.set_enabled(next, cx));
    }

    fn detect_address(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.detect_task.is_some() {
            return;
        }
        let http_client = cx.http_client();
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = fetch_public_address(http_client).await;
            this.update_in(cx, |this, window, cx| {
                this.detect_task = None;
                match result {
                    Ok(address) => {
                        this.address_editor.update(cx, |editor, cx| {
                            editor.set_text(address.to_string(), window, cx);
                        });
                        if let Some(store) = RemoteControlStore::try_global(cx) {
                            store.update(cx, |store, cx| {
                                store.set_address(Some(address.to_string()), cx);
                            });
                        }
                        this.inline_error = None;
                        cx.notify();
                    }
                    Err(err) => {
                        log::warn!("remote_control: detect_address failed: {err:#}");
                        this.inline_error = Some(format!("Couldn't detect address: {err}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        });
        self.detect_task = Some(task);
        cx.notify();
    }

    fn add_client(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = RemoteControlStore::try_global(cx) else {
            return;
        };
        let name = self.new_client_editor.read(cx).text(cx);
        if name.trim().is_empty() {
            self.inline_error = Some("Enter a client name first.".into());
            return;
        }
        let result = store.update(cx, |store, cx| store.add_client(name, cx));
        match result {
            Ok(_client) => {
                self.new_client_editor.update(cx, |editor, cx| {
                    editor.clear(window, cx);
                });
                self.inline_error = None;
            }
            Err(err) => {
                self.inline_error = Some(format!("{err}").into());
            }
        }
    }

    fn show_qr_popover(
        &mut self,
        client_name: SharedString,
        secret_standard_base64: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The QR popover replaces this modal on the workspace's
        // single modal slot (the workspace only ever shows one modal
        // at a time — see `ModalLayer::show_modal`). `toggle_modal`
        // hides the currently-active modal as its first step, which
        // would re-enter THIS modal's entity update if we called it
        // synchronously from a listener — `EntityMap::lease` panics
        // with "cannot update RemoteControlModal while it is already
        // being updated". Defer via `window.defer` so the swap runs
        // after the click handler returns.
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let (address, port, server_fingerprint) = RemoteControlStore::try_global(cx)
            .map(|store| {
                store.read_with(cx, |store, _| {
                    (
                        store.settings().server_address.clone(),
                        store.settings().server_port,
                        store.cert_fingerprint(),
                    )
                })
            })
            .unwrap_or((None, 0, None));
        window.defer(cx, move |window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    QrPopover::new(
                        client_name,
                        secret_standard_base64,
                        address,
                        port,
                        server_fingerprint,
                        window,
                        cx,
                    )
                });
            });
        });
    }

    fn cancel(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }
}

async fn fetch_public_address(http_client: Arc<dyn HttpClient>) -> Result<SharedString> {
    let response = http_client
        .get(DETECT_ADDRESS_ENDPOINT, AsyncBody::default(), true)
        .await
        .with_context(|| format!("GET {DETECT_ADDRESS_ENDPOINT}"))?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "{DETECT_ADDRESS_ENDPOINT} returned {}",
            response.status()
        ));
    }
    let mut body = Vec::new();
    response
        .into_body()
        .read_to_end(&mut body)
        .await
        .context("reading response body")?;
    let text = String::from_utf8(body).context("response body not UTF-8")?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty response from {DETECT_ADDRESS_ENDPOINT}"));
    }
    Ok(SharedString::from(trimmed.to_string()))
}

impl EventEmitter<DismissEvent> for RemoteControlModal {}
impl ModalView for RemoteControlModal {}
impl Focusable for RemoteControlModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RemoteControlModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = RemoteControlStore::try_global(cx);
        // (name, secret_prefix_for_display, full_standard_base64_secret).
        // The full secret is needed by the QR popover; the prefix is the
        // muted label rendered next to the name in the clients list.
        let (enabled, address_set, clients): (
            bool,
            bool,
            Vec<(SharedString, SharedString, String)>,
        ) = match store.as_ref() {
            Some(store) => store.read_with(cx, |store, _| {
                let settings = store.settings();
                let clients: Vec<(SharedString, SharedString, String)> = settings
                    .clients
                    .iter()
                    .map(|client| {
                        let prefix: String = client.secret_base64.chars().take(16).collect();
                        let label = format!("{prefix}\u{2026}");
                        (
                            SharedString::from(client.name.clone()),
                            SharedString::from(label),
                            client.secret_base64.clone(),
                        )
                    })
                    .collect();
                (settings.enabled, settings.server_address.is_some(), clients)
            }),
            None => (false, false, Vec::new()),
        };

        let toggle_disabled = !address_set && !enabled;
        let toggle_style = if enabled {
            ButtonStyle::Tinted(TintColor::Success)
        } else {
            ButtonStyle::Subtle
        };
        let toggle_label = if enabled {
            "Enabled \u{2014} click to disable"
        } else if toggle_disabled {
            "Set a server address to enable"
        } else {
            "Disabled \u{2014} click to enable"
        };

        v_flex()
            .key_context({
                let mut key_context = KeyContext::new_with_defaults();
                key_context.add("RemoteControlModal");
                key_context
            })
            .track_focus(&self.focus_handle)
            .elevation_3(cx)
            .w(px(560.))
            .overflow_hidden()
            .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| this.cancel(window, cx)))
            .child(
                h_flex()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Icon::new(IconName::Server)
                            .size(IconSize::Small)
                            .color(if enabled {
                                Color::Success
                            } else {
                                Color::Muted
                            }),
                    )
                    .gap_2()
                    .child(Label::new("Remote Control").size(LabelSize::Large)),
            )
            .child(
                v_flex()
                    .p_3()
                    .gap_3()
                    .child(self.render_address_row(cx))
                    .child(self.render_port_row(cx))
                    .child(self.render_toggle_row(toggle_style, toggle_label, toggle_disabled, cx))
                    .when_some(self.inline_error.clone(), |this, err| {
                        this.child(Label::new(err).color(Color::Error).size(LabelSize::Small))
                    })
                    .child(div().h(px(1.)).bg(cx.theme().colors().border_variant))
                    .child(self.render_clients_section(&clients, cx)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        Button::new("remote-control-close", "Close")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    ),
            )
    }
}

impl RemoteControlModal {
    fn render_address_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let detect_busy = self.detect_task.is_some();
        v_flex()
            .gap_1()
            .child(Label::new("Server address").size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .border_1()
                            .rounded_md()
                            .border_color(cx.theme().colors().border)
                            .px_2()
                            .py_1()
                            .child(self.address_editor.clone()),
                    )
                    .child(
                        Button::new("remote-control-detect", "Detect")
                            .disabled(detect_busy)
                            .tooltip(Tooltip::text(
                                "Ask ifconfig.me for the public address of this machine.",
                            ))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.detect_address(window, cx)),
                            ),
                    )
                    .child(Button::new("remote-control-save-address", "Save").on_click(
                        cx.listener(|this, _, _window, cx| {
                            this.save_address_from_editor(cx);
                        }),
                    )),
            )
    }

    fn render_port_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(Label::new("Port").size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(120.))
                            .border_1()
                            .rounded_md()
                            .border_color(cx.theme().colors().border)
                            .px_2()
                            .py_1()
                            .child(self.port_editor.clone()),
                    )
                    .child(
                        Button::new("remote-control-save-port", "Save").on_click(cx.listener(
                            |this, _, _window, cx| {
                                this.save_port_from_editor(cx);
                            },
                        )),
                    ),
            )
    }

    fn render_toggle_row(
        &self,
        style: ButtonStyle,
        label: &'static str,
        disabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .gap_2()
            .child(Label::new("Status").size(LabelSize::Small))
            .child(
                Button::new("remote-control-toggle", label)
                    .style(style)
                    .disabled(disabled)
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_enabled(cx))),
            )
    }

    fn render_clients_section(
        &self,
        clients: &[(SharedString, SharedString, String)],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut section = v_flex()
            .gap_2()
            .child(Label::new("Authorized clients").size(LabelSize::Small));

        if clients.is_empty() {
            section = section.child(
                Label::new("No clients yet \u{2014} add one below.")
                    .color(Color::Muted)
                    .size(LabelSize::Small),
            );
        } else {
            for (index, (name, secret_prefix, full_secret)) in clients.iter().enumerate() {
                let name_for_btn = name.clone();
                let secret_for_btn = full_secret.clone();
                section = section.child(
                    h_flex()
                        .gap_3()
                        .child(Label::new(name.clone()).size(LabelSize::Default))
                        .child(
                            Label::new(secret_prefix.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            IconButton::new(("remote-control-show-qr", index), IconName::Maximize)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Show QR"))
                                .on_click(cx.listener({
                                    let name = name_for_btn.clone();
                                    let secret = secret_for_btn.clone();
                                    move |this, _, window, cx| {
                                        this.show_qr_popover(
                                            name.clone(),
                                            secret.clone(),
                                            window,
                                            cx,
                                        );
                                    }
                                })),
                        ),
                );
            }
        }

        section.child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .border_1()
                        .rounded_md()
                        .border_color(cx.theme().colors().border)
                        .px_2()
                        .py_1()
                        .child(self.new_client_editor.clone()),
                )
                .child(
                    Button::new("remote-control-add-client", "Add client")
                        .style(ButtonStyle::Tinted(TintColor::Accent))
                        .on_click(cx.listener(|this, _, window, cx| this.add_client(window, cx))),
                ),
        )
    }
}
