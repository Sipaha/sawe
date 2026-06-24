#!/usr/bin/env sh
set -eu

# Installs spk-editor from a local tarball (e.g., ZED_BUNDLE_PATH environment variable).
# For instructions on building spk-editor, see https://github.com/Sipaha/spk-editor

main() {
    platform="$(uname -s)"
    arch="$(uname -m)"
    channel="${SPK_EDITOR_CHANNEL:-stable}"
    SPK_EDITOR_VERSION="${SPK_EDITOR_VERSION:-latest}"
    # Use TMPDIR if available (for environments with non-standard temp directories)
    if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
        temp="$(mktemp -d "$TMPDIR/spk-editor-XXXXXX")"
    else
        temp="$(mktemp -d "/tmp/spk-editor-XXXXXX")"
    fi

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    case "$platform-$arch" in
        macos-arm64* | linux-arm64* | linux-armhf | linux-aarch64)
            arch="aarch64"
            ;;
        macos-x86* | linux-x86* | linux-i686*)
            arch="x86_64"
            ;;
        *)
            echo "Unsupported platform or architecture"
            exit 1
            ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        curl () {
            command curl -fL "$@"
        }
    elif command -v wget >/dev/null 2>&1; then
        curl () {
            wget -O- "$@"
        }
    else
        echo "Could not find 'curl' or 'wget' in your path"
        exit 1
    fi

    "$platform" "$@"

    if [ "$(command -v spk-editor)" = "$HOME/.local/bin/spk-editor" ]; then
        echo "spk-editor has been installed. Run with 'spk-editor'"
    else
        echo "To run spk-editor from your terminal, you must add ~/.local/bin to your PATH"
        echo "Run:"

        case "$SHELL" in
            *zsh)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
                echo "   source ~/.zshrc"
                ;;
            *fish)
                echo "   fish_add_path -U $HOME/.local/bin"
                ;;
            *)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
                echo "   source ~/.bashrc"
                ;;
        esac

        echo "To run spk-editor now, '~/.local/bin/spk-editor'"
    fi
}

linux() {
    if [ -n "${SPK_EDITOR_BUNDLE_PATH:-}" ]; then
        cp "$SPK_EDITOR_BUNDLE_PATH" "$temp/spk-editor-linux-$arch.tar.gz"
    else
        echo "spk-editor must be built from source. See https://github.com/Sipaha/spk-editor for instructions."
        exit 1
    fi

    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    appid=""
    case "$channel" in
      stable)
        appid="ru.sipaha.spk-editor"
        ;;
      nightly)
        appid="ru.sipaha.spk-editor-nightly"
        ;;
      preview)
        appid="ru.sipaha.spk-editor-preview"
        ;;
      dev)
        appid="ru.sipaha.spk-editor-dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="ru.sipaha.spk-editor"
        ;;
    esac

    # Unpack
    rm -rf "$HOME/.local/spk-editor$suffix.app"
    mkdir -p "$HOME/.local/spk-editor$suffix.app"
    tar -xzf "$temp/spk-editor-linux-$arch.tar.gz" -C "$HOME/.local/"

    # Setup ~/.local directories
    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"

    # Link the binary
    if [ -f "$HOME/.local/spk-editor$suffix.app/bin/spk-editor" ]; then
        ln -sf "$HOME/.local/spk-editor$suffix.app/bin/spk-editor" "$HOME/.local/bin/spk-editor"
    else
        # support for versions before 0.139.x.
        ln -sf "$HOME/.local/spk-editor$suffix.app/bin/cli" "$HOME/.local/bin/spk-editor"
    fi

    # Copy .desktop file
    desktop_file_path="$HOME/.local/share/applications/${appid}.desktop"
    src_dir="$HOME/.local/spk-editor$suffix.app/share/applications"
    if [ -f "$src_dir/${appid}.desktop" ]; then
        cp "$src_dir/${appid}.desktop" "${desktop_file_path}"
    else
        # Fallback for older tarballs
        cp "$src_dir/spk-editor$suffix.desktop" "${desktop_file_path}"
    fi
    sed -i "s|Icon=spk-editor|Icon=$HOME/.local/spk-editor$suffix.app/share/icons/hicolor/512x512/apps/spk-editor.png|g" "${desktop_file_path}"
    sed -i "s|Exec=spk-editor|Exec=$HOME/.local/spk-editor$suffix.app/bin/spk-editor|g" "${desktop_file_path}"
}

macos() {
    echo "spk-editor must be built from source. See https://github.com/Sipaha/spk-editor for instructions."
    exit 1
}

main "$@"
