use std::path::PathBuf;
use std::sync::{Arc, mpsc};

use log::{error, info, warn};
use lumalla_shared::{
    Comms, GlobalArgs, MESSAGE_CHANNEL_TOKEN, MessageRunner, RendererMessage, SeatMessage,
};
use mio::Poll;

pub mod drm;
pub mod vulkan;

use crate::drm::{find_drm_devices, DrmDevice, GbmAllocator, OutputManager};
use vulkan::VulkanContext;

pub struct RendererState {
    comms: Comms,
    event_loop: Poll,
    channel: mpsc::Receiver<RendererMessage>,
    shutting_down: bool,
    #[allow(dead_code)]
    vulkan: Option<VulkanContext>,
    #[allow(dead_code)]
    drm_device: Option<DrmDevice>,
    gbm_allocator: Option<GbmAllocator>,
    output_manager: Option<OutputManager>,
    pending_drm_path: Option<PathBuf>,
}

impl RendererState {
    fn handle_message(&mut self, message: RendererMessage) -> anyhow::Result<()> {
        match message {
            RendererMessage::Shutdown => {
                self.shutting_down = true;
            }
            RendererMessage::SeatSessionCreated { seat_name } => {
                info!("Seat session created: {}", seat_name);
                self.request_drm_device()?;
            }
            RendererMessage::SeatSessionPaused => {
                info!("Seat session paused");
                // TODO: Handle session pause (release DRM master)
            }
            RendererMessage::SeatSessionResumed => {
                info!("Seat session resumed");
                // TODO: Handle session resume (reacquire DRM master)
            }
            RendererMessage::FileOpenedInSession { path, fd } => {
                info!("File opened in session: {}", path.display());
                self.handle_drm_device_opened(path, fd)?;
            }
        }

        Ok(())
    }

    /// Requests the seat to open a DRM device.
    fn request_drm_device(&mut self) -> anyhow::Result<()> {
        let devices = find_drm_devices()?;

        if devices.is_empty() {
            anyhow::bail!("No DRM devices found");
        }

        // Request the first DRM device
        let path = devices[0].clone();
        info!("Requesting DRM device: {}", path.display());

        self.pending_drm_path = Some(path.clone());
        self.comms.seat(SeatMessage::OpenDevice { path });

        Ok(())
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

        // Create DRM device
        let drm_device = DrmDevice::from_fd(fd)?;

        // Log capabilities
        let caps = drm_device.get_capabilities()?;
        info!(
            "DRM device: {} - {}",
            caps.driver_name, caps.driver_description
        );

        // Create output manager and configure outputs
        let mut output_manager = OutputManager::new(&drm_device)?;
        output_manager.configure_outputs(&drm_device)?;

        // Create GBM allocator (this takes ownership of the DRM device)
        let gbm_allocator = GbmAllocator::new(drm_device)?;

        // Now we can create buffers and start rendering
        info!("DRM/GBM backend initialized successfully");

        // Store for later use
        // Note: drm_device was moved into gbm_allocator
        self.gbm_allocator = Some(gbm_allocator);
        self.output_manager = Some(output_manager);

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
        // Initialize Vulkan context
        let vulkan = VulkanContext::new()?;

        Ok(Self {
            comms,
            event_loop,
            channel,
            shutting_down: false,
            vulkan: Some(vulkan),
            drm_device: None,
            gbm_allocator: None,
            output_manager: None,
            pending_drm_path: None,
        })
    }

    fn run(&mut self) -> anyhow::Result<()> {
        let mut events = mio::Events::with_capacity(128);
        loop {
            if let Err(err) = self.event_loop.poll(&mut events, None) {
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

            if self.shutting_down {
                break;
            }
        }

        Ok(())
    }
}
