//! Paths to locations used by Sawe.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, OnceLock};

use util::paths::SanitizedPath;
pub use util::paths::home_dir;
use util::rel_path::RelPath;

/// A default editorconfig file name to use when resolving project settings.
pub const EDITORCONFIG_NAME: &str = ".editorconfig";

/// True when this build should use a `-dev` directory suffix to keep
/// developer state separate from a production install's database,
/// sessions, MCP socket, and so on.
///
/// Default rule:
///   * Debug builds (`cfg!(debug_assertions)`) → `true`
///   * Release builds → `false`
///
/// Override (in either direction) via the `SAWE_DEV_DIRS` env
/// var: set to `1` / `true` to force the dev suffix on (useful when
/// you want a release-shaped binary to write to a sandboxed dir),
/// set to `0` / `false` to force it off (useful when a debug build
/// needs to pick up the user's actual production data — e.g. trying
/// to reproduce a bug against their workspace database).
fn use_dev_suffix() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| match std::env::var("SAWE_DEV_DIRS") {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => cfg!(debug_assertions),
    })
}

/// Kebab-case directory name (Linux / generic). See `use_dev_suffix`.
pub fn dir_name_kebab() -> &'static str {
    if use_dev_suffix() {
        "sawe-dev"
    } else {
        "sawe"
    }
}

/// PascalCase directory name (macOS / Windows). See `use_dev_suffix`.
pub fn dir_name_pascal() -> &'static str {
    if use_dev_suffix() {
        "Sawe-Dev"
    } else {
        "Sawe"
    }
}

/// A custom data directory override, set only by `set_custom_data_dir`.
/// This is used to override the default data directory location.
/// The directory will be created if it doesn't exist when set.
static CUSTOM_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The resolved data directory, combining custom override or platform defaults.
/// This is set once and cached for subsequent calls.
/// On all platforms this is `base_dir()/data`, i.e. `~/.spk/sawe/data`
/// (or `~/.spk/sawe-dev/data` for dev builds). See `base_dir`.
static CURRENT_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The resolved config directory, combining custom override or platform defaults.
/// This is set once and cached for subsequent calls.
/// On all platforms this is `base_dir()/config`, i.e. `~/.spk/sawe/config`
/// (or `~/.spk/sawe-dev/config` for dev builds). See `base_dir`.
static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Returns the relative path to the zed_server directory on the ssh host.
pub fn remote_server_dir_relative() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".zed_server").unwrap());
    *CACHED
}

// Remove this once 223 goes stable
/// Returns the relative path to the zed_wsl_server directory on the wsl host.
pub fn remote_wsl_server_dir_relative() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".zed_wsl_server").unwrap());
    *CACHED
}

/// Sets a custom directory for all user data, overriding the default data directory.
/// This function must be called before any other path operations that depend on the data directory.
/// The directory's path will be canonicalized to an absolute path by a blocking FS operation.
/// The directory will be created if it doesn't exist.
///
/// # Arguments
///
/// * `dir` - The path to use as the custom data directory. This will be used as the base
///   directory for all user data, including databases, extensions, and logs.
///
/// # Returns
///
/// A reference to the static `PathBuf` containing the custom data directory path.
///
/// # Panics
///
/// Panics if:
/// * Called after the data directory has been initialized (e.g., via `data_dir` or `config_dir`)
/// * The directory's path cannot be canonicalized to an absolute path
/// * The directory cannot be created
pub fn set_custom_data_dir(dir: &str) -> &'static PathBuf {
    if CURRENT_DATA_DIR.get().is_some() || CONFIG_DIR.get().is_some() {
        panic!("set_custom_data_dir called after data_dir or config_dir was initialized");
    }
    CUSTOM_DATA_DIR.get_or_init(|| {
        let path = PathBuf::from(dir);
        std::fs::create_dir_all(&path).expect("failed to create custom data directory");
        let canonicalized = path
            .canonicalize()
            .expect("failed to canonicalize custom data directory's path to an absolute path");
        // On Windows, `canonicalize` produces extended-length paths prefixed
        // with `\\?\`. Strip that prefix so downstream consumers (e.g.
        // Node.js language servers) that receive derived paths as arguments
        // don't choke on the verbatim syntax.
        SanitizedPath::new(&canonicalized).as_path().to_path_buf()
    })
}

/// Single root directory under which **all** profile state lives —
/// config, data, cache, logs, solutions, the lot. We tuck everything
/// into a hidden `~/.spk/` namespace so `~/` doesn't accumulate
/// per-app dotfree folders, and so sibling SPK tools can colocate
/// their own state under the same umbrella.
///
/// Layout:
///   `~/.spk/sawe/` — release profile of this editor
///   `~/.spk/sawe-dev/` — debug profile of this editor
///   `~/.spk/<other-spk-tool>/` — sibling apps drop their state here
///
/// Windows / macOS get the same single-folder layout (no native
/// `Application Support` / `AppData` split) so the cleanup story
/// stays one-line everywhere: `rm -rf ~/.spk/sawe[-dev]`.
pub fn base_dir() -> &'static PathBuf {
    static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();
    BASE_DIR.get_or_init(|| {
        if let Some(custom) = CUSTOM_DATA_DIR.get() {
            return custom.clone();
        }
        home_dir().join(".spk").join(dir_name_kebab())
    })
}

/// Returns the path to the configuration directory used by Sawe.
pub fn config_dir() -> &'static PathBuf {
    CONFIG_DIR.get_or_init(|| {
        if let Some(custom_dir) = CUSTOM_DATA_DIR.get() {
            custom_dir.join("config")
        } else {
            base_dir().join("config")
        }
    })
}

/// Returns the path to the data directory used by Sawe.
pub fn data_dir() -> &'static PathBuf {
    CURRENT_DATA_DIR.get_or_init(|| {
        if let Some(custom_dir) = CUSTOM_DATA_DIR.get() {
            custom_dir.clone()
        } else {
            base_dir().join("data")
        }
    })
}

pub fn state_dir() -> &'static PathBuf {
    static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();
    STATE_DIR.get_or_init(|| base_dir().join("state"))
}

/// Returns the path to the temp / cache directory used by Sawe.
pub fn temp_dir() -> &'static PathBuf {
    static TEMP_DIR: OnceLock<PathBuf> = OnceLock::new();
    TEMP_DIR.get_or_init(|| base_dir().join("cache"))
}

/// Returns the path to the hang traces directory.
pub fn hang_traces_dir() -> &'static PathBuf {
    static LOGS_DIR: OnceLock<PathBuf> = OnceLock::new();
    LOGS_DIR.get_or_init(|| data_dir().join("hang_traces"))
}

/// Returns the path to the logs directory.
pub fn logs_dir() -> &'static PathBuf {
    static LOGS_DIR: OnceLock<PathBuf> = OnceLock::new();
    LOGS_DIR.get_or_init(|| base_dir().join("logs"))
}

/// Returns the path to the Sawe server directory on this SSH host.
pub fn remote_server_state_dir() -> &'static PathBuf {
    static REMOTE_SERVER_STATE: OnceLock<PathBuf> = OnceLock::new();
    REMOTE_SERVER_STATE.get_or_init(|| data_dir().join("server_state"))
}

/// Returns the path to the `Sawe.log` file.
pub fn log_file() -> &'static PathBuf {
    static LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
    LOG_FILE.get_or_init(|| logs_dir().join("Sawe.log"))
}

/// Returns the path to the `Sawe.log.old` file.
pub fn old_log_file() -> &'static PathBuf {
    static OLD_LOG_FILE: OnceLock<PathBuf> = OnceLock::new();
    OLD_LOG_FILE.get_or_init(|| logs_dir().join("Sawe.log.old"))
}

/// Returns the path to the database directory.
pub fn database_dir() -> &'static PathBuf {
    static DATABASE_DIR: OnceLock<PathBuf> = OnceLock::new();
    DATABASE_DIR.get_or_init(|| data_dir().join("db"))
}

/// Returns the path to the crashes directory, if it exists for the current platform.
pub fn crashes_dir() -> &'static Option<PathBuf> {
    static CRASHES_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    CRASHES_DIR.get_or_init(|| {
        cfg!(target_os = "macos").then_some(home_dir().join("Library/Logs/DiagnosticReports"))
    })
}

/// Returns the path to the retired crashes directory, if it exists for the current platform.
pub fn crashes_retired_dir() -> &'static Option<PathBuf> {
    static CRASHES_RETIRED_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    CRASHES_RETIRED_DIR.get_or_init(|| crashes_dir().as_ref().map(|dir| dir.join("Retired")))
}

/// Returns the path to the `settings.json` file.
pub fn settings_file() -> &'static PathBuf {
    static SETTINGS_FILE: OnceLock<PathBuf> = OnceLock::new();
    SETTINGS_FILE.get_or_init(|| config_dir().join("settings.json"))
}

/// Returns the path to the global `AGENTS.md` instructions file.
pub fn agents_file() -> &'static PathBuf {
    static AGENTS_FILE: OnceLock<PathBuf> = OnceLock::new();
    AGENTS_FILE.get_or_init(|| config_dir().join("AGENTS.md"))
}

/// Returns the path to the global settings file.
pub fn global_settings_file() -> &'static PathBuf {
    static GLOBAL_SETTINGS_FILE: OnceLock<PathBuf> = OnceLock::new();
    GLOBAL_SETTINGS_FILE.get_or_init(|| config_dir().join("global_settings.json"))
}

/// Returns the path to the `settings_backup.json` file.
pub fn settings_backup_file() -> &'static PathBuf {
    static SETTINGS_FILE: OnceLock<PathBuf> = OnceLock::new();
    SETTINGS_FILE.get_or_init(|| config_dir().join("settings_backup.json"))
}

/// Returns the path to the `keymap.json` file.
pub fn keymap_file() -> &'static PathBuf {
    static KEYMAP_FILE: OnceLock<PathBuf> = OnceLock::new();
    KEYMAP_FILE.get_or_init(|| config_dir().join("keymap.json"))
}

/// Returns the path to the `keymap_backup.json` file.
pub fn keymap_backup_file() -> &'static PathBuf {
    static KEYMAP_FILE: OnceLock<PathBuf> = OnceLock::new();
    KEYMAP_FILE.get_or_init(|| config_dir().join("keymap_backup.json"))
}

/// Returns the path to the `tasks.json` file.
pub fn tasks_file() -> &'static PathBuf {
    static TASKS_FILE: OnceLock<PathBuf> = OnceLock::new();
    TASKS_FILE.get_or_init(|| config_dir().join("tasks.json"))
}

/// Returns the path to the `debug.json` file.
pub fn debug_scenarios_file() -> &'static PathBuf {
    static DEBUG_SCENARIOS_FILE: OnceLock<PathBuf> = OnceLock::new();
    DEBUG_SCENARIOS_FILE.get_or_init(|| config_dir().join("debug.json"))
}

/// Returns the path to the `run-configurations.json` file.
pub fn run_configurations_file() -> &'static PathBuf {
    static RUN_CONFIGURATIONS_FILE: OnceLock<PathBuf> = OnceLock::new();
    RUN_CONFIGURATIONS_FILE.get_or_init(|| config_dir().join("run-configurations.json"))
}

/// Returns the path to the `remote-control.json` file.
pub fn remote_control_settings_file() -> &'static PathBuf {
    static REMOTE_CONTROL_SETTINGS_FILE: OnceLock<PathBuf> = OnceLock::new();
    REMOTE_CONTROL_SETTINGS_FILE.get_or_init(|| config_dir().join("remote-control.json"))
}

/// Returns the path to the persisted Remote Control self-signed TLS cert
/// (`remote-control.cert.der`). The cert is generated on first `enabled =
/// true` and reused on subsequent starts so the SHA-256 fingerprint stays
/// stable across editor restarts — the Android client pins on it.
pub fn remote_control_cert_file() -> &'static PathBuf {
    static REMOTE_CONTROL_CERT_FILE: OnceLock<PathBuf> = OnceLock::new();
    REMOTE_CONTROL_CERT_FILE.get_or_init(|| config_dir().join("remote-control.cert.der"))
}

/// Returns the path to the persisted Remote Control private key
/// (`remote-control.key.der`). Sibling to `remote_control_cert_file`.
pub fn remote_control_key_file() -> &'static PathBuf {
    static REMOTE_CONTROL_KEY_FILE: OnceLock<PathBuf> = OnceLock::new();
    REMOTE_CONTROL_KEY_FILE.get_or_init(|| config_dir().join("remote-control.key.der"))
}

/// Returns the path to the extensions directory.
///
/// This is where installed extensions are stored.
pub fn extensions_dir() -> &'static PathBuf {
    static EXTENSIONS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EXTENSIONS_DIR.get_or_init(|| data_dir().join("extensions"))
}

/// Returns the path to the extensions directory.
///
/// This is where installed extensions are stored on a remote.
pub fn remote_extensions_dir() -> &'static PathBuf {
    static EXTENSIONS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EXTENSIONS_DIR.get_or_init(|| data_dir().join("remote_extensions"))
}

/// Returns the path to the extensions directory.
///
/// This is where installed extensions are stored on a remote.
pub fn remote_extensions_uploads_dir() -> &'static PathBuf {
    static UPLOAD_DIR: OnceLock<PathBuf> = OnceLock::new();
    UPLOAD_DIR.get_or_init(|| remote_extensions_dir().join("uploads"))
}

/// Returns the path to the themes directory.
///
/// This is where themes that are not provided by extensions are stored.
pub fn themes_dir() -> &'static PathBuf {
    static THEMES_DIR: OnceLock<PathBuf> = OnceLock::new();
    THEMES_DIR.get_or_init(|| config_dir().join("themes"))
}

/// Returns the path to the snippets directory.
pub fn snippets_dir() -> &'static PathBuf {
    static SNIPPETS_DIR: OnceLock<PathBuf> = OnceLock::new();
    SNIPPETS_DIR.get_or_init(|| config_dir().join("snippets"))
}

/// Returns the path to the contexts directory.
///
/// This is where the prompts for use with the Assistant are stored.
pub fn prompts_dir() -> &'static PathBuf {
    static PROMPTS_DIR: OnceLock<PathBuf> = OnceLock::new();
    PROMPTS_DIR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            config_dir().join("prompts")
        } else {
            data_dir().join("prompts")
        }
    })
}

/// Returns the path to the prompt templates directory.
///
/// This is where the prompt templates for core features can be overridden with templates.
///
/// # Arguments
///
/// * `dev_mode` - If true, assumes the current working directory is the Zed repository.
pub fn prompt_overrides_dir(repo_path: Option<&Path>) -> PathBuf {
    if let Some(path) = repo_path {
        let dev_path = path.join("assets").join("prompts");
        if dev_path.exists() {
            return dev_path;
        }
    }

    static PROMPT_TEMPLATES_DIR: OnceLock<PathBuf> = OnceLock::new();
    PROMPT_TEMPLATES_DIR
        .get_or_init(|| {
            if cfg!(target_os = "macos") {
                config_dir().join("prompt_overrides")
            } else {
                data_dir().join("prompt_overrides")
            }
        })
        .clone()
}

/// Returns the path to the semantic search's embeddings directory.
///
/// This is where the embeddings used to power semantic search are stored.
pub fn embeddings_dir() -> &'static PathBuf {
    static EMBEDDINGS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EMBEDDINGS_DIR.get_or_init(|| {
        if cfg!(target_os = "macos") {
            config_dir().join("embeddings")
        } else {
            data_dir().join("embeddings")
        }
    })
}

/// Returns the path to the languages directory.
///
/// This is where language servers are downloaded to for languages built-in to Zed.
pub fn languages_dir() -> &'static PathBuf {
    static LANGUAGES_DIR: OnceLock<PathBuf> = OnceLock::new();
    LANGUAGES_DIR.get_or_init(|| data_dir().join("languages"))
}

/// Returns the path to the debug adapters directory
///
/// This is where debug adapters are downloaded to for DAPs that are built-in to Zed.
pub fn debug_adapters_dir() -> &'static PathBuf {
    static DEBUG_ADAPTERS_DIR: OnceLock<PathBuf> = OnceLock::new();
    DEBUG_ADAPTERS_DIR.get_or_init(|| data_dir().join("debug_adapters"))
}

/// Returns the path to the external agents directory
///
/// This is where agent servers are downloaded to
pub fn external_agents_dir() -> &'static PathBuf {
    static EXTERNAL_AGENTS_DIR: OnceLock<PathBuf> = OnceLock::new();
    EXTERNAL_AGENTS_DIR.get_or_init(|| data_dir().join("external_agents"))
}

/// Returns the path to the Copilot directory.
pub fn copilot_dir() -> &'static PathBuf {
    static COPILOT_DIR: OnceLock<PathBuf> = OnceLock::new();
    COPILOT_DIR.get_or_init(|| data_dir().join("copilot"))
}

/// Returns the path to the default Prettier directory.
pub fn default_prettier_dir() -> &'static PathBuf {
    static DEFAULT_PRETTIER_DIR: OnceLock<PathBuf> = OnceLock::new();
    DEFAULT_PRETTIER_DIR.get_or_init(|| data_dir().join("prettier"))
}

/// Returns the path to the remote server binaries directory.
pub fn remote_servers_dir() -> &'static PathBuf {
    static REMOTE_SERVERS_DIR: OnceLock<PathBuf> = OnceLock::new();
    REMOTE_SERVERS_DIR.get_or_init(|| data_dir().join("remote_servers"))
}

/// Returns the path to the directory where the devcontainer CLI is installed.
pub fn devcontainer_dir() -> &'static PathBuf {
    static DEVCONTAINER_DIR: OnceLock<PathBuf> = OnceLock::new();
    DEVCONTAINER_DIR.get_or_init(|| data_dir().join("devcontainer"))
}

/// Returns the relative path to a `.sawe` folder within a project.
pub fn local_settings_folder_name() -> &'static str {
    ".sawe"
}

/// Returns the relative path to a `.vscode` folder within a project.
pub fn local_vscode_folder_name() -> &'static str {
    ".vscode"
}

/// Returns the relative path to a `settings.json` file within a project.
pub fn local_settings_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".sawe/settings.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `tasks.json` file within a project.
pub fn local_tasks_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".sawe/tasks.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `run-configurations.json` file within a project.
pub fn local_run_configurations_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".sawe/run-configurations.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `.vscode/tasks.json` file within a project.
pub fn local_vscode_tasks_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".vscode/tasks.json").unwrap());
    *CACHED
}

pub fn debug_task_file_name() -> &'static str {
    "debug.json"
}

pub fn task_file_name() -> &'static str {
    "tasks.json"
}

/// Returns the relative path to a `debug.json` file within a project.
/// .sawe/debug.json
pub fn local_debug_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".sawe/debug.json").unwrap());
    *CACHED
}

/// Returns the relative path to a `.vscode/launch.json` file within a project.
pub fn local_vscode_launch_file_relative_path() -> &'static RelPath {
    static CACHED: LazyLock<&'static RelPath> =
        LazyLock::new(|| RelPath::unix(".vscode/launch.json").unwrap());
    *CACHED
}

pub fn user_ssh_config_file() -> PathBuf {
    home_dir().join(".ssh/config")
}

pub fn global_ssh_config_file() -> Option<&'static Path> {
    if cfg!(windows) {
        None
    } else {
        Some(Path::new("/etc/ssh/ssh_config"))
    }
}

/// Returns candidate paths for the vscode user settings file
pub fn vscode_settings_file_paths() -> Vec<PathBuf> {
    let mut paths = vscode_user_data_paths();
    for path in paths.iter_mut() {
        path.push("User/settings.json");
    }
    paths
}

/// Returns candidate paths for the cursor user settings file
pub fn cursor_settings_file_paths() -> Vec<PathBuf> {
    let mut paths = cursor_user_data_paths();
    for path in paths.iter_mut() {
        path.push("User/settings.json");
    }
    paths
}

fn vscode_user_data_paths() -> Vec<PathBuf> {
    // https://github.com/microsoft/vscode/blob/23e7148cdb6d8a27f0109ff77e5b1e019f8da051/src/vs/platform/environment/node/userDataPath.ts#L45
    const VSCODE_PRODUCT_NAMES: &[&str] = &[
        "Code",
        "Code - Insiders",
        "Code - OSS",
        "VSCodium",
        "VSCodium - Insiders",
        "Code Dev",
        "Code - OSS Dev",
        "code-oss-dev",
    ];
    let mut paths = Vec::new();
    if let Ok(portable_path) = env::var("VSCODE_PORTABLE") {
        paths.push(Path::new(&portable_path).join("user-data"));
    }
    if let Ok(vscode_appdata) = env::var("VSCODE_APPDATA") {
        for product_name in VSCODE_PRODUCT_NAMES {
            paths.push(Path::new(&vscode_appdata).join(product_name));
        }
    }
    for product_name in VSCODE_PRODUCT_NAMES {
        add_vscode_user_data_paths(&mut paths, product_name);
    }
    paths
}

fn cursor_user_data_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    add_vscode_user_data_paths(&mut paths, "Cursor");
    paths
}

fn add_vscode_user_data_paths(paths: &mut Vec<PathBuf>, product_name: &str) {
    if cfg!(target_os = "macos") {
        paths.push(
            home_dir()
                .join("Library/Application Support")
                .join(product_name),
        );
    } else if cfg!(target_os = "windows") {
        if let Some(data_local_dir) = dirs::data_local_dir() {
            paths.push(data_local_dir.join(product_name));
        }
        if let Some(data_dir) = dirs::data_dir() {
            paths.push(data_dir.join(product_name));
        }
    } else {
        paths.push(
            dirs::config_dir()
                .unwrap_or(home_dir().join(".config"))
                .join(product_name),
        );
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn global_gitignore_path() -> Option<PathBuf> {
    Some(home_dir().join(".config").join("git").join("ignore"))
}

#[cfg(not(any(test, feature = "test-support")))]
pub fn global_gitignore_path() -> Option<PathBuf> {
    static GLOBAL_GITIGNORE_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    GLOBAL_GITIGNORE_PATH
        .get_or_init(::ignore::gitignore::gitconfig_excludes_path)
        .clone()
}

#[cfg(test)]
mod rebrand_tests {
    use super::*;

    #[test]
    fn config_dir_contains_sawe() {
        let p = config_dir();
        assert!(
            p.to_string_lossy().contains("sawe") || p.to_string_lossy().contains("Sawe"),
            "config_dir should mention sawe; got {p:?}"
        );
        assert!(
            !p.to_string_lossy().to_ascii_lowercase().contains("zed"),
            "config_dir must not mention zed; got {p:?}"
        );
    }

    #[test]
    fn data_dir_contains_sawe() {
        let p = data_dir();
        assert!(
            p.to_string_lossy().contains("sawe") || p.to_string_lossy().contains("Sawe"),
            "data_dir should mention sawe; got {p:?}"
        );
        assert!(
            !p.to_string_lossy().to_ascii_lowercase().contains("zed"),
            "data_dir must not mention zed; got {p:?}"
        );
    }
}
