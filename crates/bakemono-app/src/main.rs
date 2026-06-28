fn main() {
    #[cfg(feature = "gui")]
    bakemono_app::gui::run();

    #[cfg(not(feature = "gui"))]
    eprintln!("bakemono-app built without the `gui` feature; use the scrapetest harness instead");
}
