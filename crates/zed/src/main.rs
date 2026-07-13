// Disable command line from opening on release mode
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod notification_focus;
mod reliability;
mod zed;

// NOTE: this fork routes all runtime dirs through `paths::base_dir()` =
// `~/.spk/<dir_name_kebab()>` rather than upstream's `APP_NAME_LOWERCASE`
// scheme, so the upstream binary-name/APP_NAME assertion does not apply here.

use agent::{SharedThread, ThreadStore};
use agent_client_protocol::schema as acp;
use agent_ui::AgentPanel;
use anyhow::{Context as _, Result};
use clap::Parser;
use cli::FORCE_CLI_MODE_ENV_VAR_NAME;
use client::{Client, ProxySettings, RefreshLlmTokenListener, UserStore, parse_zed_link};
use collab_ui::channel_view::ChannelView;
use collections::HashMap;
use crashes::InitCrashHandler;
use db::kvp::{GlobalKeyValueStore, KeyValueStore};
use editor::Editor;
use extension::ExtensionHostProxy;
use fs::{Fs, RealFs};
use futures::{StreamExt, channel::oneshot, future};
use git::GitHostingProviderRegistry;
use git_ui::clone::clone_and_open;
use gpui::{
    App, AppContext, Application, AsyncApp, Focusable as _, QuitMode, Task, TaskExt,
    UpdateGlobal as _, block_on,
};
use gpui_platform;

use gpui_tokio::Tokio;
use language::LanguageRegistry;
use onboarding::{FIRST_OPEN, show_onboarding_view};
use project_panel::ProjectPanel;
use prompt_store::PromptBuilder;
use remote::RemoteConnectionOptions;
use reqwest_client::ReqwestClient;

use assets::Assets;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use parking_lot::Mutex;
use project::{project_settings::ProjectSettings, trusted_worktrees};
use proto;
use recent_projects::{RemoteSettings, open_remote_project};
use release_channel::{AppCommitSha, AppVersion, ReleaseChannel};
use session::{AppSession, Session};
use settings::{BaseKeymap, Settings, SettingsStore, watch_config_file};
use smol::future::poll_once;
use std::{
    cell::RefCell,
    env,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process,
    rc::Rc,
    sync::{Arc, LazyLock, OnceLock},
    time::Instant,
};
use theme::{ActiveTheme, GlobalTheme, ThemeRegistry};
use theme_settings::load_user_theme;
use util::{ResultExt, TryFutureExt, maybe};
use uuid::Uuid;
use workspace::{
    AppState, MultiWorkspace, SerializedWorkspaceLocation, SessionWorkspace, Toast,
    WorkspaceSettings, WorkspaceStore, notifications::NotificationId, restore_multiworkspace,
};
use zed::{
    OpenListener, OpenRequest, RawOpenRequest, app_menus, build_window_options,
    derive_paths_with_position, edit_prediction_registry, handle_cli_connection,
    handle_keymap_file_changes, initialize_workspace, open_paths_with_positions,
};

use crate::zed::{CrashHandler, OpenRequestKind, eager_load_active_theme_and_icon_theme};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn build_application(headless: bool) -> Application {
    let platform = gpui_platform::current_platform(headless);
    if std::env::var("ZED_EXPERIMENTAL_A11Y").as_deref() == Ok("1") {
        Application::with_platform(platform)
    } else {
        Application::new_inaccessible(platform)
    }
}

fn files_not_created_on_launch(errors: HashMap<io::ErrorKind, Vec<&Path>>, headless: bool) {
    let message = "Sawe failed to launch";
    let error_details = errors
        .into_iter()
        .flat_map(|(kind, paths)| {
            #[allow(unused_mut)] // for non-unix platforms
            let mut error_kind_details = match paths.len() {
                0 => return None,
                1 => format!(
                    "{kind} when creating directory {:?}",
                    paths.first().expect("match arm checks for a single entry")
                ),
                _many => format!("{kind} when creating directories {paths:?}"),
            };

            #[cfg(unix)]
            {
                if kind == io::ErrorKind::PermissionDenied {
                    error_kind_details.push_str("\n\nConsider using chown and chmod tools for altering the directories permissions if your user has corresponding rights.\
                        \nFor example, `sudo chown $(whoami):staff ~/.config` and `chmod +uwrx ~/.config`");
                }
            }

            Some(error_kind_details)
        })
        .collect::<Vec<_>>().join("\n\n");

    eprintln!("{message}: {error_details}");
    // In headless mode there's no display to put a prompt window on, so the
    // launch-failure path just printed-and-exits. The full prompt path stays
    // on the on-screen platform.
    if headless {
        return;
    }
    build_application(false)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            if let Ok(window) = cx.open_window(gpui::WindowOptions::default(), |_, cx| {
                cx.new(|_| gpui::Empty)
            }) {
                window
                    .update(cx, |_, window, cx| {
                        let response = window.prompt(
                            gpui::PromptLevel::Critical,
                            message,
                            Some(&error_details),
                            &["Exit"],
                            cx,
                        );

                        cx.spawn_in(window, async move |_, cx| {
                            response.await?;
                            cx.update(|_, cx| cx.quit())
                        })
                        .detach_and_log_err(cx);
                    })
                    .log_err();
            } else {
                fail_to_open_window(anyhow::anyhow!("{message}: {error_details}"), cx)
            }
        })
}

fn fail_to_open_window_async(e: anyhow::Error, cx: &mut AsyncApp) {
    cx.update(|cx| fail_to_open_window(e, cx));
}

fn fail_to_open_window(e: anyhow::Error, _cx: &mut App) {
    eprintln!(
        "Sawe failed to open a window: {e:?}. See https://github.com/Sipaha/sawe for troubleshooting."
    );
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    {
        process::exit(1);
    }

    // Maybe unify this with gpui::platform::linux::platform::ResultExt::notify_err(..)?
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        use ashpd::desktop::notification::{Notification, NotificationProxy, Priority};
        _cx.spawn(async move |_cx| {
            let Ok(proxy) = NotificationProxy::new().await else {
                process::exit(1);
            };

            let notification_id = "dev.zed.Oops";
            proxy
                .add_notification(
                    notification_id,
                    Notification::new("Sawe failed to launch")
                        .body(Some(
                            format!(
                                "{e:?}. See https://zed.dev/docs/linux for troubleshooting steps."
                            )
                            .as_str(),
                        ))
                        .priority(Priority::High)
                        .icon(ashpd::desktop::Icon::with_names(&[
                            "dialog-question-symbolic",
                        ])),
                )
                .await
                .ok();

            process::exit(1);
        })
        .detach();
    }
}
static STARTUP_TIME: OnceLock<Instant> = OnceLock::new();

fn main() {
    STARTUP_TIME.get_or_init(|| Instant::now());

    #[cfg(unix)]
    util::prevent_root_execution();

    let args = Args::parse();

    // `zed --askpass` Makes zed operate in nc/netcat mode for use with askpass
    #[cfg(not(target_os = "windows"))]
    if let Some(socket) = &args.askpass {
        askpass::main(socket);
        return;
    }

    // `zed --crash-handler` Makes zed operate in minidump crash handler mode
    if let Some(socket) = &args.crash_handler {
        crashes::crash_server(socket.as_path(), paths::logs_dir().clone());
        return;
    }

    #[cfg(target_os = "windows")]
    if args.record_etw_trace {
        let zed_pid = args
            .etw_zed_pid
            .and_then(|pid| if pid >= 0 { Some(pid as u32) } else { None });
        let Some(output_path) = args.etw_output else {
            eprintln!("--etw-output is required for --record-etw-trace");
            process::exit(1);
        };

        let Some(etw_socket) = args.etw_socket else {
            eprintln!("--etw-socket is required for --record-etw-trace");
            process::exit(1);
        };

        if let Err(error) =
            etw_tracing::record_etw_trace(zed_pid, &output_path, etw_socket.as_str())
        {
            eprintln!("ETW trace recording failed: {error:#}");
            process::exit(1);
        }
        return;
    }

    // `zed --nc` Makes zed operate in nc/netcat mode for use with MCP
    if let Some(socket) = &args.nc {
        match nc::main(socket) {
            Ok(()) => return,
            Err(err) => {
                eprintln!("Error: {}", err);
                process::exit(1);
            }
        }
    }

    // `sawe --worktree-hook {create|remove}` runs as claude's WorktreeCreate /
    // WorktreeRemove hook. Like `--nc`, it must exit before any GPUI / paths
    // init and before the single-instance handoff — claude spawns it per
    // worktree, a cold editor boot would add seconds to every subagent launch,
    // and handing these args to a running editor would mean the hook never runs
    // at all.
    if let Some(mode) = &args.worktree_hook {
        let Some(base) = args.worktree_base.clone() else {
            eprintln!("--worktree-hook requires --worktree-base");
            process::exit(1);
        };
        let mut stdin = io::stdin().lock();
        let mut stdout = io::stdout().lock();
        match claude_native::worktree_hook::main(mode, &base, &mut stdin, &mut stdout) {
            Ok(()) => process::exit(0),
            Err(err) => {
                eprintln!("--worktree-hook: {err:#}");
                process::exit(1);
            }
        }
    }

    // `sawe --git-rebase-helper <todo-path>` runs as `GIT_SEQUENCE_EDITOR`
    // during programmatic interactive rebase (S-RBL). The implementation lives
    // in `git::operations::helpers` so it can be exercised by unit tests
    // without GPUI init. Exits directly without booting the editor — `git`
    // invokes the helper many times per rebase and a heavyweight cold start
    // would noticeably slow down each pause.
    if let Some(todo_path) = &args.git_rebase_helper {
        match git::operations::helpers::rebase_helper_main(todo_path) {
            Ok(()) => process::exit(0),
            Err(err) => {
                eprintln!("--git-rebase-helper: {err}");
                process::exit(1);
            }
        }
    }

    // `sawe --git-message-set <token>` runs as an `exec` step inside an
    // interactive rebase to swap in a pre-staged commit message via
    // `git commit --amend -F`.
    if let Some(token) = &args.git_message_set {
        match git::operations::helpers::message_set_main(token) {
            Ok(()) => process::exit(0),
            Err(err) => {
                eprintln!("--git-message-set: {err}");
                process::exit(1);
            }
        }
    }

    #[cfg(all(not(debug_assertions), target_os = "windows"))]
    unsafe {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};

        if args.foreground {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }

    // `zed --printenv` Outputs environment variables as JSON to stdout
    if args.printenv {
        util::shell_env::print_env();
        return;
    }

    if args.dump_all_actions {
        dump_all_gpui_actions();
        return;
    }

    // Set custom data directory.
    if let Some(dir) = &args.user_data_dir {
        paths::set_custom_data_dir(dir);
    }

    #[cfg(target_os = "windows")]
    match util::get_zed_cli_path() {
        Ok(path) => askpass::set_askpass_program(path),
        Err(err) => {
            eprintln!("Error: {}", err);
            if std::option_env!("ZED_BUNDLE").is_some() {
                process::exit(1);
            }
        }
    }

    let file_errors = init_paths();
    if !file_errors.is_empty() {
        files_not_created_on_launch(file_errors, args.headless);
        return;
    }

    zlog::init();

    // Best-effort cleanup of stale `git rebase` helper session directories
    // (P-11 § orphan cleanup, S-RBL). Sessions are short-lived; anything
    // older than an hour belongs to a crashed previous run.
    git::operations::rebase::cleanup_orphan_sessions();

    // Best-effort cleanup of stale snapshot worktrees from previous
    // editor runs that crashed before the on-close handler could run
    // (S-SAR § orphan cleanup). The TTL is configurable via
    // `git_panel.show_at_revision.cleanup_orphans_older_than_h` —
    // we use the default here because the settings store isn't
    // initialised yet at this point in main; the user-facing setting
    // applies on the *next* startup after the user changes it, which
    // matches the existing pattern for the rebase helper cleanup.
    git_ui::handlers::show_at_revision::cleanup_orphan_worktrees(
        git_ui::handlers::show_at_revision::DEFAULT_CLEANUP_ORPHANS_OLDER_THAN_H,
    );

    // Best-effort cleanup of expired commit-explanation cache files
    // (S-AI-EXP). The settings store is not yet initialised at this
    // point in main, so we use the compile-time default. Anything
    // older than `2 * cache_ttl_days` is removed; live reads still
    // gate stale entries against the user-configured TTL.
    git_ui::commit_view::ai_explain::cleanup_expired(
        git_ui::commit_view::ai_explain::DEFAULT_CACHE_TTL_DAYS,
    );

    if stdout_is_a_pty() {
        zlog::init_output_stdout();
    } else {
        let result = zlog::init_output_file(paths::log_file(), Some(paths::old_log_file()));
        if let Err(err) = result {
            eprintln!("Could not open log file: {}... Defaulting to stdout", err);
            zlog::init_output_stdout();
        };
    }
    ztracing::init();

    let version = option_env!("ZED_BUILD_ID");
    let app_commit_sha =
        option_env!("ZED_COMMIT_SHA").map(|commit_sha| AppCommitSha::new(commit_sha.to_string()));
    let app_version = AppVersion::load(env!("CARGO_PKG_VERSION"), version, app_commit_sha.clone());

    if args.system_specs {
        let system_specs = system_specs::SystemSpecs::new_stateless(
            app_version,
            app_commit_sha,
            *release_channel::RELEASE_CHANNEL,
            client::telemetry::os_name(),
            client::telemetry::os_version(),
        );
        println!("Sawe System Specs (from CLI):\n{}", system_specs);
        return;
    }

    rayon::ThreadPoolBuilder::new()
        .num_threads(std::thread::available_parallelism().map_or(1, |n| n.get().div_ceil(2)))
        .stack_size(10 * 1024 * 1024)
        .thread_name(|ix| format!("RayonWorker{}", ix))
        .build_global()
        .unwrap();

    log::info!(
        "========== starting zed version {}, sha {} ==========",
        app_version,
        app_commit_sha
            .as_ref()
            .map(|sha| sha.short())
            .as_deref()
            .unwrap_or("unknown"),
    );

    // Single-instance handoff: if another sawe is already running and
    // its MCP server is reachable, hand off our CLI paths to it and exit.
    // Otherwise we continue and become the canonical instance (later we'll
    // bind the MCP server in `editor_mcp::start_server`).
    let handoff_paths: Vec<PathBuf> = args
        .paths_or_urls
        .iter()
        .filter_map(|arg| {
            if is_url_scheme(arg) {
                None
            } else {
                Some(PathBuf::from(arg))
            }
        })
        .collect();
    match editor_mcp::try_handoff_to_existing_instance(handoff_paths) {
        Ok(editor_mcp::HandoffOutcome::HandedOff { focused_window_id }) => {
            log::info!(
                "sawe: handed off to existing instance (window: {:?})",
                focused_window_id
            );
            return;
        }
        Ok(editor_mcp::HandoffOutcome::LockBusyButUnreachable { lockholder_pid }) => {
            eprintln!(
                "Another sawe instance is starting (lock held by PID {:?}); please wait or terminate it.",
                lockholder_pid
            );
            process::exit(1);
        }
        Ok(editor_mcp::HandoffOutcome::BecameCanonical) => {
            // Continue normal startup; we'll bind the MCP server later.
        }
        Err(err) => {
            log::warn!("sawe: handoff probe failed: {err}; continuing as canonical");
        }
    }

    #[cfg(windows)]
    check_for_conpty_dll();

    let app = build_application(args.headless).with_assets(Assets);

    let app_db = db::AppDatabase::new();
    let system_id = app.background_executor().spawn(system_id());
    let installation_id = app
        .background_executor()
        .spawn(installation_id(KeyValueStore::from_app_db(&app_db)));
    let session_id = Uuid::new_v4().to_string();
    let session = app.background_executor().spawn(Session::new(
        session_id.clone(),
        KeyValueStore::from_app_db(&app_db),
    ));
    let background_executor = app.background_executor();

    let (open_listener, mut open_rx) = OpenListener::new();

    let failed_single_instance_check = if *zed_env_vars::ZED_STATELESS
        || *release_channel::RELEASE_CHANNEL == ReleaseChannel::Dev
    {
        false
    } else {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            crate::zed::listen_for_cli_connections(open_listener.clone()).is_err()
        }

        #[cfg(target_os = "windows")]
        {
            !crate::zed::windows_only_instance::handle_single_instance(open_listener.clone(), &args)
        }

        #[cfg(target_os = "macos")]
        {
            use zed::mac_only_instance::*;
            ensure_only_instance() != IsOnlyInstance::Yes
        }
    };
    if failed_single_instance_check {
        println!("sawe is already running");
        return;
    }

    let should_install_crash_handler = matches!(
        env::var("ZED_GENERATE_MINIDUMPS").as_deref(),
        Ok("true" | "1")
    ) || *release_channel::RELEASE_CHANNEL
        != ReleaseChannel::Dev;

    let crash_handler = if should_install_crash_handler {
        Some(
            app.background_executor().spawn(crashes::init(
                InitCrashHandler {
                    session_id,
                    // strip the build and channel information from the version string, we send them separately
                    zed_version: semver::Version::new(
                        app_version.major,
                        app_version.minor,
                        app_version.patch,
                    )
                    .to_string(),
                    binary: "zed".to_string(),
                    release_channel: release_channel::RELEASE_CHANNEL_NAME.clone(),
                    commit_sha: app_commit_sha
                        .as_ref()
                        .map(|sha| sha.full())
                        .unwrap_or_else(|| "no sha".to_owned()),
                },
                {
                    let background_executor1 = app.background_executor();
                    move |task| {
                        background_executor1.spawn(task).detach();
                    }
                },
                |pid| paths::temp_dir().join(format!("zed-crash-handler-{pid}")),
                move |duration| background_executor.timer(duration),
            )),
        )
    } else {
        crashes::force_backtrace();
        None
    };

    let git_hosting_provider_registry = Arc::new(GitHostingProviderRegistry::new());
    let git_binary_path =
        if cfg!(target_os = "macos") && option_env!("ZED_BUNDLE").as_deref() == Some("true") {
            app.path_for_auxiliary_executable("git")
                .context("could not find git binary path")
                .log_err()
        } else {
            None
        };
    if let Some(git_binary_path) = &git_binary_path {
        log::info!("Using git binary path: {:?}", git_binary_path);
    }

    let fs = Arc::new(RealFs::new(git_binary_path, app.background_executor()));
    let (user_keymap_file_rx, user_keymap_watcher) = watch_config_file(
        &app.background_executor(),
        fs.clone(),
        paths::keymap_file().clone(),
    );

    let (shell_env_loaded_tx, shell_env_loaded_rx) = oneshot::channel();
    if !stdout_is_a_pty() {
        app.background_executor()
            .spawn(async {
                #[cfg(unix)]
                util::load_login_shell_environment().await.log_err();
                shell_env_loaded_tx.send(()).ok();
            })
            .detach();
    } else {
        drop(shell_env_loaded_tx)
    }

    app.on_open_urls({
        let open_listener = open_listener.clone();
        move |urls| {
            open_listener.open(RawOpenRequest {
                urls,
                diff_paths: Vec::new(),
                ..Default::default()
            })
        }
    });
    app.on_reopen(move |cx| {
        if let Some(app_state) = AppState::try_global(cx) {
            cx.spawn({
                async move |cx| {
                    if let Err(e) = restore_or_create_workspace(app_state, cx).await {
                        fail_to_open_window_async(e, cx)
                    }
                }
            })
            .detach();
        }
    });

    app.run(move |cx| {
        cx.set_global(app_db);
        // Cache `ThreadSafeConnection` handles for sync stores that don't have
        // `cx` at their call sites (background-thread git operations,
        // pre-commit checks, etc.). Each call clones an `Arc`-backed handle
        // out of the per-App `AppDatabase` global into a module-local
        // `OnceLock` so the public sync APIs (`record`, `list`, etc.) can
        // resolve a connection without taking `&App`.
        git::undo_registry::init(cx);
        git::operations::shelf::init(cx);
        git_ui::branch_picker::favorites::init(cx);
        git_ui::pre_commit::init(cx);
        let db_trusted_paths = match workspace::WorkspaceDb::global(cx).fetch_trusted_worktrees() {
            Ok(trusted_paths) => trusted_paths,
            Err(e) => {
                log::error!("Failed to do initial trusted worktrees fetch: {e:#}");
                HashMap::default()
            }
        };
        trusted_worktrees::init(db_trusted_paths, cx);
        menu::init();
        zed_actions::init();

        release_channel::init(app_version, cx);
        gpui_tokio::init(cx);
        if let Some(app_commit_sha) = app_commit_sha {
            AppCommitSha::set_global(app_commit_sha, cx);
        }
        settings::init(cx);
        zlog_settings::init(cx);
        zed::watch_settings_files(fs.clone(), cx);
        handle_keymap_file_changes(user_keymap_file_rx, user_keymap_watcher, cx);

        let user_agent = format!(
            "SPK-Editor/{} ({}; {})",
            AppVersion::global(cx),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let proxy_url = ProxySettings::get_global(cx).proxy_url();
        let http = {
            let _guard = Tokio::handle(cx).enter();

            ReqwestClient::proxy_and_user_agent(proxy_url, &user_agent)
                .expect("could not start HTTP client")
        };
        cx.set_http_client(Arc::new(http));

        <dyn Fs>::set_global(fs.clone(), cx);

        GitHostingProviderRegistry::set_global(git_hosting_provider_registry, cx);
        git_hosting_providers::init(cx);

        OpenListener::set_global(cx, open_listener.clone());

        extension::init(cx);
        let extension_host_proxy = ExtensionHostProxy::global(cx);

        let client = Client::production(cx);
        cx.set_http_client(client.http_client());
        let mut languages = LanguageRegistry::new(cx.background_executor().clone());
        languages.set_language_server_download_dir(paths::languages_dir().clone());
        let languages = Arc::new(languages);
        let (mut tx, rx) = watch::channel(None);
        cx.observe_global::<SettingsStore>(move |cx| {
            let settings = &ProjectSettings::get_global(cx).node;
            let options = NodeBinaryOptions {
                allow_path_lookup: !settings.ignore_system_version,
                // TODO: Expose this setting
                allow_binary_download: true,
                use_paths: settings.path.as_ref().map(|node_path| {
                    let node_path = PathBuf::from(shellexpand::tilde(node_path).as_ref());
                    let npm_path = settings
                        .npm_path
                        .as_ref()
                        .map(|path| PathBuf::from(shellexpand::tilde(&path).as_ref()));
                    (
                        node_path.clone(),
                        npm_path.unwrap_or_else(|| {
                            let base_path = PathBuf::new();
                            node_path.parent().unwrap_or(&base_path).join("npm")
                        }),
                    )
                }),
            };
            tx.send(Some(options)).log_err();
        })
        .detach();
        ui::on_new_scrollbars::<SettingsStore>(cx);

        let node_runtime = NodeRuntime::new(client.http_client(), Some(shell_env_loaded_rx), rx);

        debug_adapter_extension::init(extension_host_proxy.clone(), cx);
        languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);
        let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
        let workspace_store = cx.new(|cx| WorkspaceStore::new(client.clone(), cx));

        language_extension::init(
            language_extension::LspAccess::ViaWorkspaces({
                let workspace_store = workspace_store.clone();
                Arc::new(move |cx: &mut App| {
                    workspace_store.update(cx, |workspace_store, cx| {
                        Ok(workspace_store
                            .workspaces()
                            .filter_map(|weak| weak.upgrade())
                            .map(|workspace: gpui::Entity<workspace::Workspace>| {
                                workspace.read(cx).project().read(cx).lsp_store()
                            })
                            .collect())
                    })
                })
            }),
            extension_host_proxy.clone(),
            languages.clone(),
        );

        Client::set_global(client.clone(), cx);

        zed::init(cx);
        #[cfg(target_os = "macos")]
        zed::move_to_applications::init(cx);
        project::Project::init(&client, cx);
        debugger_ui::init(cx);
        debugger_tools::init(cx);
        client::init(&client, cx);
        feature_flags::FeatureFlagStore::init(cx);

        let system_id = cx.foreground_executor().block_on(system_id).ok();
        let installation_id = cx.foreground_executor().block_on(installation_id).ok();
        let session = cx.foreground_executor().block_on(session);

        let telemetry = client.telemetry();
        telemetry.start(
            system_id.as_ref().map(|id| id.to_string()),
            installation_id.as_ref().map(|id| id.to_string()),
            session.id().to_owned(),
            cx,
        );
        cx.subscribe(&user_store, {
            let telemetry = telemetry.clone();
            move |_, evt: &client::user::Event, cx| match evt {
                client::user::Event::PrivateUserInfoUpdated => {
                    if let Some(crash_client) = cx.try_global::<CrashHandler>() {
                        crashes::set_user_info(
                            &crash_client.0,
                            crashes::UserInfo {
                                metrics_id: telemetry.metrics_id().map(|s| s.to_string()),
                                is_staff: telemetry.is_staff(),
                            },
                        );
                    }
                }
                _ => {}
            }
        })
        .detach();

        let is_new_install = matches!(&installation_id, Some(IdType::New(_)));

        // We should rename these in the future to `first app open`, `first app open for release channel`, and `app open`
        if let (Some(system_id), Some(installation_id)) = (&system_id, &installation_id) {
            match (&system_id, &installation_id) {
                (IdType::New(_), IdType::New(_)) => {
                    telemetry::event!("App First Opened");
                    telemetry::event!("App First Opened For Release Channel");
                }
                (IdType::Existing(_), IdType::New(_)) => {
                    telemetry::event!("App First Opened For Release Channel");
                }
                (_, IdType::Existing(_)) => {
                    telemetry::event!("App Opened");
                }
            }
        }
        let app_session = cx.new(|cx| AppSession::new(session, cx));

        let app_state = Arc::new(AppState {
            languages,
            client: client.clone(),
            user_store,
            fs: fs.clone(),
            build_window_options,
            workspace_store,
            node_runtime,
            session: app_session,
        });
        AppState::set_global(app_state.clone(), cx);

        // Auto-update disabled in sawe: no upstream channel, builds from source.
        // auto_update::init(client.clone(), cx);
        dap_adapters::init(cx);
        // Auto-update UI disabled in sawe: no upstream channel, builds from source.
        // auto_update_ui::init(cx);
        reliability::init(client.clone(), cx);
        extension_host::init(
            extension_host_proxy.clone(),
            app_state.fs.clone(),
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            cx,
        );

        theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);
        eager_load_active_theme_and_icon_theme(fs.clone(), cx);
        theme_extension::init(
            extension_host_proxy,
            ThemeRegistry::global(cx),
            cx.background_executor().clone(),
        );
        command_palette::init(cx);
        solutions::init(cx);
        editor_mcp::init(cx);
        solution_agent::init(cx);
        workspace_events::init(cx);
        solution_git::init(cx);
        solutions_ui::init(cx);
        run_config::init(cx);
        run_config_ui::init(cx);
        remote_control::init(cx);
        // Wire solution_agent's binary-frame dispatch into remote_control's
        // listener — this is the third-party site where both crates'
        // public surfaces meet so neither has to dep on the other. The
        // listener fires this callback on every authenticated WS binary
        // frame; the closure parses the 16-byte upload header + writes
        // to UploadManager. See remote_control::BinaryFrameHandler and
        // docs/plans/2026-05-19-chunked-upload-binary-frames.md.
        remote_control::set_binary_frame_handler(std::sync::Arc::new(|bytes: &[u8]| {
            solution_agent::upload::dispatch_binary_frame(bytes)
        }));
        remote_control_ui::init(cx);
        git_conflict_ui::init(cx);
        let copilot_chat_configuration = copilot_chat::CopilotChatConfiguration {
            enterprise_uri: language::language_settings::all_language_settings(None, cx)
                .edit_predictions
                .copilot
                .enterprise_uri
                .clone(),
        };
        copilot_chat::init(
            app_state.fs.clone(),
            app_state.client.http_client(),
            copilot_chat_configuration,
            cx,
        );

        copilot_ui::init(&app_state, cx);
        language_model::init(cx);
        RefreshLlmTokenListener::register(
            app_state.client.clone(),
            app_state.user_store.clone(),
            cx,
        );
        language_models::init(app_state.user_store.clone(), app_state.client.clone(), cx);
        acp_tools::init(cx);
        zed::telemetry_log::init(cx);
        zed::remote_debug::init(cx);
        edit_prediction_ui::init(cx);
        web_search::init(cx);
        web_search_providers::init(app_state.client.clone(), app_state.user_store.clone(), cx);
        snippet_provider::init(cx);
        edit_prediction_registry::init(app_state.client.clone(), app_state.user_store.clone(), cx);
        let prompt_builder = PromptBuilder::load(app_state.fs.clone(), stdout_is_a_pty(), cx);
        project::AgentRegistryStore::init_global(
            cx,
            app_state.fs.clone(),
            app_state.client.http_client(),
        );
        agent_ui::init(
            app_state.fs.clone(),
            prompt_builder,
            app_state.languages.clone(),
            is_new_install,
            false,
            cx,
        );
        zed::watch_user_agents_md(app_state.fs.clone(), cx);

        repl::init(app_state.fs.clone(), cx);
        recent_projects::init(cx);
        dev_container::init(cx);

        load_embedded_fonts(cx);

        editor::init(cx);
        image_viewer::init(cx);
        repl::notebook::init(cx);
        diagnostics::init(cx);

        audio::init(cx);
        workspace::init(app_state.clone(), cx);
        ui_prompt::init(cx);

        go_to_line::init(cx);
        file_finder::init(cx);
        tab_switcher::init(cx);
        outline::init(cx);
        project_symbols::init(cx);
        project_panel::init(cx);
        outline_panel::init(cx);
        tasks_ui::init(cx);
        snippets_ui::init(cx);
        channel::init(&app_state.client.clone(), app_state.user_store.clone(), cx);
        search::init(cx);
        cx.set_global(workspace::PaneSearchBarCallbacks {
            setup_search_bar: |languages, toolbar, window, cx| {
                let search_bar = cx.new(|cx| search::BufferSearchBar::new(languages, window, cx));
                toolbar.update(cx, |toolbar, cx| {
                    toolbar.add_item(search_bar, window, cx);
                });
            },
            wrap_div_with_search_actions: search::buffer_search::register_pane_search_actions,
        });
        vim::init(cx);
        terminal_view::init(cx);
        console_panel::init(cx);
        notification_focus::init(cx);
        journal::init(app_state.clone(), cx);
        encoding_selector::init(cx);
        language_selector::init(cx);
        line_ending_selector::init(cx);
        toolchain_selector::init(cx);
        theme_selector::init(cx);
        settings_profile_selector::init(cx);
        language_tools::init(cx);
        call::init(app_state.client.clone(), app_state.user_store.clone(), cx);
        notifications::init(app_state.client.clone(), app_state.user_store.clone(), cx);
        // Collab is disabled in sawe (no Zed Industries collab server access).
        // collab_ui::init(&app_state, cx);
        // title_bar::init was nested inside collab_ui::init upstream; call it directly
        // since it has nothing to do with collab and is required for window decorations.
        title_bar::init(cx);
        git_ui::init(cx);
        // git_graph was extracted into its own crate during the re-fork; its
        // init (serializable-item + `git_graph::Open` action renderer
        // registration) must run for the "Open Git Graph" affordances in
        // git_ui to actually open the tab. Missing this call is why the graph
        // silently didn't open.
        git_graph::init(cx);
        feedback::init(cx);
        markdown_preview::init(cx);
        csv_preview::init(cx);
        svg_preview::init(cx);
        onboarding::init(cx);
        settings_ui::init(cx);
        keymap_editor::init(cx);
        extensions_ui::init(cx);
        edit_prediction::init(cx);
        inspector_ui::init(app_state.clone(), cx);
        json_schema_store::init(cx);
        miniprofiler_ui::init(*STARTUP_TIME.get().unwrap(), cx);
        which_key::init(cx);
        #[cfg(target_os = "windows")]
        etw_tracing::init(cx);

        cx.observe_global::<SettingsStore>({
            let http = app_state.client.http_client();
            let client = app_state.client.clone();
            move |cx| {
                for &mut window in cx.windows().iter_mut() {
                    let background_appearance = cx.theme().window_background_appearance();
                    window
                        .update(cx, |_, window, _| {
                            window.set_background_appearance(background_appearance)
                        })
                        .ok();
                }

                cx.set_text_rendering_mode(
                    match WorkspaceSettings::get_global(cx).text_rendering_mode {
                        settings::TextRenderingMode::PlatformDefault => {
                            gpui::TextRenderingMode::PlatformDefault
                        }
                        settings::TextRenderingMode::Subpixel => gpui::TextRenderingMode::Subpixel,
                        settings::TextRenderingMode::Grayscale => {
                            gpui::TextRenderingMode::Grayscale
                        }
                    },
                );

                let new_host = &client::ClientSettings::get_global(cx).server_url;
                if &http.base_url() != new_host {
                    http.set_base_url(new_host);
                    if client.status().borrow().is_connected() {
                        client.reconnect(&cx.to_async());
                    }
                }
            }
        })
        .detach();
        app_state.languages.set_theme(cx.theme().clone());
        cx.observe_global::<GlobalTheme>({
            let languages = app_state.languages.clone();
            move |cx| {
                languages.set_theme(cx.theme().clone());
            }
        })
        .detach();
        telemetry::event!(
            "Settings Changed",
            setting = "theme",
            value = cx.theme().name.to_string()
        );
        telemetry::event!(
            "Settings Changed",
            setting = "keymap",
            value = BaseKeymap::get_global(cx).to_string()
        );
        telemetry.flush_events().detach();

        let fs = app_state.fs.clone();
        load_user_themes_in_background(fs.clone(), cx);
        watch_themes(fs.clone(), cx);
        #[cfg(debug_assertions)]
        watch_languages(fs.clone(), app_state.languages.clone(), cx);

        let menus = app_menus(cx);
        cx.set_menus(menus);

        if let Some(mut crash_handler) = crash_handler {
            let crash_handler2 = block_on(poll_once(&mut crash_handler));
            match crash_handler2 {
                Some(crash_handler) => {
                    cx.set_global(CrashHandler(crash_handler));
                }
                None => {
                    cx.spawn(async move |cx| {
                        let client1 = crash_handler.await;
                        cx.update(|cx| {
                            cx.set_global(CrashHandler(client1));
                        });
                    })
                    .detach();
                }
            }
        }

        initialize_workspace(app_state.clone(), cx);

        cx.activate(true);

        cx.spawn({
            let client = app_state.client.clone();
            async move |cx| authenticate(client, cx).await
        })
        .detach_and_log_err(cx);

        let urls: Vec<_> = args
            .paths_or_urls
            .iter()
            .map(|arg| parse_url_arg(arg, cx))
            .collect();

        // Check if any diff paths are directories to determine diff_all mode
        let diff_all_mode = args
            .diff
            .chunks(2)
            .any(|pair| Path::new(&pair[0]).is_dir() || Path::new(&pair[1]).is_dir());

        let diff_paths: Vec<[String; 2]> = args
            .diff
            .chunks(2)
            .map(|chunk| [chunk[0].clone(), chunk[1].clone()])
            .collect();

        #[cfg(target_os = "windows")]
        let wsl = args.wsl;
        #[cfg(not(target_os = "windows"))]
        let wsl = None;

        if !urls.is_empty() || !diff_paths.is_empty() {
            open_listener.open(RawOpenRequest {
                urls,
                diff_paths,
                wsl,
                diff_all: diff_all_mode,
                dev_container: args.dev_container,
                ..Default::default()
            })
        }

        let (current_session_id, last_session_id) = {
            let session = app_state.session.read(cx);
            (
                session.id().to_owned(),
                session.last_session_id().map(|id| id.to_owned()),
            )
        };

        // `--solution <name-or-id>` short-circuits the normal restore /
        // welcome flow: resolve the solution from the global SolutionStore
        // (initialised earlier in `solutions::init`) and open its members
        // directly. Falls through to the standard path on lookup failure
        // so the user sees the welcome screen and gets a hint via log.
        let solution_arg = args.solution.clone();
        let restore_task = match open_rx
            .try_recv()
            .ok()
            .and_then(|request| OpenRequest::parse(request, cx).log_err())
        {
            Some(request) if request.is_focus_app_only() => cx.spawn({
                let app_state = app_state.clone();
                async move |cx| {
                    if let Err(e) = restore_or_create_workspace(app_state, cx).await {
                        fail_to_open_window_async(e, cx)
                    }
                }
            }),
            Some(request) => {
                handle_open_request(request, app_state.clone(), cx);
                Task::ready(())
            }
            None => cx.spawn({
                let app_state = app_state.clone();
                async move |cx| {
                    if let Some(name_or_id) = solution_arg
                        && open_solution_by_name_or_id(&name_or_id, app_state.clone(), cx)
                            .await
                            .log_err()
                            .unwrap_or(false)
                    {
                        return;
                    }
                    if let Err(e) = restore_or_create_workspace(app_state, cx).await {
                        fail_to_open_window_async(e, cx)
                    }
                }
            }),
        };

        cx.spawn({
            let db = workspace::WorkspaceDb::global(cx);
            let fs = app_state.fs.clone();
            async move |_cx| {
                restore_task.await;
                db.garbage_collect_workspaces(
                    fs.as_ref(),
                    &current_session_id,
                    last_session_id.as_deref(),
                )
                .await
            }
        })
        .detach_and_log_err(cx);

        let app_state = app_state.clone();

        component_preview::init(app_state.clone(), cx);

        cx.spawn(async move |cx| {
            while let Some(urls) = open_rx.next().await {
                cx.update(|cx| {
                    if let Some(request) = OpenRequest::parse(urls, cx).log_err() {
                        handle_open_request(request, app_state.clone(), cx);
                    }
                });
            }
        })
        .detach();

        editor_mcp::start_server(cx).log_err();
    });
}

fn handle_open_request(request: OpenRequest, app_state: Arc<AppState>, cx: &mut App) {
    if let Some(kind) = request.kind {
        match kind {
            OpenRequestKind::CliConnection(connection) => {
                cx.spawn(async move |cx| handle_cli_connection(connection, app_state, cx).await)
                    .detach();
            }
            OpenRequestKind::FocusApp => {
                cx.spawn(async move |cx| {
                    if workspace::activate_any_workspace_window(cx).is_some() {
                        return anyhow::Ok(());
                    }
                    restore_or_create_workspace(app_state, cx).await
                })
                .detach_and_log_err(cx);
            }
            OpenRequestKind::Extension { extension_id } => {
                cx.spawn(async move |cx| {
                    let workspace =
                        workspace::get_any_active_multi_workspace(app_state, cx.clone()).await?;
                    workspace.update(cx, |_, window, cx| {
                        window.dispatch_action(
                            Box::new(zed_actions::Extensions {
                                category_filter: None,
                                id: Some(extension_id),
                            }),
                            cx,
                        );
                    })
                })
                .detach_and_log_err(cx);
            }
            OpenRequestKind::AgentPanel {
                external_source_prompt,
            } => {
                cx.spawn(async move |cx| {
                    let multi_workspace =
                        workspace::get_any_active_multi_workspace(app_state, cx.clone()).await?;

                    let panels_task = multi_workspace.update(cx, |multi_workspace, _, cx| {
                        multi_workspace
                            .workspace()
                            .update(cx, |workspace, _| workspace.take_panels_task())
                    })?;
                    if let Some(task) = panels_task {
                        task.await.log_err();
                    }

                    multi_workspace.update(cx, |multi_workspace, window, cx| {
                        multi_workspace.workspace().update(cx, |workspace, cx| {
                            if let Some(panel) = workspace.focus_panel::<AgentPanel>(window, cx) {
                                panel.update(cx, |panel, cx| {
                                    panel.new_agent_thread_with_external_source_prompt(
                                        external_source_prompt,
                                        window,
                                        cx,
                                    );
                                });
                            } else {
                                log::warn!(
                                    "zed://agent received but the AgentPanel is not registered \
                                     (is `disable_ai` enabled?)"
                                );
                            }
                        });
                    })
                })
                .detach_and_log_err(cx);
            }
            OpenRequestKind::SharedAgentThread { session_id } => {
                cx.spawn(async move |cx| {
                    let multi_workspace =
                        workspace::get_any_active_multi_workspace(app_state.clone(), cx.clone())
                            .await?;

                    let workspace =
                        multi_workspace.read_with(cx, |mw, _| mw.workspace().clone())?;

                    let import_state = multi_workspace.update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            if workspace.root_paths(cx).is_empty() {
                                workspace.focus_panel::<AgentPanel>(window, cx);

                                struct OpenProjectForSharedThreadToast;
                                workspace.show_toast(
                                    Toast::new(
                                        NotificationId::unique::<OpenProjectForSharedThreadToast>(),
                                        "Open a project to import shared threads",
                                    )
                                    .autohide(),
                                    cx,
                                );

                                return anyhow::Ok(None);
                            }

                            let client = workspace.project().read(cx).client();
                            let thread_store: Option<gpui::Entity<ThreadStore>> = workspace
                                .panel::<AgentPanel>(cx)
                                .map(|panel| panel.read(cx).thread_store().clone());
                            anyhow::Ok(Some((client, thread_store)))
                        })
                    })??;

                    let Some((client, thread_store)) = import_state else {
                        return Ok(());
                    };

                    let Some(thread_store): Option<gpui::Entity<ThreadStore>> = thread_store else {
                        anyhow::bail!("Agent panel not available");
                    };

                    let response = client
                        .request(proto::GetSharedAgentThread {
                            session_id: session_id.clone(),
                        })
                        .await
                        .context("Failed to fetch shared thread")?;

                    let shared_thread = SharedThread::from_bytes(&response.thread_data)?;
                    let db_thread = shared_thread.to_db_thread();
                    let session_id = acp::SessionId::new(session_id);

                    let save_session_id = session_id.clone();

                    thread_store
                        .update(&mut cx.clone(), |store, cx| {
                            store.save_thread(
                                save_session_id.clone(),
                                db_thread,
                                Default::default(),
                                cx,
                            )
                        })
                        .await?;

                    let sharer_username = response.sharer_username.clone();

                    multi_workspace.update(cx, |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
                                panel.update(cx, |panel, cx| {
                                    panel.open_thread(
                                        session_id,
                                        None,
                                        Some(format!("🔗 {}", response.title).into()),
                                        window,
                                        cx,
                                    );
                                });
                                panel.focus_handle(cx).focus(window, cx);
                            }

                            struct ImportedThreadToast;
                            workspace.show_toast(
                                Toast::new(
                                    NotificationId::unique::<ImportedThreadToast>(),
                                    format!("Imported shared thread from {}", sharer_username),
                                )
                                .autohide(),
                                cx,
                            );
                        });
                    })?;

                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
            OpenRequestKind::InstallSkill { content } => {
                cx.spawn(async move |cx| {
                    let multi_workspace =
                        workspace::get_any_active_multi_workspace(app_state, cx.clone()).await?;

                    multi_workspace.update(cx, |_multi_workspace, _window, cx| {
                        settings_ui::open_skill_creator(
                            settings_ui::pages::SkillCreatorOpenMode::Install { content },
                            Some(multi_workspace),
                            cx,
                        );
                    })
                })
                .detach_and_log_err(cx);
            }
            OpenRequestKind::DockMenuAction { index } => {
                cx.perform_dock_menu_action(index);
            }
            OpenRequestKind::BuiltinJsonSchema { schema_path } => {
                workspace::with_active_or_new_workspace(cx, |_workspace, window, cx| {
                    cx.spawn_in(window, async move |workspace, cx| {
                        let res = async move {
                            let json = app_state.languages.language_for_name("JSONC").await.ok();
                            let lsp_store = workspace.update(cx, |workspace, cx| {
                                workspace
                                    .project()
                                    .update(cx, |project, _| project.lsp_store())
                            })?;
                            let uri = format!("zed://schemas/{}", schema_path);
                            let json_schema_content =
                                json_schema_store::handle_schema_request(lsp_store, uri, cx)
                                    .await?;
                            let json_schema_value: serde_json::Value =
                                serde_json::from_str(&json_schema_content)
                                    .context("Failed to parse JSON Schema")?;
                            let json_schema_content =
                                serde_json::to_string_pretty(&json_schema_value)
                                    .context("Failed to serialize JSON Schema as JSON")?;
                            let buffer_task = workspace.update(cx, |workspace, cx| {
                                workspace.project().update(cx, |project, cx| {
                                    project.create_buffer(json, false, cx)
                                })
                            })?;

                            let buffer = buffer_task.await?;

                            workspace.update_in(cx, |workspace, window, cx| {
                                buffer.update(cx, |buffer, cx| {
                                    buffer.edit([(0..0, json_schema_content)], None, cx);
                                    buffer.edit(
                                        [(0..0, format!("// {} JSON Schema\n", schema_path))],
                                        None,
                                        cx,
                                    );
                                });

                                workspace.add_item_to_active_pane(
                                    Box::new(cx.new(|cx| {
                                        let mut editor =
                                            editor::Editor::for_buffer(buffer, None, window, cx);
                                        editor.set_read_only(true);
                                        editor
                                    })),
                                    None,
                                    true,
                                    window,
                                    cx,
                                );
                            })
                        }
                        .await;
                        res.context("Failed to open builtin JSON Schema").log_err();
                    })
                    .detach();
                });
            }
            OpenRequestKind::Setting { setting_path } => {
                // zed://settings/languages/$(language)/tab_size  - DONT SUPPORT
                // zed://settings/languages/Rust/tab_size  - SUPPORT
                // languages.$(language).tab_size
                // [ languages $(language) tab_size]
                cx.spawn(async move |cx| {
                    let workspace =
                        workspace::get_any_active_multi_workspace(app_state, cx.clone()).await?;

                    workspace.update(cx, |_, window, cx| match setting_path {
                        None => window.dispatch_action(Box::new(zed_actions::OpenSettings), cx),
                        Some(setting_path) => window.dispatch_action(
                            Box::new(zed_actions::OpenSettingsAt {
                                path: setting_path,
                                target: None,
                            }),
                            cx,
                        ),
                    })
                })
                .detach_and_log_err(cx);
            }
            OpenRequestKind::GitClone { repo_url } => {
                workspace::with_active_or_new_workspace(cx, |_workspace, window, cx| {
                    if window.is_window_active() {
                        clone_and_open(
                            repo_url,
                            cx.weak_entity(),
                            window,
                            cx,
                            Arc::new(|workspace: &mut workspace::Workspace, window, cx| {
                                workspace.focus_panel::<ProjectPanel>(window, cx);
                            }),
                        );
                        return;
                    }

                    let subscription = Rc::new(RefCell::new(None));
                    subscription.replace(Some(cx.observe_in(&cx.entity(), window, {
                        let subscription = subscription.clone();
                        let repo_url = repo_url;
                        move |_, workspace_entity, window, cx| {
                            if window.is_window_active() && subscription.take().is_some() {
                                clone_and_open(
                                    repo_url.clone(),
                                    workspace_entity.downgrade(),
                                    window,
                                    cx,
                                    Arc::new(|workspace: &mut workspace::Workspace, window, cx| {
                                        workspace.focus_panel::<ProjectPanel>(window, cx);
                                    }),
                                );
                            }
                        }
                    })));
                });
            }
            OpenRequestKind::GitCommit { sha } => {
                let base_open_options = zed::open_options_for_request(
                    request.open_behavior,
                    &workspace::SerializedWorkspaceLocation::Local,
                    cx,
                );
                cx.spawn(async move |cx| {
                    let paths_with_position =
                        derive_paths_with_position(app_state.fs.as_ref(), request.open_paths).await;
                    let (workspace, _results) = open_paths_with_positions(
                        &paths_with_position,
                        &[],
                        false,
                        app_state,
                        base_open_options,
                        cx,
                    )
                    .await?;

                    workspace
                        .update(cx, |multi_workspace, window, cx| {
                            multi_workspace
                                .workspace()
                                .clone()
                                .update(cx, |workspace, cx| {
                                    let Some(repo) =
                                        workspace.project().read(cx).active_repository(cx)
                                    else {
                                        log::error!("no active repository found for commit view");
                                        return Err(anyhow::anyhow!("no active repository found"));
                                    };

                                    git_ui::commit_view::CommitView::open(
                                        sha,
                                        repo.downgrade(),
                                        workspace.weak_handle(),
                                        None,
                                        None,
                                        window,
                                        cx,
                                    );
                                    Ok(())
                                })
                        })
                        .log_err();

                    anyhow::Ok(())
                })
                .detach_and_log_err(cx);
            }
        }

        return;
    }

    if let Some(connection_options) = request.remote_connection {
        let open_behavior = request.open_behavior;
        let location = workspace::SerializedWorkspaceLocation::Remote(connection_options.clone());
        let base_open_options = zed::open_options_for_request(open_behavior, &location, cx);
        cx.spawn(async move |cx| {
            let paths: Vec<PathBuf> = request.open_paths.into_iter().map(PathBuf::from).collect();
            open_remote_project(connection_options, paths, app_state, base_open_options, cx).await
        })
        .detach_and_log_err(cx);
        return;
    }

    let mut task = None;
    let dev_container = request.dev_container;
    if !request.open_paths.is_empty() || !request.diff_paths.is_empty() {
        let app_state = app_state.clone();
        let base_open_options = zed::open_options_for_request(
            request.open_behavior,
            &workspace::SerializedWorkspaceLocation::Local,
            cx,
        );
        task = Some(cx.spawn(async move |cx| {
            let paths_with_position =
                derive_paths_with_position(app_state.fs.as_ref(), request.open_paths).await;
            let (_window, results) = open_paths_with_positions(
                &paths_with_position,
                &request.diff_paths,
                request.diff_all,
                app_state,
                workspace::OpenOptions {
                    open_in_dev_container: dev_container,
                    ..base_open_options
                },
                cx,
            )
            .await?;
            for result in results.into_iter().flatten() {
                if let Err(err) = result {
                    log::error!("Error opening path: {err:#}");
                }
            }
            anyhow::Ok(())
        }));
    }

    if !request.open_channel_notes.is_empty() || request.join_channel.is_some() {
        cx.spawn(async move |cx| {
            let result = maybe!(async {
                if let Some(task) = task {
                    task.await?;
                }
                let client = app_state.client.clone();
                // we continue even if authentication fails as join_channel/ open channel notes will
                // show a visible error message.
                authenticate(client, cx).await.log_err();

                if let Some(channel_id) = request.join_channel {
                    cx.update(|cx| {
                        workspace::join_channel(
                            client::ChannelId(channel_id),
                            app_state.clone(),
                            None,
                            None,
                            cx,
                        )
                    })
                    .await?;
                }

                let workspace_window =
                    workspace::get_any_active_multi_workspace(app_state, cx.clone()).await?;

                let workspace = workspace_window.read_with(cx, |mw, _| mw.workspace().clone())?;

                let mut promises = Vec::new();
                for (channel_id, heading) in request.open_channel_notes {
                    promises.push(cx.update_window(workspace_window.into(), |_, window, cx| {
                        ChannelView::open(
                            client::ChannelId(channel_id),
                            heading,
                            workspace.clone(),
                            window,
                            cx,
                        )
                        .log_err()
                    })?)
                }
                future::join_all(promises).await;
                anyhow::Ok(())
            })
            .await;
            if let Err(err) = result {
                fail_to_open_window_async(err, cx);
            }
        })
        .detach()
    } else if let Some(task) = task {
        cx.spawn(async move |cx| {
            if let Err(err) = task.await {
                fail_to_open_window_async(err, cx);
            }
        })
        .detach();
    }
}

async fn authenticate(client: Arc<Client>, cx: &AsyncApp) -> Result<()> {
    if stdout_is_a_pty() {
        if client::IMPERSONATE_LOGIN.is_some() {
            client.sign_in_with_optional_connect(false, cx).await?;
        } else if client.has_credentials(cx).await {
            client.sign_in_with_optional_connect(true, cx).await?;
        }
    } else if client.has_credentials(cx).await {
        client.sign_in_with_optional_connect(true, cx).await?;
    }

    Ok(())
}

async fn system_id() -> Result<IdType> {
    let key_name = "system_id".to_string();
    let db = GlobalKeyValueStore::global();

    if let Ok(Some(system_id)) = db.read_kvp(&key_name) {
        return Ok(IdType::Existing(system_id));
    }

    let system_id = Uuid::new_v4().to_string();

    db.write_kvp(key_name, system_id.clone()).await?;

    Ok(IdType::New(system_id))
}

async fn installation_id(db: KeyValueStore) -> Result<IdType> {
    let legacy_key_name = "device_id".to_string();
    let key_name = "installation_id".to_string();

    // Migrate legacy key to new key
    if let Ok(Some(installation_id)) = db.read_kvp(&legacy_key_name) {
        db.write_kvp(key_name, installation_id.clone()).await?;
        db.delete_kvp(legacy_key_name).await?;
        return Ok(IdType::Existing(installation_id));
    }

    if let Ok(Some(installation_id)) = db.read_kvp(&key_name) {
        return Ok(IdType::Existing(installation_id));
    }

    let installation_id = Uuid::new_v4().to_string();

    db.write_kvp(key_name, installation_id.clone()).await?;

    Ok(IdType::New(installation_id))
}

/// Resolve a Solution by either its `name` or `id` slug, then open its
/// member worktrees in a new workspace window. Empty solutions get the
/// `EmptySolutionPage` placeholder so the user lands somewhere actionable
/// instead of an empty pane.
///
/// Returns `Ok(true)` when the lookup succeeded and a window opened,
/// `Ok(false)` when no solution matched (caller should fall back to the
/// normal restore-or-welcome flow), and `Err(_)` for unrecoverable
/// failures (no SolutionStore initialised, IO errors during open_paths).
async fn open_solution_by_name_or_id(
    name_or_id: &str,
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> Result<bool> {
    use solutions::{SolutionId, SolutionStore};

    struct Resolved {
        id: SolutionId,
        paths: Vec<PathBuf>,
        name: String,
        is_empty: bool,
    }

    let resolved: Option<Resolved> = cx.update(|cx| -> Result<Option<Resolved>> {
        let Some(store) = SolutionStore::try_global(cx) else {
            return Ok(None);
        };
        // `--solution` accepts either the numeric id or the display name.
        let by_id = name_or_id.parse::<i64>().ok().map(SolutionId);
        store.read_with(cx, |s, _| {
            let Some(sol) = s
                .solutions()
                .iter()
                .find(|sol| Some(sol.id) == by_id || sol.name == name_or_id)
            else {
                return Ok::<_, anyhow::Error>(None);
            };
            let is_empty = sol.members.is_empty();
            let paths = if is_empty {
                vec![sol.root.clone()]
            } else {
                s.paths_for_open(sol.id)?
            };
            Ok(Some(Resolved {
                id: sol.id,
                paths,
                name: sol.name.clone(),
                is_empty,
            }))
        })
    })?;

    let Some(resolved) = resolved else {
        log::warn!(
            "sawe: --solution {name_or_id:?} not found in solutions.json; falling back to welcome"
        );
        return Ok(false);
    };

    cx.update(|cx| {
        if let Some(store) = SolutionStore::try_global(cx) {
            store
                .update(cx, |s, cx| s.touch_last_opened(resolved.id, cx))
                .log_err();
        }
    });

    let mut options = workspace::OpenOptions::default();
    options.open_mode = workspace::OpenMode::NewWindow;
    let task =
        cx.update(|cx| workspace::open_paths(&resolved.paths, app_state.clone(), options, cx));
    let opened = task.await?;

    if resolved.is_empty {
        let sol_id = resolved.id.clone();
        let name = resolved.name.clone();
        cx.update(|cx| {
            opened
                .window
                .update(cx, |multi_workspace, window, cx| {
                    let workspace = multi_workspace.workspace().clone();
                    let weak_workspace = workspace.downgrade();
                    workspace.update(cx, |ws, cx| {
                        let page = cx.new(|cx| {
                            solutions_ui::EmptySolutionPage::new(
                                sol_id.clone(),
                                name.clone(),
                                weak_workspace,
                                cx,
                            )
                        });
                        ws.add_item_to_active_pane(Box::new(page), None, true, window, cx);
                    });
                })
                .log_err();
        });
    }

    Ok(true)
}

pub(crate) async fn restore_or_create_workspace(
    app_state: Arc<AppState>,
    cx: &mut AsyncApp,
) -> Result<()> {
    if let Some(multi_workspaces) = restorable_workspaces(cx, &app_state).await {
        let mut error_count = 0;
        for multi_workspace in multi_workspaces {
            let result = match &multi_workspace.active_workspace.location {
                SerializedWorkspaceLocation::Local => {
                    restore_multiworkspace(multi_workspace, app_state.clone(), cx)
                        .await
                        .map(|_| ())
                }
                SerializedWorkspaceLocation::Remote(connection_options) => {
                    let mut connection_options = connection_options.clone();
                    if let RemoteConnectionOptions::Ssh(options) = &mut connection_options {
                        cx.update(|cx| {
                            RemoteSettings::get_global(cx)
                                .fill_connection_options_from_settings(options)
                        });
                    }

                    let paths = multi_workspace
                        .active_workspace
                        .paths
                        .paths()
                        .iter()
                        .map(PathBuf::from)
                        .collect::<Vec<_>>();
                    let state = multi_workspace.state.clone();
                    async {
                        let window = open_remote_project(
                            connection_options,
                            paths,
                            app_state.clone(),
                            workspace::OpenOptions::default(),
                            cx,
                        )
                        .await?;
                        workspace::apply_restored_multiworkspace_state(
                            window,
                            &state,
                            app_state.fs.clone(),
                            cx,
                        )
                        .await;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await
                }
            };

            if let Err(error) = result {
                log::error!("Failed to restore workspace: {error:#}");
                error_count += 1;
            }
        }

        if error_count > 0 {
            let message = if error_count == 1 {
                "Failed to restore 1 workspace. Check logs for details.".to_string()
            } else {
                format!(
                    "Failed to restore {} workspaces. Check logs for details.",
                    error_count
                )
            };

            // Try to find an active workspace to show the toast
            let toast_shown = cx.update(|cx| {
                if let Some(window) = cx.active_window()
                    && let Some(multi_workspace) = window.downcast::<MultiWorkspace>()
                {
                    multi_workspace
                        .update(cx, |multi_workspace, _, cx| {
                            multi_workspace.workspace().update(cx, |workspace, cx| {
                                workspace.show_toast(
                                    Toast::new(NotificationId::unique::<()>(), message.clone()),
                                    cx,
                                )
                            });
                        })
                        .ok();
                    return true;
                }
                false
            });

            // If we couldn't show a toast (no windows opened successfully),
            // open a fallback empty workspace and show the error there
            if !toast_shown {
                log::error!("All workspace restorations failed. Opening fallback empty workspace.");
                cx.update(|cx| {
                    workspace::open_new(
                        Default::default(),
                        app_state.clone(),
                        cx,
                        |workspace, _window, cx| {
                            workspace.show_toast(
                                Toast::new(NotificationId::unique::<()>(), message),
                                cx,
                            );
                        },
                    )
                })
                .await?;
            }
        }

        // If the user cancelled a failed remote connection at startup,
        // open_remote_project returns Ok but removes the window, so error_count
        // stays 0 and the toast fallback above does not trigger. Without this
        // check, Zed would exit silently.
        if cx.update(|cx| cx.windows().is_empty()) {
            cx.update(|cx| {
                workspace::open_new(
                    Default::default(),
                    app_state.clone(),
                    cx,
                    |workspace, window, cx| {
                        let restore_on_startup =
                            WorkspaceSettings::get_global(cx).restore_on_startup;
                        match restore_on_startup {
                            workspace::RestoreOnStartupBehavior::Launchpad => {}
                            _ => {
                                Editor::new_file(workspace, &Default::default(), window, cx);
                            }
                        }
                    },
                )
            })
            .await?;
        }
    } else {
        // SPK fork: Welcome is the launcher for every cold launch, not
        // just the very first one. Upstream's `FIRST_OPEN` gate split
        // the flow into "show onboarding once, then open an empty
        // workspace forever after"; with Solutions as the project model
        // an empty workspace is meaningless, so we always send the
        // user to the Welcome window where they can pick or create a
        // Solution.
        let _ = FIRST_OPEN;
        cx.update(|cx| show_onboarding_view(app_state, cx)).await?;
    }

    Ok(())
}

async fn restorable_workspaces(
    cx: &mut AsyncApp,
    app_state: &Arc<AppState>,
) -> Option<Vec<workspace::SerializedMultiWorkspace>> {
    let locations = restorable_workspace_locations(cx, app_state).await?;
    Some(cx.update(|cx| workspace::read_serialized_multi_workspaces(locations, cx)))
}

pub(crate) async fn restorable_workspace_locations(
    cx: &mut AsyncApp,
    app_state: &Arc<AppState>,
) -> Option<Vec<SessionWorkspace>> {
    let (mut restore_behavior, db) = cx.update(|cx| {
        (
            WorkspaceSettings::get(None, cx).restore_on_startup,
            workspace::WorkspaceDb::global(cx),
        )
    });

    let session_handle = app_state.session.clone();
    let (last_session_id, last_session_window_stack) = cx.update(|cx| {
        let session = session_handle.read(cx);

        (
            session.last_session_id().map(|id| id.to_string()),
            session.last_session_window_stack(),
        )
    });

    if last_session_id.is_none()
        && matches!(
            restore_behavior,
            workspace::RestoreOnStartupBehavior::LastSession
        )
    {
        restore_behavior = workspace::RestoreOnStartupBehavior::LastWorkspace;
    }

    match restore_behavior {
        workspace::RestoreOnStartupBehavior::LastWorkspace => {
            workspace::last_opened_workspace_location(&db, app_state.fs.as_ref())
                .await
                .map(|(workspace_id, location, paths)| {
                    vec![SessionWorkspace {
                        workspace_id,
                        location,
                        paths,
                        window_id: None,
                    }]
                })
        }
        workspace::RestoreOnStartupBehavior::LastSession => {
            if let Some(last_session_id) = last_session_id {
                let ordered = last_session_window_stack.is_some();

                let mut locations = workspace::last_session_workspace_locations(
                    &db,
                    &last_session_id,
                    last_session_window_stack,
                    app_state.fs.as_ref(),
                )
                .await
                .filter(|locations| !locations.is_empty());

                // Since last_session_window_order returns the windows ordered front-to-back
                // we need to open the window that was frontmost last.
                if ordered && let Some(locations) = locations.as_mut() {
                    locations.reverse();
                }

                locations
            } else {
                None
            }
        }
        _ => None,
    }
}

fn init_paths() -> HashMap<io::ErrorKind, Vec<&'static Path>> {
    [
        paths::config_dir(),
        paths::extensions_dir(),
        paths::languages_dir(),
        paths::debug_adapters_dir(),
        paths::database_dir(),
        paths::logs_dir(),
        paths::temp_dir(),
        paths::hang_traces_dir(),
    ]
    .into_iter()
    .fold(HashMap::default(), |mut errors, path| {
        if let Err(e) = std::fs::create_dir_all(path) {
            errors.entry(e.kind()).or_insert_with(Vec::new).push(path);
        }
        errors
    })
}

pub(crate) static FORCE_CLI_MODE: LazyLock<bool> = LazyLock::new(|| {
    let env_var = std::env::var(FORCE_CLI_MODE_ENV_VAR_NAME).ok().is_some();
    unsafe { std::env::remove_var(FORCE_CLI_MODE_ENV_VAR_NAME) };
    env_var
});

fn stdout_is_a_pty() -> bool {
    !*FORCE_CLI_MODE && io::stdout().is_terminal()
}

#[derive(Parser, Debug)]
#[command(name = "sawe", disable_version_flag = true, max_term_width = 100)]
struct Args {
    /// A sequence of space-separated paths or urls that you want to open.
    ///
    /// Use `path:line:row` syntax to open a file at a specific location.
    /// Non-existing paths and directories will ignore `:line:row` suffix.
    ///
    /// URLs can either be `file://` or `sawe://` scheme.
    paths_or_urls: Vec<String>,

    /// Open a Solution by name or id and skip the Welcome screen.
    ///
    /// Looks up the named entry in `~/.config/sawe/solutions.json`
    /// (matching either the human-readable `name` or the slug `id`) and
    /// opens its member worktrees in a new window. If the solution has no
    /// members yet, the window opens at `solution.root` with the empty-
    /// solution placeholder page so the user can add projects.
    ///
    /// Example: `sawe --solution probe-test`
    #[arg(long, value_name = "NAME-OR-ID")]
    solution: Option<String>,

    /// Pairs of file paths to diff. Can be specified multiple times.
    /// When directories are provided, recurses into them and shows all changed files in a single multi-diff view.
    #[arg(long, action = clap::ArgAction::Append, num_args = 2, value_names = ["OLD_PATH", "NEW_PATH"])]
    diff: Vec<String>,

    /// Sets a custom directory for all user data (e.g., database, extensions, logs).
    ///
    /// This overrides the default platform-specific data directory location.
    /// On macOS, the default is `~/Library/Application Support/Sawe`.
    /// On Linux/FreeBSD, the default is `$XDG_DATA_HOME/sawe`.
    /// On Windows, the default is `%LOCALAPPDATA%\Sawe`.
    #[arg(long, value_name = "DIR", verbatim_doc_comment)]
    user_data_dir: Option<String>,

    /// The username and WSL distribution to use when opening paths. If not specified,
    /// Sawe will attempt to open the paths directly.
    ///
    /// The username is optional, and if not specified, the default user for the distribution
    /// will be used.
    ///
    /// Example: `me@Ubuntu` or `Ubuntu`.
    ///
    /// WARN: You should not fill in this field by hand.
    #[cfg(target_os = "windows")]
    #[arg(long, value_name = "USER@DISTRO")]
    wsl: Option<String>,

    /// Open the project in a dev container.
    ///
    /// Automatically triggers "Reopen in Dev Container" if a `.devcontainer/`
    /// configuration is found in the project directory.
    #[arg(long)]
    dev_container: bool,

    /// Instructs sawe to run as a dev server on this machine. (not implemented)
    #[arg(long)]
    dev_server_token: Option<String>,

    /// Prints system specs.
    ///
    /// Useful for submitting issues on GitHub when encountering a bug that
    /// prevents Sawe from starting, so you can't run `zed: copy system specs to
    /// clipboard`
    #[arg(long)]
    system_specs: bool,

    /// Used for the MCP Server, to remove the need for netcat as a dependency,
    /// by having Sawe act like netcat communicating over a Unix socket.
    #[arg(long, hide = true)]
    nc: Option<String>,

    /// Run the editor on the native headless GPUI platform — no X / Wayland
    /// connection, no window on the user's desktop, all rendering through
    /// the offscreen wgpu pipeline. Used by `script/run-mcp --headless` to
    /// drive the editor autonomously over the MCP socket.
    ///
    /// `workspace.screenshot` still works in this mode (the offscreen
    /// renderer captures rendered pixels into a PNG). UI dispatch via
    /// `windows.dispatch_action` / `send_keystroke` / `send_text` /
    /// `click_at` works identically to the on-screen platform.
    ///
    /// SPK fork addition (was Xvfb-wrapped before; see ADR-0002).
    #[arg(long)]
    headless: bool,

    /// Used as `GIT_SEQUENCE_EDITOR` during programmatic interactive rebase.
    /// Reads the pre-built todo from the session directory pointed to by
    /// `SPK_GIT_HELPER_SESSION` and overwrites the path passed by `git`.
    /// See plan task S-RBL.
    #[arg(long, hide = true)]
    git_rebase_helper: Option<PathBuf>,

    /// Used as `exec` step inside a programmatic interactive rebase.
    /// Reads a pre-supplied commit message from the session directory
    /// pointed to by `SPK_GIT_HELPER_SESSION` and runs
    /// `git commit --amend -F <path>` in the rebase worktree. See plan task S-RBL.
    #[arg(long, hide = true)]
    git_message_set: Option<String>,

    /// Runs the editor binary as claude's `WorktreeCreate` / `WorktreeRemove`
    /// hook (`create` / `remove`). Reads the hook payload as JSON on stdin and,
    /// for `create`, prints the absolute path of the worktree it made on stdout.
    /// Configured by the editor-owned claude settings layer so agent worktrees
    /// land under `<solution_root>/.agents/worktrees/` instead of
    /// `<member>/.claude/worktrees/`.
    #[arg(long, hide = true, value_name = "create|remove")]
    worktree_hook: Option<String>,

    /// Base directory for `--worktree-hook` — `<solution_root>/.agents/worktrees`.
    #[arg(long, hide = true, value_name = "DIR")]
    worktree_base: Option<PathBuf>,

    /// Used for recording minidumps on crashes by having Sawe run a separate
    /// process communicating over a socket.
    #[arg(long, hide = true)]
    crash_handler: Option<PathBuf>,

    /// Run sawe in the foreground, only used on Windows, to match the behavior on macOS.
    #[arg(long)]
    #[cfg(target_os = "windows")]
    #[arg(hide = true)]
    foreground: bool,

    /// The dock action to perform. This is used on Windows only.
    #[arg(long)]
    #[cfg(target_os = "windows")]
    #[arg(hide = true)]
    dock_action: Option<usize>,

    /// Used for SSH/Git password authentication, to remove the need for netcat as a dependency,
    /// by having Sawe act like netcat communicating over a Unix socket.
    #[arg(long)]
    #[cfg(not(target_os = "windows"))]
    #[arg(hide = true)]
    askpass: Option<String>,

    #[arg(long, hide = true)]
    dump_all_actions: bool,

    /// Output current environment variables as JSON to stdout
    #[arg(long, hide = true)]
    printenv: bool,

    /// Record an ETW trace. Must be run as administrator.
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    record_etw_trace: bool,

    /// The PID of the Sawe process to trace for heap analysis.
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true, allow_hyphen_values = true)]
    etw_zed_pid: Option<i64>,

    /// Output path for the ETW trace file.
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    etw_output: Option<PathBuf>,

    /// Unix socket path for IPC with the parent Sawe process.
    #[cfg(target_os = "windows")]
    #[arg(long, hide = true)]
    etw_socket: Option<String>,
}

#[derive(Clone, Debug)]
enum IdType {
    New(String),
    Existing(String),
}

impl ToString for IdType {
    fn to_string(&self) -> String {
        match self {
            IdType::New(id) | IdType::Existing(id) => id.clone(),
        }
    }
}

/// Returns true if `arg` looks like a URL (any scheme this editor knows
/// how to route via the URL handler). Path arguments must NOT match.
/// Both single-instance handoff and CLI URL parsing rely on this, so a
/// new scheme only needs to be added here.
fn is_url_scheme(arg: &str) -> bool {
    arg.starts_with("file://")
        || arg.starts_with("zed://")
        || arg.starts_with("zed-cli://")
        || arg.starts_with("ssh://")
        || arg.starts_with("sawe://")
}

fn parse_url_arg(arg: &str, cx: &App) -> String {
    match std::fs::canonicalize(Path::new(&arg)) {
        Ok(path) => format!("file://{}", path.display()),
        Err(_) => {
            if is_url_scheme(arg) || parse_zed_link(arg, cx).is_some() {
                arg.into()
            } else {
                format!("file://{arg}")
            }
        }
    }
}

fn load_embedded_fonts(cx: &App) {
    let asset_source = cx.asset_source();
    let font_paths = asset_source.list("fonts").unwrap();
    let embedded_fonts = Mutex::new(Vec::new());
    let executor = cx.background_executor();

    cx.foreground_executor().block_on(executor.scoped(|scope| {
        for font_path in &font_paths {
            if !font_path.ends_with(".ttf") {
                continue;
            }

            scope.spawn(async {
                let font_bytes = asset_source.load(font_path).unwrap().unwrap();
                embedded_fonts.lock().push(font_bytes);
            });
        }
    }));

    cx.text_system()
        .add_fonts(embedded_fonts.into_inner())
        .unwrap();
}

/// Spawns a background task to load the user themes from the themes directory.
fn load_user_themes_in_background(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    cx.spawn({
        let fs = fs.clone();
        async move |cx| {
            let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
            let themes_dir = paths::themes_dir().as_ref();
            match fs
                .metadata(themes_dir)
                .await
                .ok()
                .flatten()
                .map(|m| m.is_dir)
            {
                Some(is_dir) => {
                    anyhow::ensure!(is_dir, "Themes dir path {themes_dir:?} is not a directory")
                }
                None => {
                    fs.create_dir(themes_dir).await.with_context(|| {
                        format!("Failed to create themes dir at path {themes_dir:?}")
                    })?;
                }
            }

            let mut theme_paths = fs
                .read_dir(themes_dir)
                .await
                .with_context(|| format!("reading themes from {themes_dir:?}"))?;

            while let Some(theme_path) = theme_paths.next().await {
                let Some(theme_path) = theme_path.log_err() else {
                    continue;
                };
                let Some(bytes) = fs.load_bytes(&theme_path).await.log_err() else {
                    continue;
                };

                load_user_theme(&theme_registry, &bytes).log_err();
            }

            cx.update(theme_settings::reload_theme);
            anyhow::Ok(())
        }
    })
    .detach_and_log_err(cx);
}

/// Spawns a background task to watch the themes directory for changes.
fn watch_themes(fs: Arc<dyn fs::Fs>, cx: &mut App) {
    use std::time::Duration;
    cx.spawn(async move |cx| {
        let (mut events, _) = fs
            .watch(paths::themes_dir(), Duration::from_millis(100))
            .await;

        while let Some(paths) = events.next().await {
            for event in paths {
                if fs.metadata(&event.path).await.ok().flatten().is_some() {
                    let theme_registry = cx.update(|cx| ThemeRegistry::global(cx));
                    if let Some(bytes) = fs.load_bytes(&event.path).await.log_err()
                        && load_user_theme(&theme_registry, &bytes).log_err().is_some()
                    {
                        cx.update(theme_settings::reload_theme);
                    }
                }
            }
        }
    })
    .detach()
}

#[cfg(debug_assertions)]
fn watch_languages(fs: Arc<dyn fs::Fs>, languages: Arc<LanguageRegistry>, cx: &mut App) {
    use std::time::Duration;

    cx.background_spawn(async move {
        let languages_src = Path::new("crates/grammars/src");
        let Some(languages_src) = fs.canonicalize(languages_src).await.log_err() else {
            return;
        };

        let (mut events, watcher) = fs.watch(&languages_src, Duration::from_millis(100)).await;

        // add subdirectories since fs.watch is not recursive on Linux
        if let Some(mut paths) = fs.read_dir(&languages_src).await.log_err() {
            while let Some(path) = paths.next().await {
                if let Some(path) = path.log_err()
                    && fs.is_dir(&path).await
                {
                    watcher.add(&path).log_err();
                }
            }
        }

        while let Some(event) = events.next().await {
            let has_language_file = event
                .iter()
                .any(|event| event.path.extension().is_some_and(|ext| ext == "scm"));
            if has_language_file {
                languages.reload();
            }
        }
    })
    .detach();
}

fn dump_all_gpui_actions() {
    #[derive(Debug, serde::Serialize)]
    struct ActionDef {
        name: &'static str,
        human_name: String,
        schema: Option<serde_json::Value>,
        deprecated_aliases: &'static [&'static str],
        deprecation_message: Option<&'static str>,
        documentation: Option<&'static str>,
    }
    let mut generator = settings::KeymapFile::action_schema_generator();
    let mut actions = gpui::generate_list_of_all_registered_actions()
        .map(|action| {
            let schema = (action.json_schema)(&mut generator)
                .map(|s| serde_json::to_value(s).expect("Failed to serialize action schema"));
            ActionDef {
                name: action.name,
                human_name: command_palette::humanize_action_name(action.name),
                schema,
                deprecated_aliases: action.deprecated_aliases,
                deprecation_message: action.deprecation_message,
                documentation: action.documentation,
            }
        })
        .collect::<Vec<ActionDef>>();

    actions.sort_by_key(|a| a.name);

    let schema_definitions = serde_json::to_value(generator.definitions())
        .expect("Failed to serialize schema definitions");

    let output = serde_json::json!({
        "actions": actions,
        "schema_definitions": schema_definitions,
    });

    io::Write::write(
        &mut std::io::stdout(),
        serde_json::to_string_pretty(&output).unwrap().as_bytes(),
    )
    .unwrap();
}

#[cfg(target_os = "windows")]
fn check_for_conpty_dll() {
    use windows::{
        Win32::{Foundation::FreeLibrary, System::LibraryLoader::LoadLibraryW},
        core::w,
    };

    if let Ok(hmodule) = unsafe { LoadLibraryW(w!("conpty.dll")) } {
        unsafe {
            FreeLibrary(hmodule)
                .context("Failed to free conpty.dll")
                .log_err();
        }
    } else {
        log::warn!("Failed to load conpty.dll. Terminal will work with reduced functionality.");
    }
}
