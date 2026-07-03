// release builds run windowless: no console attaches on Windows launch (dev keeps the console for logs)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    #[cfg(feature = "gui")]
    bakemono_app::gui::run();

    #[cfg(not(feature = "gui"))]
    eprintln!("bakemono-app built without the `gui` feature; use the scrapetest harness instead");
}
