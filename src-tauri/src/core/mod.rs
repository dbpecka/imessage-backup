pub mod attachment_copy;
pub mod attachment_files;
// Reading AddressBook only happens on macOS, but the matching logic is
// platform-independent so it can be unit-tested anywhere.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub mod contacts;
pub mod db_path;
pub mod filter;
pub mod icloud;
pub mod messages_app;
pub mod op_lock;
pub mod paths;
pub mod progress;
pub mod receipts;
pub mod retained;
pub mod snapshot;
