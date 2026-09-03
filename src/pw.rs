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
use std::sync::atomic::Ordering;
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
                }
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
        .add_local_listener_with_user_data(Vec::<u8>::with_capacity(16 * 1024))
        .state_changed({
            let id = source_node.clone();
            move |stream, _, _, new| {
                id.set(stream.node_id());
                if new != StreamState::Streaming {
                    audio::REC_ON.store(false, Ordering::Relaxed);
                }
            }
        })
        .process(capture)
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
        follow(playing, listening);
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
fn follow(playing: bool, listening: bool) {
    if !playing && audio::PLAY_ON.load(Ordering::Relaxed) {
        audio::PLAY_ON.store(false, Ordering::Relaxed);
        audio::clear_play();
    }
    if listening != audio::REC_ON.load(Ordering::Relaxed) {
        if listening {
            // The bitstream carries no timestamps and its phase has to be found
            // in the data, so a new listener starts on a freshly aligned stream
            // rather than on whatever the last one left behind.
            iso::reset_rec_align();
            let _ = audio::drain_rec();
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
}

impl Playback {
    fn new() -> Self {
        Self {
            carry: Vec::with_capacity(16 * 1024),
            wire: Vec::with_capacity(24 * 1024),
        }
    }
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
    audio::push_play(&pb.wire);

    // `PLAY_ON` doubles as the primed flag: until enough is in hand the device
    // is left on silence, and once it is playing there is nothing to decide.
    if !audio::PLAY_ON.load(Ordering::Relaxed) && audio::play_queued() >= prefill {
        audio::PLAY_ON.store(true, Ordering::Relaxed);
    }
}

/// Fill one buffer for the graph from what the device has sent.
fn capture(stream: &Stream, host: &mut Vec<u8>) {
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

    host.clear();
    audio::to_host(&audio::take_rec(frames * audio::IN_FRAME), host);
    let have = host.len();
    slice[..have].copy_from_slice(host);
    // Short of what was asked for: the device has not sent it yet, and silence
    // is the only honest thing to put in its place.
    slice[have..frames * audio::HOST_FRAME].fill(0);

    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = audio::HOST_FRAME as i32;
    *chunk.size_mut() = (frames * audio::HOST_FRAME) as u32;
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
