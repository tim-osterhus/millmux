pub mod error;
pub mod events;
pub mod ids;
pub mod paths;
pub mod protocol;
pub mod redaction;
pub mod scrollback;
pub mod state;
pub mod storage;
pub mod width;
pub mod workspace;

pub const PRODUCT_NAME: &str = "millrace-sessions";
pub fn product_name() -> &'static str {
    PRODUCT_NAME
}
