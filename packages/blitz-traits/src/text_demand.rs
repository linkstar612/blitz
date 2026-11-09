//! A hook that sees text before parley shapes it (polyvox R-OLB.7).
//!
//! An embedder that registers fonts lazily (one face per script, mapped the
//! first time that script is drawn) has to know what a layout is about to
//! shape, before it shapes it: a face registered after shaping draws nothing
//! until the next relayout, and the first frame shows tofu or a fallback face.
//! Blitz reports two things:
//!
//! * [`Demand::Text`]: every string it hands to parley. Inline text and list
//!   markers as the inline layout is built, and the text of a text input or
//!   textarea before its editor lays it out (initial value, a `value` change,
//!   typed, pasted or IME text, a scale change).
//! * [`Demand::Family`]: the first family named in each `font-family` it
//!   converts for parley. A face named first shapes every character it covers,
//!   Latin included, so a `:lang(th)` run of plain digits draws from the Thai
//!   face only if that face is registered by then; the text alone cannot say so.
//!
//! The hook runs synchronously on the layout thread and must return only once
//! the fonts the demand needs are registered. With no hook installed a demand
//! is one atomic load.
//!
//! A face can also arrive later, off the layout thread (polyvox R-OMS.36, a
//! download the hook started). The embedder then calls [`fonts_changed`]; every
//! document compares [`font_generation`] at the start of its next resolve and
//! lays its inline text out again when it moved, the way a loaded `@font-face`
//! does. Waking the event loop for that resolve is the embedder's job.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static FONT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The fonts the shared collection holds changed outside a layout (a face
/// registered after the text that needed it was shaped). Returns the new
/// generation.
pub fn fonts_changed() -> u64 {
    FONT_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
}

/// Bumped by each [`fonts_changed`]; a document that saw an older value lays
/// its inline text out again.
#[inline]
pub fn font_generation() -> u64 {
    FONT_GENERATION.load(Ordering::Acquire)
}

/// What is about to be shaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Demand<'a> {
    /// Text about to be shaped.
    Text(&'a str),
    /// The first named family of a style about to shape text.
    Family(&'a str),
}

/// Called with each [`Demand`].
pub type TextDemandHook = fn(Demand<'_>);

static HOOK: OnceLock<TextDemandHook> = OnceLock::new();

/// Install the process-wide hook. The first call wins; later calls return
/// `false` and change nothing.
pub fn set_hook(hook: TextDemandHook) -> bool {
    HOOK.set(hook).is_ok()
}

/// Report `text` as about to be shaped. A no-op with no hook installed or for
/// empty text.
#[inline]
pub fn demand(text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(hook) = HOOK.get() {
        hook(Demand::Text(text));
    }
}

/// Report `family` as the first family of a style about to shape text.
#[inline]
pub fn demand_family(family: &str) {
    if let Some(hook) = HOOK.get() {
        hook(Demand::Family(family));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn record(demand: Demand<'_>) {
        SEEN.lock().unwrap().push(format!("{demand:?}"));
    }

    #[test]
    fn fonts_changed_moves_the_generation() {
        let before = font_generation();
        let after = fonts_changed();
        assert!(after > before);
        assert_eq!(font_generation(), after);
    }

    #[test]
    fn the_first_hook_sees_every_nonempty_demand() {
        demand("before any hook");
        assert!(set_hook(record));
        assert!(!set_hook(|_| panic!(
            "a second hook must not replace the first"
        )));
        demand("");
        demand("\u{0E44}\u{0E17}\u{0E22}");
        demand_family("Noto Sans Thai");
        assert_eq!(
            *SEEN.lock().unwrap(),
            vec![
                format!("{:?}", Demand::Text("\u{0E44}\u{0E17}\u{0E22}")),
                format!("{:?}", Demand::Family("Noto Sans Thai")),
            ]
        );
    }
}
