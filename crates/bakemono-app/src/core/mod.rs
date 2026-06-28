pub mod catalog;
pub mod identity;
pub mod pipeline;
pub mod scrape;

// generic daemon infrastructure now lives in bakemono-daemon; re-exported so call sites stay put
pub use bakemono_daemon::{config, data_dir, seeder};
