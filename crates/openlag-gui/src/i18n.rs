//! Language of the messages the backend writes itself (tray menu, terminal output of
//! the relay installer, notifications). The UI translates its own text.

use std::sync::atomic::{AtomicBool, Ordering};

static EN: AtomicBool = AtomicBool::new(false);

pub fn set(lang: &str) {
    EN.store(lang == "en", Ordering::Relaxed);
}

pub fn en() -> bool {
    EN.load(Ordering::Relaxed)
}

/// `tr!("русский {x}", "English {x}", ...)` formats the phrase in the current language.
#[macro_export]
macro_rules! tr {
    ($ru:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::i18n::en() { format!($en $(, $arg)*) } else { format!($ru $(, $arg)*) }
    };
}
