mod buttons;
mod connection;
mod dashboard;
mod logging_backend;
mod sockets;
mod statistics;
mod tracking;
mod web_server;

#[allow(
    non_camel_case_types,
    non_upper_case_globals,
    dead_code,
    non_snake_case,
    clippy::unseparated_literal_suffix
)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
use bindings::*;

use alvr_common::{
    log,
    once_cell::sync::{Lazy, OnceCell},
    parking_lot::{Mutex, RwLock},
    prelude::*,
    RelaxedAtomic, ALVR_VERSION,
};
use alvr_events::EventType;
use alvr_filesystem::{self as afs, Layout};
use alvr_server_data::ServerDataManager;
use alvr_session::{OpenvrPropValue, OpenvrPropertyKey};
use alvr_sockets::{
    ClientListAction, GpuVendor, Haptics, ServerControlPacket, VideoFrameHeaderPacket,
};
use statistics::StatisticsManager;
use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr, CString},
    ptr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Once,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Runtime,
    sync::{broadcast, mpsc, Notify},
};

static FILESYSTEM_LAYOUT: Lazy<Layout> = Lazy::new(|| {
    afs::filesystem_layout_from_openvr_driver_root_dir(&alvr_commands::get_driver_dir().unwrap())
});
static SERVER_DATA_MANAGER: Lazy<RwLock<ServerDataManager>> =
    Lazy::new(|| RwLock::new(ServerDataManager::new(&FILESYSTEM_LAYOUT.session())));
static WEBSERVER_RUNTIME: Lazy<Mutex<Option<Runtime>>> =
    Lazy::new(|| Mutex::new(Runtime::new().ok()));
static WINDOW: Lazy<Mutex<Option<Arc<WindowType>>>> = Lazy::new(|| Mutex::new(None));

static STATISTICS_MANAGER: Lazy<Mutex<Option<StatisticsManager>>> = Lazy::new(|| Mutex::new(None));

pub struct VideoPacket {
    pub header: VideoFrameHeaderPacket,
    pub payload: Vec<u8>,
}

static CONTROL_CHANNEL_SENDER: Lazy<Mutex<Option<mpsc::UnboundedSender<ServerControlPacket>>>> =
    Lazy::new(|| Mutex::new(None));
static VIDEO_SENDER: Lazy<Mutex<Option<mpsc::UnboundedSender<VideoPacket>>>> =
    Lazy::new(|| Mutex::new(None));
static HAPTICS_SENDER: Lazy<Mutex<Option<mpsc::UnboundedSender<Haptics>>>> =
    Lazy::new(|| Mutex::new(None));
static VIDEO_MIRROR_SENDER: Lazy<Mutex<Option<broadcast::Sender<Vec<u8>>>>> =
    Lazy::new(|| Mutex::new(None));

static DISCONNECT_CLIENT_NOTIFIER: Lazy<Notify> = Lazy::new(Notify::new);
static RESTART_NOTIFIER: Lazy<Notify> = Lazy::new(Notify::new);

static FRAME_RENDER_VS_CSO: &[u8] = include_bytes!("../cpp/platform/win32/FrameRenderVS.cso");
static FRAME_RENDER_PS_CSO: &[u8] = include_bytes!("../cpp/platform/win32/FrameRenderPS.cso");
static QUAD_SHADER_CSO: &[u8] = include_bytes!("../cpp/platform/win32/QuadVertexShader.cso");
static COMPRESS_AXIS_ALIGNED_CSO: &[u8] =
    include_bytes!("../cpp/platform/win32/CompressAxisAlignedPixelShader.cso");
static COLOR_CORRECTION_CSO: &[u8] =
    include_bytes!("../cpp/platform/win32/ColorCorrectionPixelShader.cso");

static QUAD_SHADER_VERT_SPV: &[u8] = include_bytes!("../cpp/platform/linux/shader/quad.vert.spv");
static QUAD_SHADER_FRAG_SPV: &[u8] = include_bytes!("../cpp/platform/linux/shader/quad.frag.spv");
static COLOR_SHADER_FRAG_SPV: &[u8] = include_bytes!("../cpp/platform/linux/shader/color.frag.spv");
static FFR_SHADER_FRAG_SPV: &[u8] = include_bytes!("../cpp/platform/linux/shader/ffr.frag.spv");

static IS_ALIVE: Lazy<Arc<RelaxedAtomic>> = Lazy::new(|| Arc::new(RelaxedAtomic::new(false)));

pub enum WindowType {
    Alcro(alcro::UI),
    Browser,
}

pub fn to_cpp_openvr_prop(key: OpenvrPropertyKey, value: OpenvrPropValue) -> OpenvrProperty {
    let type_ = match value {
        OpenvrPropValue::Bool(_) => OpenvrPropertyType_Bool,
        OpenvrPropValue::Float(_) => OpenvrPropertyType_Float,
        OpenvrPropValue::Int32(_) => OpenvrPropertyType_Int32,
        OpenvrPropValue::Uint64(_) => OpenvrPropertyType_Uint64,
        OpenvrPropValue::Vector3(_) => OpenvrPropertyType_Vector3,
        OpenvrPropValue::Double(_) => OpenvrPropertyType_Double,
        OpenvrPropValue::String(_) => OpenvrPropertyType_String,
    };

    let value = match value {
        OpenvrPropValue::Bool(bool_) => OpenvrPropertyValue { bool_ },
        OpenvrPropValue::Float(float_) => OpenvrPropertyValue { float_ },
        OpenvrPropValue::Int32(int32) => OpenvrPropertyValue { int32 },
        OpenvrPropValue::Uint64(uint64) => OpenvrPropertyValue { uint64 },
        OpenvrPropValue::Vector3(vector3) => OpenvrPropertyValue { vector3 },
        OpenvrPropValue::Double(double_) => OpenvrPropertyValue { double_ },
        OpenvrPropValue::String(value) => {
            let c_string = CString::new(value).unwrap();
            let mut string = [0; 64];

            unsafe {
                ptr::copy_nonoverlapping(
                    c_string.as_ptr(),
                    string.as_mut_ptr(),
                    c_string.as_bytes_with_nul().len(),
                );
            }

            OpenvrPropertyValue { string }
        }
    };

    OpenvrProperty {
        key: key as u32,
        type_,
        value,
    }
}

pub fn shutdown_runtimes() {
    alvr_events::send_event(EventType::ServerQuitting);

    // Shutsdown all connection runtimes
    IS_ALIVE.set(false);

    if let Some(window_type) = WINDOW.lock().take() {
        match window_type.as_ref() {
            WindowType::Alcro(window) => window.close(),
            WindowType::Browser => (),
        }
    }

    WEBSERVER_RUNTIME.lock().take();
}

pub fn notify_shutdown_driver() {
    thread::spawn(|| {
        RESTART_NOTIFIER.notify_waiters();

        // give time to the control loop to send the restart packet (not crucial)
        thread::sleep(Duration::from_millis(100));

        shutdown_runtimes();

        unsafe { ShutdownSteamvr() };
    });
}

pub fn notify_restart_driver() {
    notify_shutdown_driver();

    alvr_commands::restart_steamvr(&FILESYSTEM_LAYOUT.launcher_exe()).ok();
}

pub fn notify_application_update() {
    notify_shutdown_driver();

    alvr_commands::invoke_application_update(&FILESYSTEM_LAYOUT.launcher_exe()).ok();
}

fn init() {
    let (log_sender, _) = broadcast::channel(web_server::WS_BROADCAST_CAPACITY);
    let (legacy_events_sender, _) = broadcast::channel(web_server::WS_BROADCAST_CAPACITY);
    let (events_sender, _) = broadcast::channel(web_server::WS_BROADCAST_CAPACITY);
    logging_backend::init_logging(
        log_sender.clone(),
        legacy_events_sender.clone(),
        events_sender.clone(),
    );

    if let Some(runtime) = WEBSERVER_RUNTIME.lock().as_mut() {
        // Acquire and drop the data manager lock to create session.json if not present
        // this is needed until Settings.cpp is replaced with Rust. todo: remove
        SERVER_DATA_MANAGER.write().session_mut();

        let connections = SERVER_DATA_MANAGER
            .read()
            .session()
            .client_connections
            .clone();
        for (hostname, connection) in connections {
            if !connection.trusted {
                SERVER_DATA_MANAGER
                    .write()
                    .update_client_list(hostname, ClientListAction::RemoveEntry);
            }
        }

        runtime.spawn(alvr_common::show_err_async(web_server::web_server(
            log_sender,
            legacy_events_sender,
            events_sender,
        )));

        thread::spawn(|| alvr_common::show_err(dashboard::ui_thread()));
    }

    {
        let mut data_manager = SERVER_DATA_MANAGER.write();
        if matches!(data_manager.get_gpu_vendor(), GpuVendor::Nvidia) {
            data_manager
                .session_mut()
                .session_settings
                .extra
                .patches
                .linux_async_reprojection = false;
        }

        if data_manager.session().server_version != *ALVR_VERSION {
            let mut session_ref = data_manager.session_mut();
            session_ref.server_version = ALVR_VERSION.clone();
            session_ref.client_connections.clear();
        }

        for conn in data_manager.session_mut().client_connections.values_mut() {
            conn.current_ip = None;
        }
    }

    unsafe {
        g_sessionPath = CString::new(FILESYSTEM_LAYOUT.session().to_string_lossy().to_string())
            .unwrap()
            .into_raw();
        g_driverRootDir = CString::new(
            FILESYSTEM_LAYOUT
                .openvr_driver_root_dir
                .to_string_lossy()
                .to_string(),
        )
        .unwrap()
        .into_raw();
    };
}

/// # Safety
#[no_mangle]
pub unsafe extern "C" fn HmdDriverFactory(
    interface_name: *const c_char,
    return_code: *mut i32,
) -> *mut c_void {
    static INIT_ONCE: Once = Once::new();
    INIT_ONCE.call_once(init);

    FRAME_RENDER_VS_CSO_PTR = FRAME_RENDER_VS_CSO.as_ptr();
    FRAME_RENDER_VS_CSO_LEN = FRAME_RENDER_VS_CSO.len() as _;
    FRAME_RENDER_PS_CSO_PTR = FRAME_RENDER_PS_CSO.as_ptr();
    FRAME_RENDER_PS_CSO_LEN = FRAME_RENDER_PS_CSO.len() as _;
    QUAD_SHADER_CSO_PTR = QUAD_SHADER_CSO.as_ptr();
    QUAD_SHADER_CSO_LEN = QUAD_SHADER_CSO.len() as _;
    COMPRESS_AXIS_ALIGNED_CSO_PTR = COMPRESS_AXIS_ALIGNED_CSO.as_ptr();
    COMPRESS_AXIS_ALIGNED_CSO_LEN = COMPRESS_AXIS_ALIGNED_CSO.len() as _;
    COLOR_CORRECTION_CSO_PTR = COLOR_CORRECTION_CSO.as_ptr();
    COLOR_CORRECTION_CSO_LEN = COLOR_CORRECTION_CSO.len() as _;
    QUAD_SHADER_VERT_SPV_PTR = QUAD_SHADER_VERT_SPV.as_ptr();
    QUAD_SHADER_VERT_SPV_LEN = QUAD_SHADER_VERT_SPV.len() as _;
    QUAD_SHADER_FRAG_SPV_PTR = QUAD_SHADER_FRAG_SPV.as_ptr();
    QUAD_SHADER_FRAG_SPV_LEN = QUAD_SHADER_FRAG_SPV.len() as _;
    COLOR_SHADER_FRAG_SPV_PTR = COLOR_SHADER_FRAG_SPV.as_ptr();
    COLOR_SHADER_FRAG_SPV_LEN = COLOR_SHADER_FRAG_SPV.len() as _;
    FFR_SHADER_FRAG_SPV_PTR = FFR_SHADER_FRAG_SPV.as_ptr();
    FFR_SHADER_FRAG_SPV_LEN = FFR_SHADER_FRAG_SPV.len() as _;

    unsafe extern "C" fn log_error(string_ptr: *const c_char) {
        alvr_common::show_e(CStr::from_ptr(string_ptr).to_string_lossy());
    }

    unsafe fn log(level: log::Level, string_ptr: *const c_char) {
        log::log!(level, "{}", CStr::from_ptr(string_ptr).to_string_lossy());
    }

    unsafe extern "C" fn log_warn(string_ptr: *const c_char) {
        log(log::Level::Warn, string_ptr);
    }

    unsafe extern "C" fn log_info(string_ptr: *const c_char) {
        log(log::Level::Info, string_ptr);
    }

    unsafe extern "C" fn log_debug(string_ptr: *const c_char) {
        log(log::Level::Debug, string_ptr);
    }

    // Should not be used in production
    unsafe extern "C" fn log_periodically(tag_ptr: *const c_char, message_ptr: *const c_char) {
        const INTERVAL: Duration = Duration::from_secs(1);
        static LASTEST_TAG_TIMESTAMPS: Lazy<Mutex<HashMap<String, Instant>>> =
            Lazy::new(|| Mutex::new(HashMap::new()));

        let tag = CStr::from_ptr(tag_ptr).to_string_lossy();
        let message = CStr::from_ptr(message_ptr).to_string_lossy();

        let mut timestamps_ref = LASTEST_TAG_TIMESTAMPS.lock();
        let old_timestamp = timestamps_ref
            .entry(tag.to_string())
            .or_insert_with(Instant::now);
        if *old_timestamp + INTERVAL < Instant::now() {
            *old_timestamp += INTERVAL;

            log::warn!("{}: {}", tag, message);
        }
    }

    extern "C" fn initialize_decoder(buffer_ptr: *const u8, len: i32) {
        if let Some(sender) = &*CONTROL_CHANNEL_SENDER.lock() {
            let mut config_buffer = vec![0; len as usize];

            unsafe {
                ptr::copy_nonoverlapping(buffer_ptr, config_buffer.as_mut_ptr(), len as usize)
            };

            if let Some(sender) = &*VIDEO_MIRROR_SENDER.lock() {
                sender.send(config_buffer.clone()).ok();
            }

            sender
                .send(ServerControlPacket::InitializeDecoder { config_buffer })
                .ok();
        }
    }

    extern "C" fn video_send(header: VideoFrame, buffer_ptr: *mut u8, len: i32) {
        if let Some(sender) = &*VIDEO_SENDER.lock() {
            let header = VideoFrameHeaderPacket {
                packet_counter: header.packetCounter,
                tracking_frame_index: header.trackingFrameIndex,
                video_frame_index: header.videoFrameIndex,
                sent_time: header.sentTime,
                frame_byte_size: header.frameByteSize,
                fec_index: header.fecIndex,
                fec_percentage: header.fecPercentage,
            };

            let mut vec_buffer = vec![0; len as _];

            // use copy_nonoverlapping (aka memcpy) to avoid freeing memory allocated by C++
            unsafe {
                ptr::copy_nonoverlapping(buffer_ptr, vec_buffer.as_mut_ptr(), len as _);
            }

            if let Some(sender) = &*VIDEO_MIRROR_SENDER.lock() {
                sender.send(vec_buffer.clone()).ok();
            }

            sender
                .send(VideoPacket {
                    header,
                    payload: vec_buffer,
                })
                .ok();

            if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
                stats.report_video_packet(len as _);
            }
        }
    }

    extern "C" fn haptics_send(path: u64, duration_s: f32, frequency: f32, amplitude: f32) {
        if let Some(sender) = &*HAPTICS_SENDER.lock() {
            let haptics = Haptics {
                path,
                duration: Duration::from_secs_f32(duration_s),
                frequency,
                amplitude,
            };

            sender.send(haptics).ok();
        }
    }

    pub extern "C" fn driver_ready_idle(set_default_chap: bool) {
        alvr_common::show_err(alvr_commands::apply_driver_paths_backup(
            FILESYSTEM_LAYOUT.openvr_driver_root_dir.clone(),
        ));

        IS_ALIVE.set(true);

        thread::spawn(move || {
            if set_default_chap {
                // call this when inside a new tokio thread. Calling this on the parent thread will
                // crash SteamVR
                unsafe { SetChaperone(2.0, 2.0) };
            }

            if let Err(InterruptibleError::Other(e)) = connection::handshake_loop() {
                warn!("Connection thread closed: {e}");
            }
        });
    }

    extern "C" fn _shutdown_runtime() {
        shutdown_runtimes();
    }

    unsafe extern "C" fn path_string_to_hash(path: *const c_char) -> u64 {
        alvr_common::hash_string(CStr::from_ptr(path).to_str().unwrap())
    }

    extern "C" fn report_present(timestamp_ns: u64) {
        if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
            stats.report_frame_present(Duration::from_nanos(timestamp_ns));
        }
    }

    extern "C" fn report_composed(timestamp_ns: u64) {
        if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
            stats.report_frame_composed(Duration::from_nanos(timestamp_ns));
        }
    }

    extern "C" fn report_encoded(timestamp_ns: u64) {
        if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
            stats.report_frame_encoded(Duration::from_nanos(timestamp_ns));
        }
    }

    extern "C" fn report_fec_failure(percentage: i32) {
        if let Some(stats) = &mut *STATISTICS_MANAGER.lock() {
            stats.report_fec_failure(percentage as u32);
        }
    }

    LogError = Some(log_error);
    LogWarn = Some(log_warn);
    LogInfo = Some(log_info);
    LogDebug = Some(log_debug);
    LogPeriodically = Some(log_periodically);
    DriverReadyIdle = Some(driver_ready_idle);
    InitializeDecoder = Some(initialize_decoder);
    VideoSend = Some(video_send);
    HapticsSend = Some(haptics_send);
    ShutdownRuntime = Some(_shutdown_runtime);
    PathStringToHash = Some(path_string_to_hash);
    ReportPresent = Some(report_present);
    ReportComposed = Some(report_composed);
    ReportEncoded = Some(report_encoded);
    ReportFecFailure = Some(report_fec_failure);

    // cast to usize to allow the variables to cross thread boundaries
    let interface_name_usize = interface_name as usize;
    let return_code_usize = return_code as usize;

    static PTR_USIZE: OnceCell<AtomicUsize> = OnceCell::new();
    static NUM_TRIALS: OnceCell<AtomicUsize> = OnceCell::new();

    PTR_USIZE.set(AtomicUsize::new(0)).ok();
    NUM_TRIALS.set(AtomicUsize::new(0)).ok();

    thread::spawn(move || {
        NUM_TRIALS.get().unwrap().fetch_add(1, Ordering::Relaxed);
        if NUM_TRIALS.get().unwrap().load(Ordering::Relaxed) <= 1 {
            PTR_USIZE.get().unwrap().store(
                CppEntryPoint(interface_name_usize as _, return_code_usize as _) as _,
                Ordering::Relaxed,
            );
        }
    })
    .join()
    .ok();

    PTR_USIZE.get().unwrap().load(Ordering::Relaxed) as _
}
