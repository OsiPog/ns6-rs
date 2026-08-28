//! The NS6's physical controls, in the order it is natural to walk them.
//!
//! `ns6 learn --guided` reads this list, asks for one control at a time, and
//! writes down whichever MIDI messages arrive. The result is the input half of
//! a Mixxx mapping; nothing here is guessed from the protocol, it is just the
//! panel legend.

/// A control to ask for, and the Mixxx group/key it should end up driving.
pub struct Item {
    /// What to ask the user to move, exactly as the panel labels it.
    pub prompt: &'static str,
    /// Short identifier used in the generated mapping.
    pub id: &'static str,
}

macro_rules! items {
    ($(($prompt:expr, $id:expr)),* $(,)?) => {
        &[$(Item { prompt: $prompt, id: $id }),*]
    };
}

/// Walked deck by deck, then the mixer, then the browse/FX section.
///
/// Deck 3 and 4 live under the same physical controls as 1 and 2 - the NS6
/// switches a side between decks - so they are asked for separately, after
/// flipping the deck selector, in case the controller re-uses the CC numbers on
/// a different channel.
pub const ITEMS: &[Item] = items![
    // --- Left platter / deck 1
    ("LEFT jog wheel: spin it (top surface)", "deck1.jog"),
    ("LEFT jog wheel: touch the top and let go", "deck1.jog_touch"),
    ("LEFT pitch fader: sweep it end to end", "deck1.pitch"),
    ("LEFT PLAY", "deck1.play"),
    ("LEFT CUE", "deck1.cue"),
    ("LEFT SYNC", "deck1.sync"),
    ("LEFT PITCH BEND -", "deck1.bend_down"),
    ("LEFT PITCH BEND +", "deck1.bend_up"),
    ("LEFT hot cue 1", "deck1.hotcue1"),
    ("LEFT hot cue 2", "deck1.hotcue2"),
    ("LEFT hot cue 3", "deck1.hotcue3"),
    ("LEFT AUTO LOOP knob: turn it", "deck1.loop_size"),
    ("LEFT AUTO LOOP knob: press it", "deck1.loop_toggle"),
    ("LEFT LOOP IN", "deck1.loop_in"),
    ("LEFT LOOP OUT", "deck1.loop_out"),
    ("LEFT RELOOP", "deck1.reloop"),
    ("LEFT CENSOR", "deck1.censor"),
    ("LEFT SCRATCH", "deck1.scratch"),
    ("LEFT strip search: slide a finger along it", "deck1.strip"),
    // --- Right platter / deck 2
    ("RIGHT jog wheel: spin it", "deck2.jog"),
    ("RIGHT jog wheel: touch the top and let go", "deck2.jog_touch"),
    ("RIGHT pitch fader: sweep it end to end", "deck2.pitch"),
    ("RIGHT PLAY", "deck2.play"),
    ("RIGHT CUE", "deck2.cue"),
    ("RIGHT SYNC", "deck2.sync"),
    ("RIGHT PITCH BEND -", "deck2.bend_down"),
    ("RIGHT PITCH BEND +", "deck2.bend_up"),
    ("RIGHT hot cue 1", "deck2.hotcue1"),
    ("RIGHT hot cue 2", "deck2.hotcue2"),
    ("RIGHT hot cue 3", "deck2.hotcue3"),
    ("RIGHT AUTO LOOP knob: turn it", "deck2.loop_size"),
    ("RIGHT AUTO LOOP knob: press it", "deck2.loop_toggle"),
    ("RIGHT LOOP IN", "deck2.loop_in"),
    ("RIGHT LOOP OUT", "deck2.loop_out"),
    ("RIGHT RELOOP", "deck2.reloop"),
    ("RIGHT CENSOR", "deck2.censor"),
    ("RIGHT SCRATCH", "deck2.scratch"),
    ("RIGHT strip search: slide a finger along it", "deck2.strip"),
    // --- Mixer
    ("Channel 1 fader", "mixer.ch1.volume"),
    ("Channel 2 fader", "mixer.ch2.volume"),
    ("Channel 3 fader", "mixer.ch3.volume"),
    ("Channel 4 fader", "mixer.ch4.volume"),
    ("Crossfader", "mixer.crossfader"),
    ("Channel 1 GAIN", "mixer.ch1.gain"),
    ("Channel 2 GAIN", "mixer.ch2.gain"),
    ("Channel 3 GAIN", "mixer.ch3.gain"),
    ("Channel 4 GAIN", "mixer.ch4.gain"),
    ("Channel 1 FILTER", "mixer.ch1.filter"),
    ("Channel 2 FILTER", "mixer.ch2.filter"),
    ("Channel 3 FILTER", "mixer.ch3.filter"),
    ("Channel 4 FILTER", "mixer.ch4.filter"),
    ("Channel 1 CUE/PFL", "mixer.ch1.pfl"),
    ("Channel 2 CUE/PFL", "mixer.ch2.pfl"),
    ("Channel 3 CUE/PFL", "mixer.ch3.pfl"),
    ("Channel 4 CUE/PFL", "mixer.ch4.pfl"),
    ("MASTER volume knob", "mixer.master"),
    ("BOOTH volume knob", "mixer.booth"),
    ("CUE MIX / blend knob", "mixer.cue_mix"),
    ("CUE GAIN / headphone volume knob", "mixer.cue_gain"),
    // --- Browse and FX
    ("BROWSE knob: turn it", "browse.knob"),
    ("BROWSE knob: press it", "browse.press"),
    ("BACK button", "browse.back"),
    ("LOAD A", "browse.load_a"),
    ("LOAD B", "browse.load_b"),
    ("LEFT FX knob", "fx.left.knob"),
    ("LEFT FX 1", "fx.left.b1"),
    ("LEFT FX 2", "fx.left.b2"),
    ("LEFT FX 3", "fx.left.b3"),
    ("RIGHT FX knob", "fx.right.knob"),
    ("RIGHT FX 1", "fx.right.b1"),
    ("RIGHT FX 2", "fx.right.b2"),
    ("RIGHT FX 3", "fx.right.b3"),
    // --- Deck selectors, asked last so the earlier answers stay valid
    ("LEFT deck selector: switch it to 3, then back to 1", "deck.sel_left"),
    ("RIGHT deck selector: switch it to 4, then back to 2", "deck.sel_right"),
];
