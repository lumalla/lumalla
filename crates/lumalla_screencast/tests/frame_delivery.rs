//! Isolated PipeWire producer → consumer frame delivery check.
//!
//! Runs against the session PipeWire daemon (no full compositor). Skip if the
//! bus is unavailable.

use std::{
    os::fd::OwnedFd,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use lumalla_screencast::{
    DmaBufferExport, ScreencastManager, ScreencastSource, ScreencastWake, VideoFrame,
};
use pipewire::{
    self as pw,
    context::ContextRc,
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::{self, Pod, serialize::PodSerializer},
        utils::{Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamFlags, StreamRc, StreamState},
};

fn dummy_dma_exports(count: usize, width: u32, height: u32) -> Vec<DmaBufferExport> {
    let stride = width * 4;
    let size = stride * height;
    (0..count)
        .map(|index| {
            let fd = nix_memfd(size);
            DmaBufferExport {
                index,
                fd,
                width,
                height,
                stride,
                offset: 0,
                modifier: 0,
            }
        })
        .collect()
}

fn nix_memfd(size: u32) -> OwnedFd {
    use std::os::fd::{FromRawFd, RawFd};
    let raw = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            c"lumalla-test-dma".as_ptr(),
            libc::MFD_CLOEXEC,
        )
    };
    assert!(raw >= 0, "memfd_create failed");
    let fd = raw as RawFd;
    assert_eq!(unsafe { libc::ftruncate(fd, size as libc::off_t) }, 0);
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn rgba_frame(width: u32, height: u32, color: [u8; 4]) -> VideoFrame {
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for px in rgba.chunks_exact_mut(4) {
        px.copy_from_slice(&color);
    }
    VideoFrame {
        width,
        height,
        rgba,
    }
}

fn serialize_rgba_format(width: u32, height: u32) -> Vec<u8> {
    let obj = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::RGBA,
            VideoFormat::RGBA,
            VideoFormat::RGBx,
            VideoFormat::BGRx
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width, height },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 4096,
                height: 4096
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 60,
                denom: 1
            }
        ),
    );
    PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pod::Value::Object(obj))
        .unwrap()
        .0
        .into_inner()
}

/// Connect an input stream to `target_node` and count buffers with non-zero size.
fn consume_frames(target_node: u32, width: u32, height: u32, want: u32, timeout: Duration) -> u32 {
    let got = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let got_cb = Arc::clone(&got);
    let stop_for_thread = Arc::clone(&stop);

    let join = thread::spawn(move || {
        let stop_cb_state = Arc::clone(&stop_for_thread);
        let stop_cb_proc = Arc::clone(&stop_for_thread);
        let stop_timer = Arc::clone(&stop_for_thread);
        pw::init();
        let mainloop = MainLoopRc::new(None).expect("mainloop");
        let context = ContextRc::new(&mainloop, None).expect("context");
        let core = context.connect_rc(None).expect("core");

        let stream = StreamRc::new(
            core,
            "lumalla-frame-probe",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Camera",
            },
        )
        .expect("stream");

        let format_bytes = serialize_rgba_format(width, height);
        let mut params = [Pod::from_bytes(&format_bytes).expect("pod")];

        let _listener = stream
            .add_local_listener_with_user_data(VideoInfoRaw::default())
            .state_changed(move |_, _, _, new| {
                if matches!(new, StreamState::Error(_)) {
                    stop_cb_state.store(true, Ordering::Release);
                }
            })
            .param_changed(|_, user_data, id, param| {
                let Some(param) = param else {
                    return;
                };
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let _ = user_data.parse(param);
            })
            .process({
                let got_cb = Arc::clone(&got_cb);
                move |stream, _| {
                    while let Some(mut buffer) = stream.dequeue_buffer() {
                        let datas = buffer.datas_mut();
                        if let Some(data) = datas.first() {
                            if data.chunk().size() > 0 {
                                let n = got_cb.fetch_add(1, Ordering::AcqRel) + 1;
                                if n >= want {
                                    stop_cb_proc.store(true, Ordering::Release);
                                }
                            }
                        }
                    }
                }
            })
            .register()
            .expect("listener");

        stream
            .connect(
                Direction::Input,
                Some(target_node),
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .expect("connect");

        let ml_quit = mainloop.clone();
        let timer = mainloop.loop_().add_timer(move |_| {
            if stop_timer.load(Ordering::Acquire) {
                ml_quit.quit();
            }
        });
        let _ = timer
            .update_timer(
                Some(Duration::from_millis(10)),
                Some(Duration::from_millis(10)),
            )
            .into_result();

        mainloop.run();
        drop(timer);
    });

    let start = Instant::now();
    while start.elapsed() < timeout {
        if got.load(Ordering::Acquire) >= want {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    stop.store(true, Ordering::Release);
    let _ = join.join();
    got.load(Ordering::Acquire)
}

#[test]
fn memfd_source_delivers_frames_to_consumer() {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        eprintln!("skip: no XDG_RUNTIME_DIR");
        return;
    }

    let width = 64u32;
    let height = 48u32;
    let wakes = Arc::new(Mutex::new(Vec::<ScreencastWake>::new()));
    let wakes_cb = Arc::clone(&wakes);

    let manager = Arc::new(Mutex::new(ScreencastManager::new(Arc::new(move |wake| {
        wakes_cb.lock().unwrap().push(wake);
    }))));

    let exports = dummy_dma_exports(ScreencastManager::dma_buffer_count(), width, height);
    let stream_id = manager
        .lock()
        .unwrap()
        .start_stream(
            ScreencastSource::Region {
                x: 0,
                y: 0,
                width: width as i32,
                height: height as i32,
            },
            0,
            0,
            width as i32,
            height as i32,
            String::from("LumallaFrameTest"),
            30,
            exports,
        )
        .expect("start_stream");

    let node_id = {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let ready = wakes.lock().unwrap().iter().find_map(|w| match w {
                ScreencastWake::StreamReady {
                    stream_id: id,
                    result,
                } if *id == stream_id => Some(result.clone()),
                _ => None,
            });
            if let Some(result) = ready {
                break result.expect("stream ready");
            }
            assert!(Instant::now() < deadline, "timeout waiting for stream ready");
            thread::sleep(Duration::from_millis(10));
        }
    };
    manager
        .lock()
        .unwrap()
        .complete_start(stream_id, Ok(node_id))
        .expect("complete_start");

    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let manager_prod = Arc::clone(&manager);
    let producer = thread::spawn(move || {
        while !stop_flag.load(Ordering::Acquire) {
            let _ = manager_prod.lock().unwrap().push_memfd_frame(
                stream_id,
                rgba_frame(width, height, [255, 0, 0, 255]),
            );
            let _ = manager_prod.lock().unwrap().take_pending_blits();
            thread::sleep(Duration::from_millis(8));
        }
    });

    let frames = consume_frames(node_id, width, height, 5, Duration::from_secs(5));
    stop.store(true, Ordering::Release);
    let _ = producer.join();
    manager.lock().unwrap().stop_stream(stream_id);

    assert!(
        frames >= 5,
        "expected at least 5 non-empty frames from node {node_id}, got {frames}"
    );
}
