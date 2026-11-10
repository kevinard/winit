//! Bounded recording policy; protocol dispatch owns protection and source state.

pub(crate) const CTRL: u8 = 1;
pub(crate) const ALT: u8 = 2;
pub(crate) const SHIFT: u8 = 4;
pub(crate) const NUM: u8 = 8;
pub(crate) const LOGO: u8 = 16;
const CHORD_MODIFIERS: u8 = CTRL | ALT | SHIFT | LOGO;
const ALL: u8 = CHORD_MODIFIERS | NUM;
const MAX_HELD: usize = 256;

/// Why a native recording attempt ended without changing the chosen trigger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReason {
    /// Explicit cancellation, including plain Escape.
    User,
    /// Focus, permission or keyboard-seat continuity was lost.
    ProtectionChanged,
    /// The keyboard map or layout changed.
    KeymapChanged,
    /// Input from more than one keyboard seat would mix a chord.
    AmbiguousSeat,
    /// The current symbol or effective modifiers cannot be represented.
    Unrepresentable,
    /// Native input or attempt counters exceeded their bounds.
    InputLimit,
    /// The last owning handle was dropped.
    Dropped,
}
/// Latest source-owned recording state; terminal states cannot revive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Draining input queued before the arm request.
    Preparing,
    /// Release keys already held before recording can begin.
    WaitingForRelease,
    /// Ready for a fresh press.
    Ready,
    /// A chord is being collected; release its keys to finish.
    Collecting,
    /// The canonical XDG trigger, without granting application authority.
    Recorded(String),
    /// The attempt ended without a recorded trigger.
    Cancelled(CancelReason),
    /// The native inhibitor, window or event loop retired.
    Closed,
}
impl State {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Recorded(_) | Self::Cancelled(_) | Self::Closed)
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Modifiers {
    pub bits: u8,
    pub unknown: bool,
}
impl Modifiers {
    fn supported(self) -> bool {
        !self.unknown && self.bits & !ALL == 0
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Key {
    pub name: String,
    pub modifiers: Modifiers,
}
fn implied(name: &str) -> u8 {
    match name {
        "Control_L" | "Control_R" => CTRL,
        "Alt_L" | "Alt_R" => ALT,
        "Shift_L" | "Shift_R" => SHIFT,
        "Super_L" | "Super_R" => LOGO,
        _ => 0,
    }
}
fn trigger(name: &str, modifiers: Modifiers) -> Option<String> {
    if name.is_empty()
        || name.len() > 128
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        || !modifiers.supported()
        || implied(name) & !modifiers.bits != 0
    {
        return None;
    }
    let mut value = String::new();
    for (bit, text) in [
        (CTRL, "CTRL+"),
        (ALT, "ALT+"),
        (SHIFT, "SHIFT+"),
        (NUM, "NUM+"),
        (LOGO, "LOGO+"),
    ] {
        if modifiers.bits & bit != 0 && implied(name) & bit == 0 {
            value.push_str(text);
        }
    }
    value.push_str(name);
    Some(value)
}

/// Logical down keys and consumed cycles survive an attempt's cancellation.
#[derive(Default)]
pub(crate) struct Held {
    down: Vec<u32>,
    consumed: Vec<u32>,
    invalid: bool,
}
impl Held {
    pub fn enter(&mut self, keys: impl IntoIterator<Item = u32>) {
        self.down.clear();
        self.invalid = false;
        for key in keys {
            if self.down.len() == MAX_HELD && !self.down.contains(&key) {
                self.invalid = true;
                break;
            }
            if !self.down.contains(&key) {
                self.down.push(key);
            }
        }
        // Enter is authoritative for releases which occurred outside this client.
        self.consumed.retain(|key| self.down.contains(key));
    }
    pub fn leave(&mut self) {
        self.down.clear();
        // Held input may return in a later Enter. Do not expose its orphan up.
    }
    pub fn empty(&self) -> bool {
        self.down.is_empty() && !self.invalid
    }
    pub fn invalidate(&mut self) {
        self.invalid = true;
    }
    pub fn valid(&self) -> bool {
        !self.invalid
    }
    /// Returns (fresh press, consumed). On overflow the new cycle is ordinary.
    pub fn edge(&mut self, code: u32, down: bool, consume: bool) -> (bool, bool) {
        if down {
            if self.down.len() == MAX_HELD && !self.down.contains(&code) {
                self.invalid = true;
                return (false, false);
            }
            let fresh = !self.down.contains(&code);
            if fresh {
                self.down.push(code);
            }
            if fresh && consume {
                if !self.consumed.contains(&code) {
                    self.consumed.push(code);
                }
            }
            (fresh, self.consumed.contains(&code))
        } else {
            self.down.retain(|key| *key != code);
            let consumed = self.consumed.contains(&code);
            self.consumed.retain(|key| *key != code);
            (false, consumed)
        }
    }
    pub fn consumes(&self, code: u32) -> bool {
        self.consumed.contains(&code)
    }
}

struct Pending {
    sequence: u64,
    key: Key,
}
/// One attempt, revoked synchronously by the native owner on discontinuity.
pub(crate) struct Attempt<S> {
    state: State,
    source: Option<S>,
    pending: Option<Pending>,
    sequence: u64,
    modifier: Option<String>,
    selected: Option<String>,
}
impl<S: Clone + Eq> Attempt<S> {
    pub fn new() -> Self {
        Self {
            state: State::Preparing,
            source: None,
            pending: None,
            sequence: 0,
            modifier: None,
            selected: None,
        }
    }
    pub fn state(&self) -> &State {
        &self.state
    }
    pub fn source(&self) -> Option<&S> {
        self.source.as_ref()
    }
    pub fn pending_sequence(&self) -> Option<u64> {
        self.pending.as_ref().map(|pending| pending.sequence)
    }
    pub fn cancel(&mut self, reason: CancelReason) {
        if !self.state.is_terminal() {
            self.state = State::Cancelled(reason);
            self.pending = None;
        }
    }
    pub fn close(&mut self) {
        if !self.state.is_terminal() {
            self.state = State::Closed;
            self.pending = None;
        }
    }
    pub fn armed(&mut self, all_released: bool) {
        if self.state == State::Preparing {
            self.state = if all_released {
                State::Ready
            } else {
                State::WaitingForRelease
            };
        }
    }
    /// The caller resolves an older press before delivering a later key edge.
    pub fn press(&mut self, seat: S, key: Option<Key>) -> Option<u64> {
        if self.state.is_terminal() {
            return None;
        }
        if key.as_ref().is_some_and(|key| {
            key.name == "Escape"
                && key.modifiers.supported()
                && key.modifiers.bits & CHORD_MODIFIERS == 0
        }) {
            self.cancel(CancelReason::User);
            return None;
        }
        if matches!(self.state, State::Preparing | State::WaitingForRelease) {
            return None;
        }
        if self.source.as_ref().is_some_and(|source| source != &seat) {
            self.cancel(CancelReason::AmbiguousSeat);
            return None;
        }
        let Some(key) = key.filter(|key| key.modifiers.supported()) else {
            self.cancel(CancelReason::Unrepresentable);
            return None;
        };
        self.source = Some(seat);
        self.state = State::Collecting;
        if self.selected.is_some() {
            return None;
        }
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.cancel(CancelReason::InputLimit);
            return None;
        };
        self.sequence = sequence;
        self.pending = Some(Pending { sequence, key });
        Some(sequence)
    }
    /// Use actual client state after the press, never a synthetic server clone.
    pub fn resolve(&mut self, modifiers: Option<Modifiers>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let Some(post) = modifiers else {
            self.cancel(CancelReason::Unrepresentable);
            return;
        };
        let Some(value) = trigger(&pending.key.name, post) else {
            self.cancel(CancelReason::Unrepresentable);
            return;
        };
        let modifier = implied(&pending.key.name) != 0
            || (pending.key.modifiers.bits ^ post.bits) & CHORD_MODIFIERS != 0;
        if modifier {
            self.modifier = Some(value);
        } else {
            self.selected = Some(value);
        }
    }
    pub fn released(&mut self, all_released: bool) {
        if !all_released {
            return;
        }
        if self.state == State::WaitingForRelease {
            self.state = State::Ready;
        } else if self.state == State::Collecting && self.pending.is_none() {
            if let Some(value) = self.selected.take().or_else(|| self.modifier.take()) {
                self.state = State::Recorded(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn mods(bits: u8) -> Modifiers {
        Modifiers {
            bits,
            unknown: false,
        }
    }
    fn key(name: &str, bits: u8) -> Option<Key> {
        Some(Key {
            name: name.into(),
            modifiers: mods(bits),
        })
    }
    fn ready() -> Attempt<u8> {
        let mut attempt = Attempt::new();
        attempt.armed(true);
        attempt
    }
    #[test]
    fn queued_and_already_held_keys_cannot_become_the_recorded_trigger() {
        let mut a = Attempt::new();
        assert_eq!(a.press(1, key("space", 0)), None);
        a.armed(false);
        a.press(1, key("a", 0));
        a.released(false);
        assert_eq!(a.state(), &State::WaitingForRelease);
        a.released(true);
        assert_eq!(a.state(), &State::Ready);
        a.press(1, key("F9", 0));
        a.resolve(Some(mods(0)));
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("F9".into()));
    }
    #[test]
    fn control_a_keeps_its_press_snapshot_when_control_is_released_first() {
        let mut a = ready();
        a.press(1, key("Control_L", 0));
        a.resolve(Some(mods(CTRL)));
        a.press(1, key("a", CTRL));
        a.resolve(Some(mods(CTRL)));
        a.released(false);
        a.resolve(Some(mods(0)));
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("CTRL+a".into()));
    }
    #[test]
    fn base_symbols_and_post_press_modifiers_handle_shift_and_remapped_caps() {
        let mut a = ready();
        a.press(1, key("Caps_Lock", 0));
        a.resolve(Some(mods(CTRL)));
        assert_eq!(a.state(), &State::Collecting);
        a.press(1, key("a", CTRL | SHIFT));
        a.resolve(Some(mods(CTRL | SHIFT)));
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("CTRL+SHIFT+a".into()));
        let mut a = ready();
        a.press(1, key("Caps_Lock", 0));
        a.resolve(Some(mods(CTRL)));
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("CTRL+Caps_Lock".into()));
    }
    #[test]
    fn bare_modifiers_and_lock_keys_use_actual_post_press_state() {
        let mut a = ready();
        a.press(1, key("Control_R", 0));
        a.resolve(Some(mods(CTRL)));
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("Control_R".into()));
        let mut a = ready();
        a.press(1, key("Num_Lock", 0));
        a.resolve(Some(mods(NUM)));
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("NUM+Num_Lock".into()));
    }
    #[test]
    fn unknown_modifiers_missing_implied_modifiers_and_invalid_symbols_refuse_success() {
        for (name, post) in [
            (
                "e",
                Modifiers {
                    bits: 0,
                    unknown: true,
                },
            ),
            ("Control_L", mods(0)),
            ("bad+name", mods(0)),
        ] {
            let mut a = ready();
            a.press(1, key(name, 0));
            a.resolve(Some(post));
            a.released(true);
            assert_eq!(a.state(), &State::Cancelled(CancelReason::Unrepresentable));
        }
    }
    #[test]
    fn first_normal_key_wins_and_modifier_only_chords_wait_for_release() {
        let mut a = ready();
        a.press(1, key("Control_L", 0));
        a.resolve(Some(mods(CTRL)));
        a.press(1, key("Shift_L", CTRL));
        a.resolve(Some(mods(CTRL | SHIFT)));
        a.released(false);
        assert_eq!(a.state(), &State::Collecting);
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("CTRL+Shift_L".into()));
        let mut a = ready();
        a.press(1, key("a", 0));
        a.resolve(Some(mods(0)));
        assert_eq!(a.press(1, key("b", 0)), None);
        a.released(true);
        assert_eq!(a.state(), &State::Recorded("a".into()));
    }
    #[test]
    fn protection_loss_and_close_cannot_revive_after_late_fences_or_presses() {
        for reason in [
            CancelReason::ProtectionChanged,
            CancelReason::KeymapChanged,
            CancelReason::Dropped,
            CancelReason::InputLimit,
        ] {
            let mut a = ready();
            a.press(1, key("a", 0));
            a.cancel(reason);
            a.armed(true);
            a.resolve(Some(mods(0)));
            a.released(true);
            a.press(1, key("b", 0));
            assert_eq!(a.state(), &State::Cancelled(reason));
        }
        let mut a = ready();
        a.close();
        a.press(1, key("a", 0));
        assert_eq!(a.state(), &State::Closed);
    }
    #[test]
    fn escape_and_another_seat_cancel_only_the_attempt() {
        let mut a = ready();
        a.press(1, key("Escape", 0));
        assert_eq!(a.state(), &State::Cancelled(CancelReason::User));
        let mut escape = ready();
        escape.press(1, key("Escape", NUM));
        assert_eq!(escape.state(), &State::Cancelled(CancelReason::User));
        let mut a = ready();
        a.press(1, key("a", 0));
        assert_eq!(a.source(), Some(&1));
        assert_eq!(a.pending_sequence(), Some(1));
        a.resolve(Some(mods(0)));
        a.press(2, key("b", 0));
        assert_eq!(a.state(), &State::Cancelled(CancelReason::AmbiguousSeat));
    }
    #[test]
    fn consumed_cycles_survive_cancel_focus_loss_and_repeats() {
        let mut h = Held::default();
        h.enter([42]);
        assert!(!h.empty());
        assert_eq!(h.edge(30, true, true), (true, true));
        assert_eq!(h.edge(30, true, false), (false, true));
        assert!(h.consumes(30));
        h.leave();
        h.enter([30]);
        assert!(h.consumes(30));
        assert_eq!(h.edge(30, false, false), (false, true));
        assert!(h.empty());
        h.edge(31, true, true);
        h.leave();
        h.enter([]);
        assert!(!h.consumes(31));
    }
    #[test]
    fn malformed_enter_and_press_growth_stay_bounded_and_require_a_fresh_enter() {
        let mut h = Held::default();
        h.enter(0..300);
        assert!(!h.valid());
        assert_eq!(h.down.len(), 256);
        h.enter(0..256);
        assert!(h.valid());
        assert_eq!(h.edge(1000, true, true), (false, false));
        assert!(!h.valid());
        h.enter([]);
        h.invalidate();
        assert!(!h.empty());
        h.enter([]);
        assert!(h.valid());
        assert!(h.empty());
    }
}
