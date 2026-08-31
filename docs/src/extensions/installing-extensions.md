---
title: Installing Extensions
description: "Browse, install, and manage extensions from the Zed Extension Gallery."
---

# Installing Extensions {#installing-extensions}

Extensions add functionality to Zed, including languages, themes, and AI tools. Browse and install them from the Extension Gallery.

Open the Extension Gallery with {#kb zed::Extensions}, or select "Zed > Extensions" from the menu bar.

## Installation Location

Extensions are installed in `~/.spk/sawe/data/extensions`, on every platform.

This directory contains two subdirectories:

- `installed`, which contains the source code for each extension.
- `work` which contains files created by the extension itself, such as downloaded language servers.

## Auto-installing

To automate extension installation/uninstallation see the docs for [auto_install_extensions](../reference/all-settings.md#auto-install-extensions).
