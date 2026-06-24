#!/usr/bin/env sh
set -eu

# Uninstalls spk-editor that was installed using the install.sh script

check_remaining_installations() {
    platform="$(uname -s)"
    if [ "$platform" = "Darwin" ]; then
        # Check for any SpkEditor variants in /Applications
        remaining=$(ls -d /Applications/SpkEditor*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    else
        # Check for any spk-editor variants in ~/.local
        remaining=$(ls -d "$HOME/.local/spk-editor"*.app 2>/dev/null | wc -l)
        [ "$remaining" -eq 0 ]
    fi
}

prompt_remove_preferences() {
    printf "Do you want to keep your spk-editor preferences? [Y/n] "
    read -r response
    case "$response" in
        [nN]|[nN][oO])
            rm -rf "$HOME/.config/spk-editor"
            echo "Preferences removed."
            ;;
        *)
            echo "Preferences kept."
            ;;
    esac
}

main() {
    platform="$(uname -s)"
    channel="${SPK_EDITOR_CHANNEL:-stable}"

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    "$platform"

    echo "spk-editor has been uninstalled"
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
        appid="ru.sipaha.spk-editor"
        db_suffix="stable"
        ;;
      nightly)
        appid="ru.sipaha.spk-editor-nightly"
        db_suffix="nightly"
        ;;
      preview)
        appid="ru.sipaha.spk-editor-preview"
        db_suffix="preview"
        ;;
      dev)
        appid="ru.sipaha.spk-editor-dev"
        db_suffix="dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="ru.sipaha.spk-editor"
        db_suffix="stable"
        ;;
    esac

    # Remove the app directory
    rm -rf "$HOME/.local/spk-editor$suffix.app"

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/spk-editor"

    # Remove the .desktop file
    rm -f "$HOME/.local/share/applications/${appid}.desktop"

    # Remove the database directory for this channel
    rm -rf "$HOME/.local/share/spk-editor/db/0-$db_suffix"

    # Remove socket file
    rm -f "$HOME/.local/share/spk-editor/spk-editor-$db_suffix.sock"

    # Remove the entire spk-editor directory if no installations remain
    if check_remaining_installations; then
        rm -rf "$HOME/.local/share/spk-editor"
        prompt_remove_preferences
    fi

    rm -rf $HOME/.zed_server
}

macos() {
    app="SpkEditor.app"
    db_suffix="stable"
    app_id="ru.sipaha.spk-editor"
    case "$channel" in
      nightly)
        app="SpkEditorNightly.app"
        db_suffix="nightly"
        app_id="ru.sipaha.spk-editor-nightly"
        ;;
      preview)
        app="SpkEditorPreview.app"
        db_suffix="preview"
        app_id="ru.sipaha.spk-editor-preview"
        ;;
      dev)
        app="SpkEditorDev.app"
        db_suffix="dev"
        app_id="ru.sipaha.spk-editor-dev"
        ;;
    esac

    # Remove the app bundle
    if [ -d "/Applications/$app" ]; then
        rm -rf "/Applications/$app"
    fi

    # Remove the binary symlink
    rm -f "$HOME/.local/bin/spk-editor"

    # Remove the database directory for this channel
    rm -rf "$HOME/Library/Application Support/SpkEditor/db/0-$db_suffix"

    # Remove app-specific files and directories
    rm -rf "$HOME/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/$app_id.sfl"*
    rm -rf "$HOME/Library/Caches/$app_id"
    rm -rf "$HOME/Library/HTTPStorages/$app_id"
    rm -rf "$HOME/Library/Preferences/$app_id.plist"
    rm -rf "$HOME/Library/Saved Application State/$app_id.savedState"

    # Remove the entire SpkEditor directory if no installations remain
    if check_remaining_installations; then
        rm -rf "$HOME/Library/Application Support/SpkEditor"
        rm -rf "$HOME/Library/Logs/SpkEditor"

        prompt_remove_preferences
    fi

    rm -rf $HOME/.zed_server
}

main "$@"
