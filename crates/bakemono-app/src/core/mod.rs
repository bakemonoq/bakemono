pub mod catalog;
pub mod identity;
pub mod pipeline;
pub mod scrape;
pub mod source;

// generic daemon infrastructure now lives in bakemono-engine; re-exported so call sites stay put
pub use bakemono_engine::{config, data_dir, seeder};
