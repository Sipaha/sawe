#!/usr/bin/env sh
set -eu

# Uninstalls sawe that was installed using the install.sh script
#
# Deliberately does NOT touch $HOME/.zed_server (upstream's uninstaller does).
# `.zed_server` is a preserved shared identifier -- paths::remote_server_dir_relative()
# returns it unchanged -- so a real Zed uses the same directory, and the binaries
# inside it are named `zed-remote-server-<channel>-<version>` by both products, so
# ours cannot even be told apart from theirs. It is also populated by *incoming*
# remote connections rather than by installing this editor, so removing it would
# delete another product's files that this install never created.

check_remaining_installations() {
    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        # Check for any Sawe variants in /Applications
        remaining=$(ls -d /Applications/Sawe*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    else
        # Check for any sawe variants in ~/.local
        remaining=$(ls -d "$HOME/.local/sawe"*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    fi
}

# Mirror of `paths::base_dir()` -- `home_dir()/.spk/<dir_name_kebab()>` in
# crates/paths/src/paths.rs. Every profile directory this editor writes hangs
# off that single root on *every* platform: there is no `~/.config/sawe`, no
# `~/Library/Application Support/Sawe` and no `%APPDATA%\Sawe`, which is why
# the three paths this script used to remove were paths the binary had never
# created, and why its "keep your preferences?" prompt had no effect either way.
#
# Only two environment variables move it, so only two are mirrored here:
# `SAWE_HOME` is the sole override `util::paths::home_dir()` reads, and
# `SAWE_DEV_DIRS` is the sole override of the `-dev` suffix (whose default is
# off for the release builds this script uninstalls). If this disagrees with
# the binary, we remove a directory nobody uses and leave the real one behind.
profile_dir() {
    dir="sawe"
    case "$(printf '%s' "${SAWE_DEV_DIRS:-}" | tr '[:upper:]' '[:lower:]')" in
        1|true|yes|on) dir="sawe-dev" ;;
    esac
    printf '%s/.spk/%s' "${SAWE_HOME:-$HOME}" "$dir"
}

# The only state under the profile root that belongs to *this* release channel:
# its database scope (`db_path()` in crates/db -- `data/db/0-<channel>`, next to
# a `0-global` scope shared by all channels) and the datagram socket the CLI
# hands its arguments to (`paths::cli_ipc_socket_in`). Config, logs, cache and
# state are shared by every channel installed for this user, so they can only go
# once the last install does.
remove_channel_state() {
    base="$(profile_dir)"
    rm -rf "$base/data/db/0-$db_suffix"
    rm -f "$base/data/sawe-$channel.sock"
}

# Called only when no installation remains. Removes the profile root's
# app-owned subdirectories by name rather than the root itself, because
# `$base/ss` is the Solutions root (`solutions::settings::default_root`) and
# holds the user's own project checkouts -- an uninstaller must not delete
# source trees. `config` is left to prompt_remove_preferences.
remove_shared_state() {
    base="$(profile_dir)"
    rm -rf "$base/data" "$base/state" "$base/cache" "$base/logs"
}

prompt_remove_preferences() {
    printf "Do you want to keep your sawe preferences? [Y/n] "
    read -r response
    case "$response" in
        [nN]|[nN][oO])
            rm -rf "$(profile_dir)/config"
            echo "Preferences removed."
            ;;
        *)
            echo "Preferences kept."
            ;;
    esac
    # Tidy the root away if nothing is left in it, but never recursively: a
    # surviving `ss/` (or a config the user chose to keep) must stay.
    rmdir "$(profile_dir)" 2>/dev/null || true
}

main() {
    platform="$(uname -s)"
    channel="${SAWE_CHANNEL:-stable}"

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    "$platform"

    echo "sawe has been uninstalled"
}

linux() {
    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    appid=""
    db_suffix="stable"
    case "$channel" in
      stable)
        appid="ru.sipaha.sawe"
        db_suffix="stable"
        ;;
      nightly)
        appid="ru.sipaha.sawe-nightly"
        db_suffix="nightly"
        ;;
      preview)
        appid="ru.sipaha.sawe-preview"
        db_suffix="preview"
        ;;
      dev)
        appid="ru.sipaha.sawe-dev"
        db_suffix="dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="ru.sipaha.sawe"
        db_suffix="stable"
        ;;
    esac

    # Remove the app directory
    rm -rf "$HOME/.local/sawe$suffix.app"

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/sawe"

    # Remove the .desktop file
    rm -f "$HOME/.local/share/applications/${appid}.desktop"

    remove_channel_state

    if check_remaining_installations; then
        remove_shared_state
        prompt_remove_preferences
    fi
}

macos() {
    app="Sawe.app"
    db_suffix="stable"
    app_id="ru.sipaha.sawe"
    case "$channel" in
      nightly)
        app="SaweNightly.app"
        db_suffix="nightly"
        app_id="ru.sipaha.sawe-nightly"
        ;;
      preview)
        app="SawePreview.app"
        db_suffix="preview"
        app_id="ru.sipaha.sawe-preview"
        ;;
      dev)
        app="SaweDev.app"
        db_suffix="dev"
        app_id="ru.sipaha.sawe-dev"
        ;;
    esac

    # Remove the app bundle
    if [ -d "/Applications/$app" ]; then
        rm -rf "/Applications/$app"
    fi

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/sawe"

    remove_channel_state

    # Remove the files macOS itself keeps for our bundle id. These are the only
    # `~/Library` paths involved: `paths::logs_dir()` is `<profile>/logs`, not
    # `~/Library/Logs/Sawe`, and there is no `~/Library/Application
    # Support/Sawe`. `~/Library/Logs/DiagnosticReports` is deliberately absent
    # too -- `paths::crashes_dir()` reads it, but it is the OS-wide crash log
    # directory shared with every other application.
    rm -rf "$HOME/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/$app_id.sfl"*
    rm -rf "$HOME/Library/Caches/$app_id"
    rm -rf "$HOME/Library/HTTPStorages/$app_id"
    rm -rf "$HOME/Library/Preferences/$app_id.plist"
    rm -rf "$HOME/Library/Saved Application State/$app_id.savedState"

    if check_remaining_installations; then
        remove_shared_state
        prompt_remove_preferences
    fi
}

main "$@"
