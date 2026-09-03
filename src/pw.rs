//! PipeWire nodes, so the controller is an ordinary sound card.
//!
//! Nothing in the kernel binds this device, so nothing in the sound stack knows
//! it exists: no ALSA card, no sink, no source. The rest of the audio path here
//! streams through a file path - a pipe, a file, stdout - which proves the
//! hardware but leaves that gap open. Something else still has to have created
//! a device for the driver to be the far end of, and a FIFO carries no clock,
//! no format and no notion of a device arriving or leaving.
//!
//! These two nodes close it. They *are* the device in the graph: a sink whose
//! frames go out of the controller and a source carrying what its mixer sends
//! back, both present for exactly as long as the driver runs and gone with it.
//!
//! Both speak s32le at 44.1 kHz, the device's own rate. The sink carries four
//! channels, because the device has two outputs: the **master** on 1-2 and the
//! **headphone jack** on 3-4. The source carries the stereo the device sends
//! back. PipeWire's adapter converts whatever a client speaks into that, so
//! nothing here resamples - the device's clock and the graph's are still
//! independent, and a long session still drifts.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::Duration;

use pipewire as pw;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw, MAX_CHANNELS};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::{Direction, SpaTypes};
use pw::stream::{Stream, StreamFlags, StreamState};
use pw::types::ObjectType;
use pw::{properties::properties, spa};

use crate::audio;
use crate::iso;

/// The device's rate. It has no other.
const RATE: u32 = 44_100;

/// How often the loop looks up from the audio to see what has changed around
/// it: the driver winding down, or a node gaining or losing its last link.
const TICK: Duration = Duration::from_millis(100);

/// How long to wait before trying the daemon again.
const RETRY: Duration = Duration::from_secs(2);

/// Publish the two nodes and keep them up until `alive` goes false.
///
/// Not reaching a daemon is not fatal - the bridge's MIDI half is useful on its
/// own, and a headless run has nothing to publish to - but it is not final
/// either. PipeWire gets restarted, and a driver started from udev can easily
/// beat the session's own services to it, so this keeps trying: the nodes are
/// meant to be there for as long as the device is, not for as long as whatever
/// happened to be running when it arrived.
pub fn spawn(alive: fn() -> bool) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut complained = false;
        while alive() {
            match run(alive) {
                Ok(()) => complained = false,
                Err(e) => {
                    // Once. A daemon that is not there is not there repeatedly.
                    if !complained {
                        eprintln!("pipewire: {e} - no audio device published, retrying");
                        complained = true;
                    }
                }
            }
            audio::PLAY_ON.store(false, Ordering::Relaxed);
            audio::REC_ON.store(false, Ordering::Relaxed);
            if alive() {
                thread::sleep(RETRY);
            }
        }
    })
}

fn run(alive: fn() -> bool) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let gain = env("NS6_GAIN", 1.0f32);
    // How much audio to have in hand before the device is told to play any of
    // it. Feeding it the instant the first buffer arrives means running with an
    // empty queue, where every scheduling jitter is a gap.
    let prefill = RATE as usize / 1000 * env("NS6_PLAY_MS", 40usize) * audio::OUT_FRAME;
    // The same for the other direction, where the device is the producer: how
    // much captured audio to sit on, so a late graph cycle has something to
    // take rather than a hole.
    let refill = RATE as usize / 1000 * env("NS6_REC_MS", 20usize) * audio::IN_FRAME;

    // The nodes' own ids, learned as they are bound. Only the daemon can say
    // what they are, and the graph below is described entirely in them.
    let sink_node = Rc::new(Cell::new(u32::MAX));
    let source_node = Rc::new(Cell::new(u32::MAX));

    let sink = pw::stream::StreamBox::new(
        &core,
        "ns6",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => "ns6",
            *pw::keys::NODE_NICK => "NS6",
            *pw::keys::NODE_DESCRIPTION => "Numark NS6 (master 1-2, phones 3-4)",
            *pw::keys::AUDIO_CHANNELS => "4",
        },
    )?;
    let _sink_listener = sink
        .add_local_listener_with_user_data(Playback::new())
        .state_changed({
            let id = sink_node.clone();
            move |stream, pb, _, new| {
                id.set(stream.node_id());
                // A sink that stopped and started again is a new stream: what
                // was left of the last one is stale audio nobody asked to hear,
                // and the next one fills up from empty before it plays.
                if new != StreamState::Streaming {
                    audio::PLAY_ON.store(false, Ordering::Relaxed);
                    audio::clear_play();
                    pb.carry.clear();
                    // The next client may deliver in quite different sized
                    // pieces, and the queue is sized from what it sees - so the
                    // depth averaged from the last one is not a measurement of
                    // anything any more either.
                    pb.burst = 0;
                    pb.fill = None;
                }
            }
        })
        .io_changed(|_, pb, id, area, size| {
            // The adapter hands over the rate-match area once it has decided it
            // is resampling for us, and takes it away again when it stops.
            if id == spa::sys::SPA_IO_RateMatch {
                pb.rate_match =
                    if size as usize >= std::mem::size_of::<spa::sys::spa_io_rate_match>() {
                        area.cast()
                    } else {
                        std::ptr::null_mut()
                    };
            }
        })
        .process(move |stream, pb| play(stream, pb, gain, prefill))
        .register()?;
    sink.connect(
        Direction::Input,
        None,
        StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut [format_param(Channels::Quad)],
    )?;

    let source = pw::stream::StreamBox::new(
        &core,
        "ns6-mix",
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_CLASS => "Audio/Source",
            *pw::keys::NODE_NAME => "ns6-mix",
            *pw::keys::NODE_NICK => "NS6 mix",
            *pw::keys::NODE_DESCRIPTION => "Numark NS6 (mixer output)",
            *pw::keys::AUDIO_CHANNELS => "2",
        },
    )?;
    let _source_listener = source
        .add_local_listener_with_user_data(Capture::new())
        .state_changed({
            let id = source_node.clone();
            move |stream, cap, _, new| {
                id.set(stream.node_id());
                if new != StreamState::Streaming {
                    audio::REC_ON.store(false, Ordering::Relaxed);
                    audio::REC_PRIMED.store(false, Ordering::Relaxed);
                    // The next recorder may ask in quite different sized
                    // pieces, and the queue is sized from what it asks for - so
                    // the depth averaged from the last one is not a measurement
                    // of anything any more either.
                    cap.burst = 0;
                    cap.fill = None;
                }
            }
        })
        .io_changed(|_, cap, id, area, size| {
            if id == spa::sys::SPA_IO_RateMatch {
                cap.rate_match =
                    if size as usize >= std::mem::size_of::<spa::sys::spa_io_rate_match>() {
                        cap.offered = true;
                        area.cast()
                    } else {
                        std::ptr::null_mut()
                    };
            }
        })
        .process(move |stream, cap| capture(stream, cap, refill))
        .register()?;
    source.connect(
        Direction::Output,
        None,
        StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut [format_param(Channels::Stereo)],
    )?;

    // Every link in the graph, by the nodes at its two ends.
    //
    // This is the only honest answer to "is anybody there". An unlinked node is
    // not idle as far as PipeWire is concerned: it stays `running`, it is still
    // scheduled, and `process` is still called on it - with nothing to read at
    // the sink, and into a buffer nobody will look at from the source. Neither
    // the stream's state nor its own traffic distinguishes that from real use,
    // so the links get counted instead.
    let links: Rc<RefCell<HashMap<u32, (u32, u32)>>> = Rc::new(RefCell::new(HashMap::new()));
    let registry = core.get_registry()?;
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let links = links.clone();
            move |global| {
                if global.type_ != ObjectType::Link {
                    return;
                }
                let Some(props) = global.props else {
                    return;
                };
                let node = |key| props.get(key).and_then(|v| v.parse::<u32>().ok());
                if let (Some(out), Some(inp)) = (node("link.output.node"), node("link.input.node"))
                {
                    links.borrow_mut().insert(global.id, (out, inp));
                }
            }
        })
        .global_remove({
            let links = links.clone();
            move |id| {
                links.borrow_mut().remove(&id);
            }
        })
        .register();

    println!("pipewire: sink \"Numark NS6\" (master 1-2, phones 3-4), source \"Numark NS6 (mixer output)\"");

    let quit = mainloop.clone();
    let was = Cell::new((false, false));
    let last_pushed = Cell::new(0u64);
    let timer = mainloop.loop_().add_timer(move |_| {
        if !alive() {
            quit.quit();
            return;
        }
        let (mut playing, mut listening) = (false, false);
        for &(out, inp) in links.borrow().values() {
            playing |= inp == sink_node.get();
            listening |= out == source_node.get();
        }
        // Whether anything is linked is the whole of the gating decision, and
        // it is not visible from outside, so it can be asked for.
        if was.get() != (playing, listening) && std::env::var("NS6_PW_DEBUG").is_ok() {
            eprintln!(
                "pipewire: sink node {} {}, source node {} {}",
                sink_node.get(),
                if playing { "in use" } else { "idle" },
                source_node.get(),
                if listening { "in use" } else { "idle" },
            );
        }
        was.set((playing, listening));
        let pushed = audio::PLAY_PUSHED.load(Ordering::Relaxed);
        let fed = pushed != last_pushed.get();
        last_pushed.set(pushed);
        follow(playing, listening, fed);
    });
    let _ = timer.update_timer(Some(TICK), Some(TICK));
    mainloop.run();

    audio::PLAY_ON.store(false, Ordering::Relaxed);
    audio::REC_ON.store(false, Ordering::Relaxed);
    Ok(())
}

/// Run each direction only while the graph has a use for it.
///
/// The input is what makes this worth doing: decoding it is 5.6 MB/s of
/// bitstream for 176 kB/s of audio, most of what this driver asks of a CPU, and
/// none of it is wanted while nobody is recording.
/// `fed` is whether the sink was handed anything since the last tick, and it
/// has to be asked as well as the links: `process` primes the queue and starts
/// the device within a few cycles of a client connecting, which is sooner than
/// the link it connected over reaches this loop. Tearing the queue down on
/// that gap threw away the priming and started it again - measured, 78 ms of
/// silence written out to the device a second or two into every playback.
fn follow(playing: bool, listening: bool, fed: bool) {
    if !playing && !fed && audio::PLAY_ON.load(Ordering::Relaxed) {
        audio::PLAY_ON.store(false, Ordering::Relaxed);
        audio::clear_play();
    }
    if listening != audio::REC_ON.load(Ordering::Relaxed) {
        if listening {
            // The bitstream carries no timestamps and its phase has to be found
            // in the data, so a new listener starts on a freshly aligned stream
            // rather than on whatever the last one left behind.
            iso::reset_rec_align();
            audio::arm_rec();
        }
        audio::REC_ON.store(listening, Ordering::Relaxed);
    }
}

/// What the sink carries between buffers.
struct Playback {
    /// Host frames left over from a buffer that did not divide evenly.
    carry: Vec<u8>,
    /// Wire frames, built here so the steady state allocates nothing.
    wire: Vec<u8>,
    /// Where to ask the resampler for a slightly different rate, once the
    /// adapter has given us the area to ask in. Null until then.
    rate_match: *mut spa::sys::spa_io_rate_match,
    /// Queue depth, smoothed. See [`SMOOTH_SECONDS`]. `None` until the first
    /// cycle has a depth to seed it with.
    fill: Option<f64>,
    /// The largest buffer this client has handed over recently, in bytes of
    /// wire frames. See [`Playback::target`].
    burst: usize,
    /// The correction currently being asked for, moved gently.
    correction: f64,
    /// Whether to ask at all. Off is the old behaviour, kept because this is
    /// the kind of loop that has to be provable against the hardware.
    matching: bool,
}

impl Playback {
    fn new() -> Self {
        Self {
            carry: Vec::with_capacity(16 * 1024),
            wire: Vec::with_capacity(24 * 1024),
            rate_match: std::ptr::null_mut(),
            fill: None,
            burst: 0,
            correction: 1.0,
            matching: std::env::var("NS6_NO_RATE_MATCH").is_err(),
        }
    }

    /// How much audio to hold, given what this client actually delivers.
    ///
    /// `NS6_PLAY_MS` is a floor, not the answer. What matters is the size of
    /// the pieces the graph actually delivers in, because the device drains
    /// continuously while they arrive one per cycle: the queue swings by a
    /// whole delivery every cycle, and by two when one runs late. Measured
    /// here, that swing was 5 ms to 52 ms around a 28 ms average - so it hit
    /// empty, and an empty queue is written out as silence, which steps the
    /// waveform to zero and back. That is the clicking.
    ///
    /// Three deliveries of headroom keeps the trough clear of zero with a late
    /// cycle to spare. It costs latency, which is why it is measured from what
    /// arrives rather than guessed at: a client delivering 21 ms gets 63 ms,
    /// not the 200 ms a badly behaved one would need.
    fn target(&self, floor: usize) -> usize {
        floor.max(self.burst * 3)
    }
}

/// How hard to pull the queue back towards its target depth: the fractional
/// rate correction asked for per second the queue is away from where it should
/// be.
///
/// The queue is an integrator - a rate error accumulates in it - so a plain
/// proportional term is enough, and settles at a standing offset rather than
/// hunting. A drift of 100 ppm parks the queue 2 ms off target here, which is
/// nothing.
///
/// This and [`SMOOTH_SECONDS`] are the whole loop, and it is the two of them
/// together that decide whether it settles or rings: a depth measured through
/// a lag of `t` seconds and corrected with a gain of `k` per second is a
/// second-order loop damped by `1 / (2 * sqrt(k * t))`. At 0.05 against 5 s
/// that is exactly 1 - critically damped, so a queue knocked off target comes
/// back to it without overshooting past.
///
/// **This is the distortion that arrives partway through a long tone.** The
/// gain here used to be twenty times this against the same lag, which is a
/// damping of 0.22: a loop that rings rather than settles. It did. Measured
/// through the graph on a 200 Hz tone, both queues swung across their whole
/// 0-250 ms range on a 45-second cycle and hit each end of it - empty, where
/// the frames nobody had are written out as silence, and the 250 ms cap, where
/// the frames nobody took are dropped. Either is a step in the waveform, and a
/// step in a steady tone is a click. Nothing else in the run was wrong: the
/// bitstream the device sent was bit-exact and continuously aligned across the
/// whole of it, and the frames handed to the device were the tone.
const RATE_GAIN: f64 = 0.05;

/// The most correction that will ever be asked for, either way.
///
/// Two clocks that are 1% apart are not drifting, they are a bug somewhere
/// else, and a runaway controller must not be able to hide it.
const RATE_LIMIT: f64 = 0.01;

/// How long the queue depth is averaged over.
///
/// The raw depth is useless to steer on: it drops by a whole quantum as the
/// device drains it and jumps back up when the next buffer arrives, so it
/// swings by tens of milliseconds every cycle. Steering on that modulates the
/// resampling ratio at graph rate, which is heard as dirt on the audio - most
/// obviously on a low tone, where the modulation is a large part of a period.
/// Drift, in contrast, is a thing that changes over minutes.
///
/// In seconds and not in cycles, because a cycle is whatever quantum the
/// client chose. The same per-cycle number is a 1-second lag for one client
/// and a 20-second one for another, and only one of those is a loop that
/// settles - see [`RATE_GAIN`], which is tuned against this.
const SMOOTH_SECONDS: f64 = 5.0;

/// The most the correction may move in a second.
///
/// A resampling ratio that steps is a click. There is no hurry: nothing being
/// corrected here changes faster than a crystal warms up, and 0.002 still
/// crosses the whole of [`RATE_LIMIT`] in five seconds.
const RATE_SLEW: f64 = 0.002;

/// One step of the loop that holds a queue at the depth it was primed to.
///
/// Both directions run this same loop against the same constants, because it
/// is the same loop: a queue between two clocks, and a resampler that can be
/// asked to run a little slow or a little fast to keep it where it was put.
/// Which side of the queue this driver is on only changes what `queued` counts.
///
/// `cycle` - the frames this cycle carried - is the clock. The constants are
/// in seconds, and reading the time from the cycle count instead would tie the
/// tuning to the client's quantum, which is the client's to choose.
fn hold_queue(
    area: *mut spa::sys::spa_io_rate_match,
    fill: &mut Option<f64>,
    correction: &mut f64,
    queued: usize,
    target: usize,
    frame: usize,
    cycle: usize,
) {
    if area.is_null() || target == 0 {
        return;
    }
    let depth = (queued / frame) as f64;
    // The first cycle has nothing to average with, so the depth it finds is
    // the average. Starting from a guess instead is starting by correcting an
    // error that is not there: the queue is primed to its target before any of
    // this runs, and a loop told it was 87 ms short of that spends the first
    // half-minute pulling against nothing.
    let fill = fill.get_or_insert(depth);
    if cycle > 0 {
        let dt = cycle as f64 / RATE as f64;
        *fill += (depth - *fill) * (dt / SMOOTH_SECONDS).min(1.0);
        // Seconds of audio away from where the queue should be.
        let error = (*fill - (target / frame) as f64) / RATE as f64;
        // Too full means the resampler is handing this side frames faster than
        // the far end is taking them, so it should hand over slightly fewer.
        let want = (1.0 - RATE_GAIN * error).clamp(1.0 - RATE_LIMIT, 1.0 + RATE_LIMIT);
        let slew = RATE_SLEW * dt;
        *correction += (want - *correction).clamp(-slew, slew);
    }
    unsafe {
        (*area).rate = *correction;
        (*area).flags |= spa::sys::SPA_IO_RATE_MATCH_FLAG_ACTIVE;
    }
}

/// Hold the queue the device is draining at the depth it was primed to.
///
/// This is the whole answer to the clock problem. The device's crystal and the
/// graph's are independent, nothing else here resamples, and without this the
/// queue walks steadily to one end: to the 250 ms cap, where frames are dropped
/// once a minute, or to empty, where they are made up as silence.
fn follow_the_device_clock(pb: &mut Playback, target: usize, cycle: usize) {
    if !pb.matching {
        return;
    }
    hold_queue(
        pb.rate_match,
        &mut pb.fill,
        &mut pb.correction,
        audio::play_queued(),
        target,
        audio::OUT_FRAME,
        cycle,
    );
}

/// Take one buffer from the graph and queue it for the device.
fn play(stream: &Stream, pb: &mut Playback, gain: f32, prefill: usize) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let data = &mut datas[0];
    let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
    let Some(slice) = data.data() else {
        return;
    };
    // The chunk describes a window into the mapping, and both ends of it are
    // the producer's word rather than ours.
    let offset = offset.min(slice.len());
    let size = size.min(slice.len() - offset);
    pb.carry.extend_from_slice(&slice[offset..offset + size]);

    pb.wire.clear();
    let used = audio::encode_host_quad(&pb.carry, gain, &mut pb.wire);
    pb.carry.drain(..used);
    pb.burst = pb.burst.max(pb.wire.len());
    let cycle = pb.wire.len() / audio::OUT_FRAME;
    audio::push_play(&pb.wire);

    let target = pb.target(prefill);
    // `PLAY_ON` doubles as the primed flag: until enough is in hand the device
    // is left on silence, and once it is playing there is nothing to decide.
    if !audio::PLAY_ON.load(Ordering::Relaxed) && audio::play_queued() >= target {
        audio::PLAY_ON.store(true, Ordering::Relaxed);
    } else {
        follow_the_device_clock(pb, target, cycle);
    }
}

/// What the source carries between buffers.
struct Capture {
    /// Host frames, built here so the steady state allocates nothing.
    host: Vec<u8>,
    /// Where to ask the resampler for a slightly different rate, once the
    /// adapter has given us the area to ask in. Null until then.
    rate_match: *mut spa::sys::spa_io_rate_match,
    /// Whether it was ever offered at all. A client speaking the device's own
    /// rate may be given no resampler, and then there is nothing to ask.
    offered: bool,
    /// Queue depth, smoothed. See [`SMOOTH_SECONDS`]. `None` until the first
    /// cycle has a depth to seed it with.
    fill: Option<f64>,
    /// The largest buffer this client has asked for, in bytes of decoded
    /// frames. See [`Capture::target`].
    burst: usize,
    /// The correction currently being asked for, moved gently.
    correction: f64,
    matching: bool,
}

impl Capture {
    fn new() -> Self {
        Self {
            host: Vec::with_capacity(16 * 1024),
            rate_match: std::ptr::null_mut(),
            offered: false,
            fill: None,
            burst: 0,
            correction: 1.0,
            matching: std::env::var("NS6_NO_RATE_MATCH").is_err(),
        }
    }

    /// How much captured audio to hold, given what this client actually asks
    /// for. The same argument as [`Playback::target`], from the other side.
    fn target(&self, floor: usize) -> usize {
        floor.max(self.burst * 3)
    }
}

/// Ask the resampler to take frames a little faster or slower, to hold the
/// capture queue at the depth it settled to.
///
/// The mirror of [`follow_the_device_clock`], and needed for the same reason:
/// the device produces on its own crystal and the graph consumes on the
/// graph's, so the queue between them integrates the difference. Measured
/// here, idle, that difference is **+16.7 ppm** - the queue grows by a
/// millisecond a minute. It is far slower than the output fault was, and
/// nothing drops for the first few hours, which is exactly why it wants
/// correcting rather than watching: a set long enough to reach the queue's
/// 250 ms cap is an ordinary length of set.
///
/// The sign is the same as the playback side, which is not what it looks like
/// it should be and was got wrong first: a `rate` below 1 makes the resampler
/// ask *this* side for **more** frames, in both directions. Asking for more
/// when the queue was already short drained it further, which fed back into
/// asking for more still - the correction sat pinned at the -1% clamp with the
/// graph consuming exactly 1% more than the device produced, and the shortfall
/// written out as silence at 442 frames a second. A runaway in a loop this
/// slow looks like a steady fault, so it is worth being able to see the
/// number: the `capture loop:` line reports the correction being asked for.
fn follow_the_device_clock_in(cap: &mut Capture, target: usize, cycle: usize) {
    if !cap.matching {
        return;
    }
    hold_queue(
        cap.rate_match,
        &mut cap.fill,
        &mut cap.correction,
        audio::rec_queued(),
        target,
        audio::IN_FRAME,
        cycle,
    );
    REC_CORRECTION.store(((cap.correction - 1.0) * 1e6) as i64, Ordering::Relaxed);
    REC_TARGET.store(target as i64, Ordering::Relaxed);
}

/// Whether the source was ever offered a rate-match area, for reporting.
pub static REC_MATCHING: AtomicBool = AtomicBool::new(false);
/// The correction the capture loop is asking for, in ppm, for reporting.
pub static REC_CORRECTION: AtomicI64 = AtomicI64::new(0);
/// The depth the capture loop is aiming at, in bytes, for reporting.
pub static REC_TARGET: AtomicI64 = AtomicI64::new(0);

/// Fill one buffer for the graph from what the device has sent.
fn capture(stream: &Stream, cap: &mut Capture, refill: usize) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let requested = buffer.requested() as usize;
    let datas = buffer.datas_mut();
    if datas.is_empty() {
        return;
    }
    let data = &mut datas[0];
    let Some(slice) = data.data() else {
        return;
    };
    let capacity = slice.len() / audio::HOST_FRAME;
    // A request of zero means the graph does not mind; the buffer's own size is
    // then the only bound there is.
    let frames = if requested == 0 {
        capacity
    } else {
        requested.min(capacity)
    };

    cap.burst = cap.burst.max(frames * audio::IN_FRAME);
    let target = cap.target(refill);
    let want = frames * audio::HOST_FRAME;

    // The lead-in, once per listener: hold the graph on silence rather than on
    // fragments until there is a queue to serve it from. See [`REC_PRIMED`].
    let primed = audio::REC_PRIMED.load(Ordering::Relaxed) || {
        let full = audio::rec_queued() >= target;
        audio::REC_PRIMED.store(full, Ordering::Relaxed);
        full
    };

    let have = if primed {
        cap.host.clear();
        audio::to_host(&audio::take_rec(frames * audio::IN_FRAME), &mut cap.host);
        slice[..cap.host.len()].copy_from_slice(&cap.host);
        cap.host.len()
    } else {
        0
    };
    // Short of what was asked for: the device has not sent it yet, and silence
    // is the only honest thing to put in its place - but it is still a gap,
    // and a gap nobody counts is one nobody can be shown. The lead-in is not
    // one of those: nothing is missing there, the queue is being filled.
    slice[have..want].fill(0);
    if primed && have < want && audio::REC_ON.load(Ordering::Relaxed) {
        let short = ((want - have) / audio::HOST_FRAME) as u64;
        audio::REC_UNDERRUNS.fetch_add(short, Ordering::Relaxed);
    }

    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = audio::HOST_FRAME as i32;
    *chunk.size_mut() = want as u32;

    REC_MATCHING.store(cap.offered, Ordering::Relaxed);
    if primed {
        follow_the_device_clock_in(cap, target, frames);
    }
}

/// What each node carries: four channels out, two in.
#[derive(Clone, Copy)]
enum Channels {
    /// The master output on 1-2 and the headphone jack on 3-4.
    Quad,
    /// The mixer's output, which is the only thing the input is.
    Stereo,
}

/// The format a node offers: s32le at the device's rate, and nothing else.
///
/// 24 bits is what the hardware carries in either direction, and s32 is the
/// container everything else uses for it.
///
/// The headphone pair is given the rear positions rather than being left
/// unpositioned, so that an ordinary stereo application has somewhere obvious
/// to land: PipeWire puts it on 1-2, which is the master output, and a browser
/// does not end up playing into somebody's headphones.
fn format_param(channels: Channels) -> &'static Pod {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::S32LE);
    info.set_rate(RATE);
    let mut position = [0; MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    match channels {
        Channels::Quad => {
            info.set_channels(4);
            position[2] = spa::sys::SPA_AUDIO_CHANNEL_RL;
            position[3] = spa::sys::SPA_AUDIO_CHANNEL_RR;
        }
        Channels::Stereo => info.set_channels(2),
    }
    info.set_position(position);

    let bytes: Vec<u8> = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .expect("serializing a fixed audio format cannot fail")
    .0
    .into_inner();

    // `connect` wants a borrow that outlives the call, and the pod is the same
    // constant every time, so it may as well outlive everything.
    Pod::from_bytes(Vec::leak(bytes)).expect("a serialized pod is a pod")
}

fn env<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
