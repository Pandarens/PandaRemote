//! The host's side of the shared clipboard: text the viewer copied goes onto this PC's clipboard, and text
//! copied here goes to the viewer. Only text, up to `MAX_CLIPBOARD`.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::proto::MAX_CLIPBOARD;

/// Last text that crossed the link, so a value we just set isn't sent straight back.
#[derive(Clone, Default)]
pub struct Shared(Arc<Mutex<Option<String>>>);

impl Shared {
    /// Puts the viewer's text on this PC's clipboard.
    pub fn set(&self, text: String) {
        if text.len() > MAX_CLIPBOARD {
            return;
        }
        *self.0.lock().unwrap() = Some(text.clone());
        // The clipboard belongs to whatever desktop is current; a short-lived handle per call follows it.
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => {}
            Err(e) => tracing::debug!("clipboard set failed: {e}"),
        }
    }

    /// Watches this PC's clipboard and calls `changed` with new text until `stop` is set.
    pub fn watch(&self, stop: Arc<AtomicBool>, changed: impl Fn(String) + Send + 'static) {
        let last = self.0.clone();
        let _ = std::thread::Builder::new().name("clipboard".into()).spawn(move || {
            // Whatever is on the clipboard when the session starts stays here.
            *last.lock().unwrap() = read();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let Some(text) = read() else { continue };
                let mut last = last.lock().unwrap();
                if last.as_deref() != Some(text.as_str()) && text.len() <= MAX_CLIPBOARD {
                    *last = Some(text.clone());
                    drop(last);
                    changed(text);
                }
            }
        });
    }
}

fn read() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok().filter(|t| !t.is_empty())
}
