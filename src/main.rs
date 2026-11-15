#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod ui;

fn main() {
    #[cfg(windows)]
    {
        let args: Vec<_> = std::env::args_os().collect();
        if args.get(1).is_some_and(|a| a == "--apply-update") {
            if args.len() == 6
                && let Ok(pid) = args[2].to_string_lossy().parse()
                && let Err(error) = file_recovery::updates::apply_update(
                    pid,
                    std::path::Path::new(&args[3]),
                    std::path::Path::new(&args[4]),
                    &args[5].to_string_lossy(),
                )
            {
                ui::fatal(&error);
            }
            return;
        }
        if let Err(error) = ui::run() {
            ui::fatal(&error);
        }
    }
    #[cfg(not(windows))]
    eprintln!(
        "The native workspace requires Windows 10 or later. The recovery library is portable."
    );
}
