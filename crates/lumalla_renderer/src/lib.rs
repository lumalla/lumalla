use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use log::{error, info, warn};
use lumalla_shared::{
    Comms, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, RendererMessage, SeatMessage,
};
use mio::Poll;

pub mod drm;
pub mod vulkan;

use crate::drm::{DrmDevice, DumbBuffer, OutputManager, find_drm_devices};
use vulkan::VulkanContext;

pub struct RendererState {
    comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<RendererMessage>,
    shutting_down: bool,
    #[allow(dead_code)]
    vulkan: Option<VulkanContext>,
    /// The active display state (if initialized)
    display: Option<DisplayState>,
    pending_drm_path: Option<PathBuf>,
    /// When the renderer started (for safety timeout)
    start_time: Instant,
}

/// Holds the active display state.
struct DisplayState {
    /// The DRM device
    drm_device: DrmDevice,
    /// Output manager for display configuration
    output_manager: OutputManager,
    /// Dumb buffers for test rendering (double buffered)
    buffers: Vec<DumbBuffer>,
    /// Current front buffer index
    front_buffer: usize,
    /// When the display was initialized (for test timeout)
    start_time: Instant,
}

/// How long to show the test pattern before auto-exiting (set to None to disable)
const TEST_DISPLAY_TIMEOUT: Option<Duration> = Some(Duration::from_secs(5));

impl RendererState {
    fn handle_message(&mut self, message: RendererMessage) -> anyhow::Result<()> {
        match message {
            RendererMessage::Shutdown => {
                self.shutting_down = true;
            }
            RendererMessage::SeatSessionCreated {
                seat_name: _seat_name,
            } => {
                self.request_drm_device()?;
            }
            RendererMessage::SeatSessionPaused => {
                // TODO: Handle session pause (release DRM master)
            }
            RendererMessage::SeatSessionResumed => {
                // TODO: Handle session resume (reacquire DRM master)
            }
            RendererMessage::FileOpenedInSession { path, fd } => {
                self.handle_drm_device_opened(path, fd)?;
            }
        }

        Ok(())
    }

    /// Requests the seat to open a DRM device.
    ///
    /// If Vulkan was initialized successfully, uses the DRM device that corresponds
    /// to the selected Vulkan physical device. Otherwise, falls back to finding
    /// available DRM devices and preferring card0.
    fn request_drm_device(&mut self) -> anyhow::Result<()> {
        // Try to use the DRM device from Vulkan's selected physical device
        let path = if let Some(vulkan) = &self.vulkan {
            if let Some(vulkan_drm_path) = vulkan.drm_device_path() {
                vulkan_drm_path.clone()
            } else {
                // Vulkan didn't provide a DRM path, fall back to discovery
                self.find_fallback_drm_device()?
            }
        } else {
            // No Vulkan context, fall back to discovery
            self.find_fallback_drm_device()?
        };

        self.pending_drm_path = Some(path.clone());
        self.comms.seat(SeatMessage::OpenDevice { path });

        Ok(())
    }

    /// Finds a fallback DRM device when Vulkan doesn't provide one.
    fn find_fallback_drm_device(&self) -> anyhow::Result<PathBuf> {
        let devices = find_drm_devices()?;

        if devices.is_empty() {
            anyhow::bail!("No DRM devices found");
        }

        // Prefer card0 as it's usually the primary display GPU
        let path = devices
            .iter()
            .find(|p| p.to_string_lossy().ends_with("card0"))
            .unwrap_or(&devices[0])
            .clone();

        Ok(path)
    }

    /// Handles a DRM device being opened by the seat.
    fn handle_drm_device_opened(
        &mut self,
        path: PathBuf,
        fd: std::os::fd::OwnedFd,
    ) -> anyhow::Result<()> {
        // Verify this is the device we requested
        if self.pending_drm_path.as_ref() != Some(&path) {
            warn!("Received unexpected device: {}", path.display());
            return Ok(());
        }
        self.pending_drm_path = None;

        let drm_device = DrmDevice::from_fd(fd)?;

        let _caps = drm_device.get_capabilities()?;

        let mut output_manager = OutputManager::new(&drm_device)?;
        output_manager.configure_outputs(&drm_device)?;

        let (width, height) = if let Some(output) = output_manager.outputs.first() {
            output.mode.size()
        } else {
            anyhow::bail!("No outputs configured");
        };

        let mut buffer1 = DumbBuffer::new(&drm_device, width as u32, height as u32)?;
        let mut buffer2 = DumbBuffer::new(&drm_device, width as u32, height as u32)?;

        buffer1.draw_color_bars(&drm_device)?;
        buffer2.draw_gradient(&drm_device)?;

        let fbs = vec![buffer1.framebuffer()];
        match output_manager.atomic_enable_with_fb(&drm_device, &fbs) {
            Ok(()) => {}
            Err(e) => {
                return Err(e);
            }
        }

        self.display = Some(DisplayState {
            drm_device,
            output_manager,
            buffers: vec![buffer1, buffer2],
            front_buffer: 0,
            start_time: Instant::now(),
        });

        Ok(())
    }

    /// Swaps to the next buffer (for animation testing).
    #[allow(dead_code)]
    fn swap_buffers(&mut self) -> anyhow::Result<()> {
        if let Some(display) = &mut self.display {
            // Toggle buffer
            display.front_buffer = (display.front_buffer + 1) % display.buffers.len();

            let fb = display.buffers[display.front_buffer].framebuffer();

            // Page flip to the new buffer
            display
                .output_manager
                .atomic_page_flip(&display.drm_device, 0, fb)?;

            info!("Swapped to buffer {}", display.front_buffer);
        }

        Ok(())
    }
}

impl MessageRunner for RendererState {
    type Message = RendererMessage;

    fn new(
        comms: Comms,
        event_loop: Poll,
        channel: mpsc::Receiver<Self::Message>,
        _args: Arc<GlobalArgs>,
    ) -> anyhow::Result<Self> {
        // Initialize Vulkan context (optional - continue without if it fails)
        let vulkan = VulkanContext::new().ok();

        Ok(Self {
            comms,
            event_loop,
            channel,
            shutting_down: false,
            vulkan,
            display: None,
            pending_drm_path: None,
            start_time: Instant::now(),
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = mio::Events::with_capacity(128);

        loop {
            // Short timeout for polling so we can check the test timeout
            let poll_timeout = Some(Duration::from_millis(100));

            if let Err(err) = self.event_loop.poll(&mut events, poll_timeout) {
                error!("Unable to poll event loop: {err}");
            }

            for event in events.iter() {
                match event.token() {
                    MESSAGE_CHANNEL_TOKEN => {
                        while let Ok(msg) = self.channel.try_recv() {
                            if let Err(err) = self.handle_message(msg) {
                                error!("Unable to handle message: {err}");
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Check test display timeout
            if let (Some(display), Some(timeout)) = (&self.display, TEST_DISPLAY_TIMEOUT) {
                if display.start_time.elapsed() >= timeout {
                    info!("Test display timeout reached, shutting down");
                    self.shutting_down = true;
                }
            }

            // Safety timeout: if nothing happened for 15 seconds, shut down
            const SAFETY_TIMEOUT: Duration = Duration::from_secs(15);
            if self.start_time.elapsed() >= SAFETY_TIMEOUT {
                warn!("Safety timeout reached, shutting down renderer");
                self.shutting_down = true;
            }

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}
