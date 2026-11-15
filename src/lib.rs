pub mod engine;
pub mod fat;
pub mod i18n;
pub mod partition;
pub mod preview;
pub mod restore;
pub mod settings;
pub mod source;
pub mod updates;

pub type Result<T> = std::result::Result<T, String>;

pub(crate) fn io_error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests;
