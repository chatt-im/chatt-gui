# Raw D-Bus desktop portal client

This crate implements the XDG Desktop Portal file chooser directly over the
session bus Unix socket. It does not depend on `zbus`, `ashpd`, GLib, GTK, or a
command-line dialog helper.

The only crate dependency is `libc`, used to obtain the effective user ID for
D-Bus `EXTERNAL` authentication.

## CLI

Open one or more files:

```sh
cargo run -- open --multiple --title "Choose attachments" --accept-label Upload
```

Choose a destination:

```sh
cargo run -- save --current-name attachment.bin --current-folder /tmp
```

Passing `--directory` to `open` selects a directory. Both commands also accept
`--parent-window`; an empty parent identifier is valid and is the default.

Successful selections are printed one path per line. Cancellation is successful
and prints `cancelled` to standard error.

The original raw notification example remains available:

```sh
cargo run -- notify "Title" "Body"
```

## Library

Use `open_files(OpenFileOptions)` or `save_file(SaveFileOptions)`. Both return a
`FileChooserResponse` containing selected `PathBuf` values, cancellation, or the
portal's generic non-success response.

Each chooser uses a dedicated session-bus connection. The implementation:

1. authenticates with the D-Bus `EXTERNAL` mechanism and calls `Hello`;
2. starts and resolves `org.freedesktop.portal.Desktop`;
3. subscribes to `org.freedesktop.portal.Request::Response` before requesting a
   dialog;
4. sends `OpenFile` or `SaveFile` with a random `handle_token`;
5. waits without a method timeout while the user interacts with the dialog;
6. verifies the portal's unique sender and request object path; and
7. decodes local `file://` URIs without assuming UTF-8 filesystem paths.

Protocol references used by this prototype are available locally in
`/tmp/xdg-desktop-portal` and `/tmp/dbus`.
