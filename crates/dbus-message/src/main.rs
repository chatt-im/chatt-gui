use std::env;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

use dbus_message::{
    FileChooserResponse, OpenFileOptions, SaveFileOptions, open_files, save_file,
    send_notification,
};

fn input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn next_utf8(
    args: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> io::Result<String> {
    args.next()
        .ok_or_else(|| input(format!("{option} requires a value")))?
        .into_string()
        .map_err(|_| input(format!("{option} must be UTF-8")))
}

fn print_response(response: FileChooserResponse) {
    match response {
        FileChooserResponse::Selected(paths) => {
            for path in paths {
                println!("{}", path.display());
            }
        }
        FileChooserResponse::Cancelled => eprintln!("cancelled"),
        FileChooserResponse::Other => eprintln!("portal ended the request without a selection"),
    }
}

fn open_command(mut args: impl Iterator<Item = OsString>) -> io::Result<()> {
    let mut options = OpenFileOptions::default();
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--multiple") => options.multiple = true,
            Some("--directory") => {
                options.directory = true;
                options.title = "Open Folder".into();
            }
            Some("--title") => options.title = next_utf8(&mut args, "--title")?,
            Some("--accept-label") => {
                options.accept_label = Some(next_utf8(&mut args, "--accept-label")?)
            }
            Some("--current-folder") => {
                options.current_folder = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| input("--current-folder requires a value"))?,
                ))
            }
            Some("--parent-window") => {
                options.parent_window = next_utf8(&mut args, "--parent-window")?
            }
            Some(option) => return Err(input(format!("unknown open option {option}"))),
            None => return Err(input("open options must be UTF-8")),
        }
    }
    print_response(open_files(options)?);
    Ok(())
}

fn save_command(mut args: impl Iterator<Item = OsString>) -> io::Result<()> {
    let mut options = SaveFileOptions::default();
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--title") => options.title = next_utf8(&mut args, "--title")?,
            Some("--accept-label") => {
                options.accept_label = Some(next_utf8(&mut args, "--accept-label")?)
            }
            Some("--current-name") => {
                options.current_name = Some(next_utf8(&mut args, "--current-name")?)
            }
            Some("--current-folder") => {
                options.current_folder = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| input("--current-folder requires a value"))?,
                ))
            }
            Some("--parent-window") => {
                options.parent_window = next_utf8(&mut args, "--parent-window")?
            }
            Some(option) => return Err(input(format!("unknown save option {option}"))),
            None => return Err(input("save options must be UTF-8")),
        }
    }
    print_response(save_file(options)?);
    Ok(())
}

fn usage() -> &'static str {
    "usage:
  dbus-message open [--multiple] [--directory] [--title TITLE]
                    [--accept-label LABEL] [--current-folder PATH]
                    [--parent-window IDENTIFIER]
  dbus-message save [--title TITLE] [--accept-label LABEL]
                    [--current-name NAME] [--current-folder PATH]
                    [--parent-window IDENTIFIER]
  dbus-message notify [TITLE] [BODY]"
}

fn main() -> io::Result<()> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref().and_then(|argument| argument.to_str()) {
        Some("open") => open_command(args),
        Some("save") => save_command(args),
        Some("notify") => {
            let title = args
                .next()
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| input("notification title must be UTF-8"))
                })
                .transpose()?
                .unwrap_or_else(|| "Rust Raw D-Bus".into());
            let body = args
                .next()
                .map(|value| {
                    value
                        .into_string()
                        .map_err(|_| input("notification body must be UTF-8"))
                })
                .transpose()?
                .unwrap_or_else(|| "Notification sent over a raw Unix socket!".into());
            if args.next().is_some() {
                return Err(input("notify accepts at most a title and body"));
            }
            send_notification(&title, &body)
        }
        Some("-h" | "--help" | "help") => {
            println!("{}", usage());
            Ok(())
        }
        Some(command) => Err(input(format!("unknown command {command}\n{}", usage()))),
        None => Err(input(usage())),
    }
}
