//! KMS output management with atomic modesetting

use anyhow::Context;
use drm::control::{
    connector, crtc, framebuffer, plane, property, AtomicCommitFlags, Device as ControlDevice,
    Mode, ResourceHandle,
};
use log::{debug, info, warn};

use super::DrmDevice;

/// Represents a physical display connector (e.g., HDMI, DisplayPort).
#[derive(Debug, Clone)]
pub struct Connector {
    /// The connector handle
    pub handle: connector::Handle,
    /// Connector type (HDMI, DP, etc.)
    pub connector_type: connector::Interface,
    /// Current connection state
    pub connection: connector::State,
    /// Available display modes
    pub modes: Vec<Mode>,
    /// Physical size in mm (if available)
    pub physical_size: (u32, u32),
    /// The encoder currently connected (if any)
    pub encoder: Option<drm::control::encoder::Handle>,
}

/// Represents a CRTC (display controller).
#[derive(Debug, Clone)]
pub struct Crtc {
    /// The CRTC handle
    pub handle: crtc::Handle,
    /// Currently active mode (if any)
    pub mode: Option<Mode>,
    /// Currently attached framebuffer (if any)
    pub framebuffer: Option<framebuffer::Handle>,
}

/// Represents a plane (for hardware compositing).
#[derive(Debug, Clone)]
pub struct Plane {
    /// The plane handle
    pub handle: plane::Handle,
    /// Plane type (Primary, Cursor, Overlay)
    pub plane_type: PlaneType,
    /// Supported CRTCs
    pub possible_crtcs: Vec<crtc::Handle>,
    /// Supported formats
    pub formats: Vec<u32>,
}

/// Type of plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaneType {
    /// Primary plane - main scanout surface
    Primary,
    /// Cursor plane
    Cursor,
    /// Overlay plane - for hardware compositing
    Overlay,
}

/// A configured output (connector + CRTC + primary plane).
#[derive(Debug)]
pub struct Output {
    /// The connector for this output
    pub connector: Connector,
    /// The CRTC driving this output
    pub crtc: crtc::Handle,
    /// The primary plane for this CRTC
    pub primary_plane: plane::Handle,
    /// The active display mode
    pub mode: Mode,
    /// Property handles for atomic commits
    pub props: OutputProperties,
}

/// Property handles needed for atomic modesetting.
#[derive(Debug)]
pub struct OutputProperties {
    // Connector properties
    pub connector_crtc_id: property::Handle,

    // CRTC properties
    pub crtc_active: property::Handle,
    pub crtc_mode_id: property::Handle,

    // Plane properties
    pub plane_fb_id: property::Handle,
    pub plane_crtc_id: property::Handle,
    pub plane_crtc_x: property::Handle,
    pub plane_crtc_y: property::Handle,
    pub plane_crtc_w: property::Handle,
    pub plane_crtc_h: property::Handle,
    pub plane_src_x: property::Handle,
    pub plane_src_y: property::Handle,
    pub plane_src_w: property::Handle,
    pub plane_src_h: property::Handle,
}

/// Manages DRM outputs (displays).
pub struct OutputManager {
    /// Available connectors
    pub connectors: Vec<Connector>,
    /// Available CRTCs
    pub crtcs: Vec<Crtc>,
    /// Available planes
    pub planes: Vec<Plane>,
    /// Configured outputs
    pub outputs: Vec<Output>,
}

impl OutputManager {
    /// Creates a new output manager by enumerating DRM resources.
    pub fn new(device: &DrmDevice) -> anyhow::Result<Self> {
        let resources = device
            .resource_handles()
            .context("Failed to get DRM resources")?;

        // Enumerate connectors
        let mut connectors = Vec::new();
        for &handle in resources.connectors() {
            if let Ok(info) = device.get_connector(handle, false) {
                let connector = Connector {
                    handle,
                    connector_type: info.interface(),
                    connection: info.state(),
                    modes: info.modes().to_vec(),
                    physical_size: info.size().unwrap_or((0, 0)),
                    encoder: info.current_encoder(),
                };
                debug!(
                    "Found connector: {:?} ({:?})",
                    connector.connector_type, connector.connection
                );
                connectors.push(connector);
            }
        }

        // Enumerate CRTCs
        let mut crtcs = Vec::new();
        for &handle in resources.crtcs() {
            if let Ok(info) = device.get_crtc(handle) {
                let crtc = Crtc {
                    handle,
                    mode: info.mode(),
                    framebuffer: info.framebuffer(),
                };
                debug!("Found CRTC: {:?}", handle);
                crtcs.push(crtc);
            }
        }

        // Enumerate planes
        let plane_resources = device
            .plane_handles()
            .context("Failed to get plane resources")?;

        let mut planes = Vec::new();
        for &handle in plane_resources.iter() {
            if let Ok(info) = device.get_plane(handle) {
                // Get plane type from properties
                let plane_type = Self::get_plane_type(device, handle)?;

                // Get the list of CRTCs this plane can work with
                let possible_crtcs_list = resources.filter_crtcs(info.possible_crtcs());

                let plane = Plane {
                    handle,
                    plane_type,
                    possible_crtcs: possible_crtcs_list,
                    formats: info.formats().to_vec(),
                };
                debug!("Found plane: {:?} ({:?})", handle, plane_type);
                planes.push(plane);
            }
        }

        info!(
            "DRM resources: {} connectors, {} CRTCs, {} planes",
            connectors.len(),
            crtcs.len(),
            planes.len()
        );

        Ok(Self {
            connectors,
            crtcs,
            planes,
            outputs: Vec::new(),
        })
    }

    /// Gets the plane type from its properties.
    fn get_plane_type(device: &DrmDevice, handle: plane::Handle) -> anyhow::Result<PlaneType> {
        let props = device
            .get_properties(handle)
            .context("Failed to get plane properties")?;

        for (&prop_handle, &value) in props.iter() {
            if let Ok(prop_info) = device.get_property(prop_handle) {
                if prop_info.name().to_str() == Ok("type") {
                    return Ok(match value {
                        0 => PlaneType::Overlay,
                        1 => PlaneType::Primary,
                        2 => PlaneType::Cursor,
                        _ => PlaneType::Overlay,
                    });
                }
            }
        }

        Ok(PlaneType::Overlay)
    }

    /// Configures outputs for all connected displays.
    ///
    /// This finds connected connectors and assigns CRTCs and planes.
    pub fn configure_outputs(&mut self, device: &DrmDevice) -> anyhow::Result<()> {
        self.outputs.clear();

        let mut used_crtcs = Vec::new();
        let mut used_planes = Vec::new();

        for connector in &self.connectors {
            // Skip disconnected connectors
            if connector.connection != connector::State::Connected {
                continue;
            }

            // Skip connectors without modes
            if connector.modes.is_empty() {
                warn!(
                    "Connected connector {:?} has no modes",
                    connector.connector_type
                );
                continue;
            }

            // Find a CRTC for this connector
            let crtc = self.find_crtc_for_connector(device, connector, &used_crtcs);
            let Some(crtc_handle) = crtc else {
                warn!(
                    "No available CRTC for connector {:?}",
                    connector.connector_type
                );
                continue;
            };

            // Find primary plane for this CRTC
            let _crtc_index = self
                .crtcs
                .iter()
                .position(|c| c.handle == crtc_handle)
                .unwrap();

            let primary_plane = self
                .planes
                .iter()
                .find(|p| {
                    p.plane_type == PlaneType::Primary
                        && p.possible_crtcs.contains(&crtc_handle)
                        && !used_planes.contains(&p.handle)
                })
                .map(|p| p.handle);

            let Some(plane_handle) = primary_plane else {
                warn!("No primary plane available for CRTC {:?}", crtc_handle);
                continue;
            };

            // Select preferred mode (first mode is usually the preferred/native one)
            let mode = connector.modes[0];

            // Get property handles
            let props = self.get_output_properties(device, connector.handle, crtc_handle, plane_handle)?;

            used_crtcs.push(crtc_handle);
            used_planes.push(plane_handle);

            info!(
                "Configured output: {:?} @ {}x{} {}Hz",
                connector.connector_type,
                mode.size().0,
                mode.size().1,
                mode.vrefresh()
            );

            self.outputs.push(Output {
                connector: connector.clone(),
                crtc: crtc_handle,
                primary_plane: plane_handle,
                mode,
                props,
            });
        }

        if self.outputs.is_empty() {
            anyhow::bail!("No outputs could be configured");
        }

        info!("Configured {} output(s)", self.outputs.len());

        Ok(())
    }

    /// Finds an available CRTC for a connector.
    fn find_crtc_for_connector(
        &self,
        device: &DrmDevice,
        connector: &Connector,
        used_crtcs: &[crtc::Handle],
    ) -> Option<crtc::Handle> {
        // If the connector has an encoder, prefer its CRTC
        if let Some(encoder_handle) = connector.encoder {
            if let Ok(encoder) = device.get_encoder(encoder_handle) {
                if let Some(crtc) = encoder.crtc() {
                    if !used_crtcs.contains(&crtc) {
                        return Some(crtc);
                    }
                }
            }
        }

        // Otherwise, find a compatible CRTC
        let connector_info = device.get_connector(connector.handle, false).ok()?;

        // Get resources to use filter_crtcs
        let resources = device.resource_handles().ok()?;

        for &encoder_handle in connector_info.encoders() {
            if let Ok(encoder) = device.get_encoder(encoder_handle) {
                // Get the list of CRTCs this encoder can work with
                let possible_crtcs = resources.filter_crtcs(encoder.possible_crtcs());

                for crtc in &self.crtcs {
                    if possible_crtcs.contains(&crtc.handle) && !used_crtcs.contains(&crtc.handle) {
                        return Some(crtc.handle);
                    }
                }
            }
        }

        None
    }

    /// Gets property handles for atomic modesetting.
    fn get_output_properties(
        &self,
        device: &DrmDevice,
        connector: connector::Handle,
        crtc: crtc::Handle,
        plane: plane::Handle,
    ) -> anyhow::Result<OutputProperties> {
        Ok(OutputProperties {
            connector_crtc_id: Self::find_property(device, connector, "CRTC_ID")?,
            crtc_active: Self::find_property(device, crtc, "ACTIVE")?,
            crtc_mode_id: Self::find_property(device, crtc, "MODE_ID")?,
            plane_fb_id: Self::find_property(device, plane, "FB_ID")?,
            plane_crtc_id: Self::find_property(device, plane, "CRTC_ID")?,
            plane_crtc_x: Self::find_property(device, plane, "CRTC_X")?,
            plane_crtc_y: Self::find_property(device, plane, "CRTC_Y")?,
            plane_crtc_w: Self::find_property(device, plane, "CRTC_W")?,
            plane_crtc_h: Self::find_property(device, plane, "CRTC_H")?,
            plane_src_x: Self::find_property(device, plane, "SRC_X")?,
            plane_src_y: Self::find_property(device, plane, "SRC_Y")?,
            plane_src_w: Self::find_property(device, plane, "SRC_W")?,
            plane_src_h: Self::find_property(device, plane, "SRC_H")?,
        })
    }

    /// Finds a property by name.
    fn find_property<T: ResourceHandle>(
        device: &DrmDevice,
        handle: T,
        name: &str,
    ) -> anyhow::Result<property::Handle> {
        let props = device
            .get_properties(handle)
            .context("Failed to get properties")?;

        for (&prop_handle, &_value) in props.iter() {
            if let Ok(prop_info) = device.get_property(prop_handle) {
                if prop_info.name().to_str() == Ok(name) {
                    return Ok(prop_handle);
                }
            }
        }

        anyhow::bail!("Property '{}' not found", name)
    }

    /// Performs an atomic commit to enable outputs and set modes.
    pub fn atomic_enable(&self, device: &DrmDevice) -> anyhow::Result<()> {
        let mut atomic_req = drm::control::atomic::AtomicModeReq::new();

        for output in &self.outputs {
            let (width, height) = output.mode.size();

            // Create a mode blob
            let mode_blob = device
                .create_property_blob(&output.mode)
                .context("Failed to create mode blob")?;

            // Set connector properties
            atomic_req.add_property(
                output.connector.handle,
                output.props.connector_crtc_id,
                property::Value::CRTC(Some(output.crtc)),
            );

            // Set CRTC properties
            atomic_req.add_property(output.crtc, output.props.crtc_active, property::Value::Boolean(true));
            atomic_req.add_property(
                output.crtc,
                output.props.crtc_mode_id,
                property::Value::Blob(mode_blob.into()),
            );

            // Set plane properties (position and size)
            // Note: FB_ID will be set when we have a framebuffer to display
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_crtc_id,
                property::Value::CRTC(Some(output.crtc)),
            );
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_crtc_x,
                property::Value::UnsignedRange(0),
            );
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_crtc_y,
                property::Value::UnsignedRange(0),
            );
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_crtc_w,
                property::Value::UnsignedRange(width as u64),
            );
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_crtc_h,
                property::Value::UnsignedRange(height as u64),
            );
            // Source coordinates are in 16.16 fixed point
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_src_x,
                property::Value::UnsignedRange(0),
            );
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_src_y,
                property::Value::UnsignedRange(0),
            );
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_src_w,
                property::Value::UnsignedRange((width as u64) << 16),
            );
            atomic_req.add_property(
                output.primary_plane,
                output.props.plane_src_h,
                property::Value::UnsignedRange((height as u64) << 16),
            );
        }

        device
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, atomic_req)
            .context("Failed to commit atomic modeset")?;

        info!("Atomic modeset committed successfully");

        Ok(())
    }

    /// Performs an atomic page flip with a new framebuffer.
    pub fn atomic_page_flip(
        &self,
        device: &DrmDevice,
        output_index: usize,
        fb: framebuffer::Handle,
    ) -> anyhow::Result<()> {
        let output = &self.outputs[output_index];

        let mut atomic_req = drm::control::atomic::AtomicModeReq::new();

        atomic_req.add_property(
            output.primary_plane,
            output.props.plane_fb_id,
            property::Value::Framebuffer(Some(fb)),
        );

        device
            .atomic_commit(
                AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT,
                atomic_req,
            )
            .context("Failed to atomic page flip")?;

        Ok(())
    }
}
