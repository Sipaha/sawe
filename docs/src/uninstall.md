---
title: Uninstall
description: "This guide covers how to uninstall Sawe on different operating systems."
---

# Uninstall

This guide covers how to uninstall Sawe on different operating systems.

## macOS

### Standard Installation

If you installed Sawe as an application bundle:

1. Quit Sawe if it's running
2. Open Finder and go to your Applications folder
3. Drag Sawe to the Trash (or right-click and select "Move to Trash")
4. Empty the Trash

### Removing User Data (Optional)

To completely remove all Sawe configuration files and data:

1. Open Finder
2. Press `Cmd + Shift + G` to open "Go to Folder"
3. Delete the following if they exist:
   - `~/.spk/sawe`
   - `~/Library/Saved Application State/ru.sipaha.sawe.savedState`
   - `~/Library/Caches/ru.sipaha.sawe`
   - `~/Library/HTTPStorages/ru.sipaha.sawe`
   - `~/Library/Preferences/ru.sipaha.sawe.plist`
   - `~/Library/Application Support/com.apple.sharedfilelist/com.apple.LSSharedFileList.ApplicationRecentDocuments/ru.sipaha.sawe.sfl*`

## Linux

### Standard Uninstall

If Sawe was installed using the install script, run:

```sh
sawe --uninstall
```

If no other Sawe installation remains, you'll be asked whether to keep your preferences. After making a choice, you should see a message that Sawe was successfully uninstalled. That prompt does not cover your configuration and data: those live in `~/.spk/sawe`, which the uninstaller leaves in place. Delete it as well for a clean removal.

If the `sawe` command is not found in your PATH, try:

```sh
$HOME/.local/bin/sawe --uninstall
```

or:

```sh
$HOME/.local/sawe.app/bin/sawe --uninstall
```

### Package Manager

If you installed Sawe using a package manager (such as Flatpak, Snap, or a distribution-specific package manager), consult that package manager's documentation for uninstallation instructions.

### Manual Removal

If the uninstall command fails or Sawe was installed to a custom location, you can manually remove:

- Installation directory: `~/.local/sawe.app` (or your custom installation path)
- Binary symlink: `~/.local/bin/sawe`
- Desktop entry: `~/.local/share/applications/ru.sipaha.sawe.desktop`
- Configuration and data: `~/.spk/sawe`

## Windows

### Standard Installation

1. Quit Sawe if it's running
2. Open Settings (Windows key + I)
3. Go to "Apps" > "Installed apps" (or "Apps & features" on Windows 10)
4. Search for "Sawe"
5. Click the three dots menu next to Sawe and select "Uninstall"
6. Follow the prompts to complete the uninstallation

Alternatively, you can:

1. Open the Start menu
2. Right-click on Sawe
3. Select "Uninstall"

### Removing User Data (Optional)

To completely remove all Sawe configuration files and data:

1. Press `Windows key + R` to open Run
2. Type `%USERPROFILE%\.spk` and press Enter
3. Delete the `sawe` folder if it exists

## Troubleshooting

If you encounter issues during uninstallation:

- **macOS/Windows**: Ensure Sawe is completely quit before attempting to uninstall. Check Activity Manager (macOS) or Task Manager (Windows) for any running Sawe processes.
- **Linux**: If the uninstall script fails, check the error message and consider manual removal of the directories listed above.
- **All platforms**: If you want to start fresh while keeping Sawe installed, you can delete the configuration directories instead of uninstalling the application entirely.

For additional help, see our [Linux-specific documentation](./linux.md) or visit the [Zed community](https://zed.dev/community-links).
