//! Screen capture through DXGI Desktop Duplication.
//!
//! Duplication hands out the frame the compositor already composed, as a GPU
//! texture, with no copy through system memory and no per-frame permission
//! prompt. That makes it both the fastest and the least intrusive option on
//! Windows — the opposite of the Wayland situation, where the fast path is the
//! one that needs approval.
//!
//! Two constraints shape everything below. The D3D11 device must be created on
//! the adapter that actually drives the output, or duplication fails outright on
//! laptops with switchable graphics. And a frame must be released before the
//! next is acquired, because duplication hands out one surface at a time and
//! holding it stalls the desktop.

use anyhow::{Context, bail};
use std::sync::Arc;
use tracing::debug;
use waypad_core::stream::ScreenSource;
use windows::Win32::Graphics::{
    Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0},
    Direct3D11::{
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
        ID3D11DeviceContext, ID3D11Texture2D,
    },
    Dxgi::{
        CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
        DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
        IDXGIOutputDuplication, IDXGIResource,
    },
};
use windows::core::Interface;

/// A monitor that can be duplicated, and where it sits on the desktop.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    /// The GDI device name, e.g. `\\.\DISPLAY1`. Stable enough to identify a
    /// monitor across a stream restart, which is what the source id needs.
    pub device_name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// The monitor at the desktop origin, which is what a client should get
    /// when it does not ask for a particular one.
    pub primary: bool,
    /// Index of the adapter driving this output, so the capture device is
    /// created on the right GPU.
    adapter_index: u32,
    /// Index of the output on that adapter.
    output_index: u32,
}

impl OutputInfo {
    /// The id carried on the wire. Prefixed like the Linux ones so a client
    /// never has to guess which backend a source came from.
    pub fn source_id(&self) -> String {
        format!("windows:monitor:{}", self.device_name)
    }

    pub fn to_source(&self) -> ScreenSource {
        ScreenSource {
            id: self.source_id(),
            label: format!(
                "{} ({}x{})",
                self.device_name.trim_start_matches(r"\\.\"),
                self.width,
                self.height
            ),
            kind: "monitor".into(),
            backend: "windows-dxgi".into(),
            width: self.width,
            height: self.height,
            x: self.x,
            y: self.y,
            scale: 1.0,
            focused: self.primary,
        }
    }
}

/// Lists every duplicable monitor.
pub fn enumerate_outputs() -> anyhow::Result<Vec<OutputInfo>> {
    // SAFETY: the factory is created and released by this function; every
    // enumeration call reports absence through an error rather than a null.
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().context("failed to create a DXGI factory")?;
        let mut outputs = Vec::new();
        for adapter_index in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(adapter_index) else {
                break;
            };
            for output_index in 0.. {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                let Ok(desc) = output.GetDesc() else {
                    continue;
                };
                // An output that is not attached has no desktop rectangle and
                // cannot be duplicated.
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let bounds = desc.DesktopCoordinates;
                let width = (bounds.right - bounds.left).max(0) as u32;
                let height = (bounds.bottom - bounds.top).max(0) as u32;
                if width == 0 || height == 0 {
                    continue;
                }
                outputs.push(OutputInfo {
                    device_name: String::from_utf16_lossy(
                        &desc.DeviceName[..desc
                            .DeviceName
                            .iter()
                            .position(|c| *c == 0)
                            .unwrap_or(desc.DeviceName.len())],
                    ),
                    x: bounds.left,
                    y: bounds.top,
                    width,
                    height,
                    primary: bounds.left == 0 && bounds.top == 0,
                    adapter_index,
                    output_index,
                });
            }
        }
        Ok(outputs)
    }
}

/// A live duplication session for one monitor.
pub struct DuplicatedOutput {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    info: OutputInfo,
    /// True while a frame is checked out and must be released before the next
    /// acquire. Duplication hands out one surface at a time.
    frame_held: bool,
}

impl std::fmt::Debug for DuplicatedOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplicatedOutput")
            .field("output", &self.info.device_name)
            .finish_non_exhaustive()
    }
}

impl DuplicatedOutput {
    pub fn open(info: &OutputInfo) -> anyhow::Result<Self> {
        // SAFETY: each COM call is checked; the device and duplication live as
        // long as this struct and are released by their Drop impls.
        unsafe {
            let factory: IDXGIFactory1 =
                CreateDXGIFactory1().context("failed to create a DXGI factory")?;
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapters1(info.adapter_index)
                .with_context(|| format!("adapter {} disappeared", info.adapter_index))?;

            // D3D_DRIVER_TYPE_UNKNOWN is required when an adapter is passed
            // explicitly, and passing it explicitly is the whole point: on a
            // laptop with switchable graphics a device created on the default
            // adapter cannot duplicate an output driven by the other one.
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("failed to create a Direct3D 11 device for capture")?;
            let device = device.context("Direct3D returned no device")?;
            let context = context.context("Direct3D returned no device context")?;

            let output = adapter
                .EnumOutputs(info.output_index)
                .with_context(|| format!("output {} disappeared", info.output_index))?;
            let output: IDXGIOutput1 = output
                .cast()
                .context("this output does not support desktop duplication")?;
            let duplication = output.DuplicateOutput(&device).context(
                "DuplicateOutput failed. Another application may already be duplicating this \
                 monitor, or the session may not have an interactive desktop.",
            )?;

            debug!(
                output = %info.device_name,
                width = info.width,
                height = info.height,
                "desktop duplication opened"
            );
            Ok(Self {
                device,
                context,
                duplication,
                info: info.clone(),
                frame_held: false,
            })
        }
    }

    pub fn info(&self) -> &OutputInfo {
        &self.info
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    /// Waits up to `timeout_ms` for the desktop to change, and returns the new
    /// frame as a GPU texture.
    ///
    /// `Ok(None)` means no new desktop image arrived in that window, which is
    /// the normal case on a still desktop and not an error: the caller repeats
    /// the last picture or simply waits.
    ///
    /// Duplication also delivers frames that carry only a cursor update, with
    /// `LastPresentTime` left at zero and a texture whose contents are not a
    /// valid desktop image — in practice all black. Those are reported as
    /// `None` too. Handing one on would look like a working capture that
    /// produces a black screen, which is a great deal harder to diagnose than
    /// no capture at all.
    pub fn acquire(&mut self, timeout_ms: u32) -> anyhow::Result<Option<ID3D11Texture2D>> {
        self.release()?;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: the duplication is live and both out-parameters are owned here.
        let result = unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        };
        match result {
            Ok(()) => {}
            Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(err) if err.code() == DXGI_ERROR_ACCESS_LOST => {
                // A resolution change, a full-screen transition, or a session
                // switch invalidates duplication. Reported distinctly so the
                // caller can reopen instead of ending the stream.
                bail!("desktop duplication lost access and must be reopened: {err}")
            }
            Err(err) => bail!("AcquireNextFrame failed: {err}"),
        }
        self.frame_held = true;
        if info.LastPresentTime == 0 {
            // Cursor-only update: the surface holds no new desktop image.
            self.release()?;
            return Ok(None);
        }
        let resource = resource.context("duplication returned no surface")?;
        let texture: ID3D11Texture2D = resource
            .cast()
            .context("duplication surface is not a 2D texture")?;
        Ok(Some(texture))
    }

    /// Hands the surface back. Safe to call when nothing is held.
    pub fn release(&mut self) -> anyhow::Result<()> {
        if !self.frame_held {
            return Ok(());
        }
        self.frame_held = false;
        // SAFETY: paired with a successful AcquireNextFrame.
        unsafe { self.duplication.ReleaseFrame() }.context("ReleaseFrame failed")
    }
}

impl Drop for DuplicatedOutput {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// Picks the output a source id refers to, falling back to the primary.
pub fn resolve_output(source_id: &str) -> anyhow::Result<OutputInfo> {
    let outputs = enumerate_outputs()?;
    if outputs.is_empty() {
        bail!("Windows reports no monitor attached to the desktop");
    }
    if let Some(found) = outputs
        .iter()
        .find(|output| output.source_id() == source_id)
    {
        return Ok(found.clone());
    }
    outputs
        .iter()
        .find(|output| output.primary)
        .or_else(|| outputs.first())
        .cloned()
        .context("no usable monitor")
}

/// Shared handle so the capture backend can enumerate without reopening a
/// device for every call.
pub type SharedOutputs = Arc<Vec<OutputInfo>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerates_at_least_the_primary_monitor() {
        let outputs = enumerate_outputs().expect("enumeration succeeds on a desktop host");
        assert!(!outputs.is_empty(), "a desktop host has a monitor");
        assert!(
            outputs.iter().any(|output| output.primary),
            "one output sits at the desktop origin: {outputs:#?}"
        );
        for output in &outputs {
            assert!(output.width > 0 && output.height > 0, "{output:?}");
            assert!(output.device_name.starts_with(r"\\.\"), "{output:?}");
        }
    }

    #[test]
    fn source_ids_are_stable_and_backend_tagged() {
        let outputs = enumerate_outputs().unwrap();
        let source = outputs[0].to_source();
        assert!(source.id.starts_with("windows:monitor:"));
        assert_eq!(source.backend, "windows-dxgi");
        assert_eq!(source.kind, "monitor");
        // Enumerating twice must give the same id, or a reconnecting client
        // would lose its selected monitor.
        assert_eq!(enumerate_outputs().unwrap()[0].source_id(), source.id);
    }

    #[test]
    fn resolve_falls_back_to_the_primary_for_an_unknown_id() {
        let resolved = resolve_output("windows:monitor:\\\\.\\NOPE").unwrap();
        assert!(resolved.primary || enumerate_outputs().unwrap().len() == 1);
    }
}
