#!/usr/bin/env sh
set -eu

# Installs sawe from a local tarball (e.g., ZED_BUNDLE_PATH environment variable).
# For instructions on building sawe, see https://github.com/Sipaha/sawe

main() {
    platform="$(uname -s)"
    arch="$(uname -m)"
    channel="${SAWE_CHANNEL:-stable}"
    SAWE_VERSION="${SAWE_VERSION:-latest}"
    # Use TMPDIR if available (for environments with non-standard temp directories)
    if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
        temp="$(mktemp -d "$TMPDIR/sawe-XXXXXX")"
    else
        temp="$(mktemp -d "/tmp/sawe-XXXXXX")"
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

    if [ "$(command -v sawe)" = "$HOME/.local/bin/sawe" ]; then
        echo "sawe has been installed. Run with 'sawe'"
    else
        echo "To run sawe from your terminal, you must add ~/.local/bin to your PATH"
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

        echo "To run sawe now, '~/.local/bin/sawe'"
    fi
}

linux() {
    if [ -n "${SAWE_BUNDLE_PATH:-}" ]; then
        cp "$SAWE_BUNDLE_PATH" "$temp/sawe-linux-$arch.tar.gz"
    else
        echo "sawe must be built from source. See https://github.com/Sipaha/sawe for instructions."
        exit 1
    fi

    suffix=""
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi

    appid=""
    case "$channel" in
      stable)
        appid="ru.sipaha.sawe"
        ;;
      nightly)
        appid="ru.sipaha.sawe-nightly"
        ;;
      preview)
        appid="ru.sipaha.sawe-preview"
        ;;
      dev)
        appid="ru.sipaha.sawe-dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="ru.sipaha.sawe"
        ;;
    esac

    # Unpack
    rm -rf "$HOME/.local/sawe$suffix.app"
    mkdir -p "$HOME/.local/sawe$suffix.app"
    tar -xzf "$temp/sawe-linux-$arch.tar.gz" -C "$HOME/.local/"

    # Setup ~/.local directories
    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"

    # Link the binary
    if [ -f "$HOME/.local/sawe$suffix.app/bin/sawe" ]; then
        ln -sf "$HOME/.local/sawe$suffix.app/bin/sawe" "$HOME/.local/bin/sawe"
    else
        # support for versions before 0.139.x.
        ln -sf "$HOME/.local/sawe$suffix.app/bin/cli" "$HOME/.local/bin/sawe"
    fi

    # Copy .desktop file
    desktop_file_path="$HOME/.local/share/applications/${appid}.desktop"
    src_dir="$HOME/.local/sawe$suffix.app/share/applications"
    if [ -f "$src_dir/${appid}.desktop" ]; then
        cp "$src_dir/${appid}.desktop" "${desktop_file_path}"
    else
        # Fallback for older tarballs
        cp "$src_dir/sawe$suffix.desktop" "${desktop_file_path}"
    fi
    sed -i "s|Icon=sawe|Icon=$HOME/.local/sawe$suffix.app/share/icons/hicolor/512x512/apps/sawe.png|g" "${desktop_file_path}"
    sed -i "s|Exec=sawe|Exec=$HOME/.local/sawe$suffix.app/bin/sawe|g" "${desktop_file_path}"
}

macos() {
    echo "sawe must be built from source. See https://github.com/Sipaha/sawe for instructions."
    exit 1
}

main "$@"
