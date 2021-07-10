/*!
# DirectX12 API internals.

## Pipeline Layout

!*/

#![allow(unused_variables)]

mod adapter;
mod command;
mod conv;
mod descriptor;
mod device;
mod instance;

use arrayvec::ArrayVec;
use parking_lot::Mutex;
use std::{borrow::Cow, fmt, mem, ptr, sync::Arc};
use winapi::{
    shared::{dxgi, dxgi1_2, dxgi1_4, dxgiformat, dxgitype, windef, winerror},
    um::{d3d12, synchapi, winbase, winnt},
    Interface as _,
};

#[derive(Clone)]
pub struct Api;
//TODO: remove these temporaries
#[derive(Debug)]
pub struct Resource;

impl crate::Api for Api {
    type Instance = Instance;
    type Surface = Surface;
    type Adapter = Adapter;
    type Device = Device;

    type Queue = Queue;
    type CommandEncoder = CommandEncoder;
    type CommandBuffer = CommandBuffer;

    type Buffer = Buffer;
    type Texture = Texture;
    type SurfaceTexture = Texture;
    type TextureView = TextureView;
    type Sampler = Sampler;
    type QuerySet = QuerySet;
    type Fence = Fence;

    type BindGroupLayout = BindGroupLayout;
    type BindGroup = BindGroup;
    type PipelineLayout = PipelineLayout;
    type ShaderModule = Resource;
    type RenderPipeline = Resource;
    type ComputePipeline = Resource;
}

trait HResult<O> {
    fn into_result(self) -> Result<O, Cow<'static, str>>;
    fn into_device_result(self, description: &str) -> Result<O, crate::DeviceError>;
}
impl HResult<()> for i32 {
    fn into_result(self) -> Result<(), Cow<'static, str>> {
        if self >= 0 {
            return Ok(());
        }
        let description = match self {
            winerror::E_UNEXPECTED => "unexpected",
            winerror::E_NOTIMPL => "not implemented",
            winerror::E_OUTOFMEMORY => "out of memory",
            winerror::E_INVALIDARG => "invalid argument",
            _ => return Err(Cow::Owned(format!("0x{:X}", self as u32))),
        };
        Err(Cow::Borrowed(description))
    }
    fn into_device_result(self, description: &str) -> Result<(), crate::DeviceError> {
        self.into_result().map_err(|err| {
            log::error!("{} failed: {}", description, err);
            if self == winerror::E_OUTOFMEMORY {
                crate::DeviceError::OutOfMemory
            } else {
                crate::DeviceError::Lost
            }
        })
    }
}

impl<T> HResult<T> for (T, i32) {
    fn into_result(self) -> Result<T, Cow<'static, str>> {
        self.1.into_result().map(|()| self.0)
    }
    fn into_device_result(self, description: &str) -> Result<T, crate::DeviceError> {
        self.1.into_device_result(description).map(|()| self.0)
    }
}

const ZERO_BUFFER_SIZE: wgt::BufferAddress = 256 << 10;

pub struct Instance {
    factory: native::Factory4,
    library: Arc<native::D3D12Lib>,
    lib_dxgi: native::DxgiLib,
}

unsafe impl Send for Instance {}
unsafe impl Sync for Instance {}

struct SwapChain {
    raw: native::WeakPtr<dxgi1_4::IDXGISwapChain3>,
    // need to associate raw image pointers with the swapchain so they can be properly released
    // when the swapchain is destroyed
    resources: Vec<native::Resource>,
    waitable: winnt::HANDLE,
    acquired_count: usize,
}

pub struct Surface {
    factory: native::WeakPtr<dxgi1_4::IDXGIFactory4>,
    wnd_handle: windef::HWND,
    swap_chain: Option<SwapChain>,
}

unsafe impl Send for Surface {}
unsafe impl Sync for Surface {}

#[derive(Debug, Clone, Copy)]
enum MemoryArchitecture {
    Unified { cache_coherent: bool },
    NonUnified,
}

#[derive(Debug, Clone, Copy)]
struct PrivateCapabilities {
    heterogeneous_resource_heaps: bool,
    memory_architecture: MemoryArchitecture,
}

#[derive(Default)]
struct Workarounds {
    // On WARP, temporary CPU descriptors are still used by the runtime
    // after we call `CopyDescriptors`.
    avoid_cpu_descriptor_overwrites: bool,
}

pub struct Adapter {
    raw: native::WeakPtr<dxgi1_2::IDXGIAdapter2>,
    device: native::Device,
    library: Arc<native::D3D12Lib>,
    private_caps: PrivateCapabilities,
    workarounds: Workarounds,
}

unsafe impl Send for Adapter {}
unsafe impl Sync for Adapter {}

/// Helper structure for waiting for GPU.
struct Idler {
    fence: native::Fence,
    event: native::Event,
}

struct CommandSignatures {
    draw: native::CommandSignature,
    draw_indexed: native::CommandSignature,
    dispatch: native::CommandSignature,
}

impl CommandSignatures {
    unsafe fn destroy(&self) {
        self.draw.destroy();
        self.draw_indexed.destroy();
        self.dispatch.destroy();
    }
}

struct DeviceShared {
    zero_buffer: native::Resource,
    cmd_signatures: CommandSignatures,
    heap_views: descriptor::GeneralHeap,
    heap_samplers: descriptor::GeneralHeap,
}

impl DeviceShared {
    unsafe fn destroy(&self) {
        self.zero_buffer.destroy();
        self.cmd_signatures.destroy();
        self.heap_views.raw.destroy();
        self.heap_samplers.raw.destroy();
    }
}

pub struct Device {
    raw: native::Device,
    present_queue: native::CommandQueue,
    idler: Idler,
    private_caps: PrivateCapabilities,
    shared: Arc<DeviceShared>,
    // CPU only pools
    rtv_pool: Mutex<descriptor::CpuPool>,
    dsv_pool: Mutex<descriptor::CpuPool>,
    srv_uav_pool: Mutex<descriptor::CpuPool>,
    sampler_pool: Mutex<descriptor::CpuPool>,
    // library
    library: Arc<native::D3D12Lib>,
}

unsafe impl Send for Device {}
unsafe impl Sync for Device {}

pub struct Queue {
    raw: native::CommandQueue,
}

unsafe impl Send for Queue {}
unsafe impl Sync for Queue {}

#[derive(Default)]
struct Temp {
    marker: Vec<u16>,
    barriers: Vec<d3d12::D3D12_RESOURCE_BARRIER>,
}

impl Temp {
    fn clear(&mut self) {
        self.marker.clear();
        self.barriers.clear();
    }
}

struct PassResolve {
    src: (native::Resource, u32),
    dst: (native::Resource, u32),
    format: dxgiformat::DXGI_FORMAT,
}

enum PassKind {
    Render,
    Compute,
    Transfer,
}

struct PassState {
    has_label: bool,
    resolves: ArrayVec<[PassResolve; crate::MAX_COLOR_TARGETS]>,
    vertex_buffers: [d3d12::D3D12_VERTEX_BUFFER_VIEW; crate::MAX_VERTEX_BUFFERS],
    dirty_vertex_buffers: usize,
    kind: PassKind,
}

impl PassState {
    fn new() -> Self {
        PassState {
            has_label: false,
            resolves: ArrayVec::new(),
            vertex_buffers: [unsafe { mem::zeroed() }; crate::MAX_VERTEX_BUFFERS],
            dirty_vertex_buffers: 0,
            kind: PassKind::Transfer,
        }
    }

    fn clear(&mut self) {
        self.has_label = false;
        self.resolves.clear();
        self.dirty_vertex_buffers = 0;
        self.kind = PassKind::Transfer;
    }
}

pub struct CommandEncoder {
    allocator: native::CommandAllocator,
    device: native::Device,
    shared: Arc<DeviceShared>,
    list: Option<native::GraphicsCommandList>,
    free_lists: Vec<native::GraphicsCommandList>,
    pass: PassState,
    temp: Temp,
}

unsafe impl Send for CommandEncoder {}
unsafe impl Sync for CommandEncoder {}

pub struct CommandBuffer {
    raw: native::GraphicsCommandList,
}

unsafe impl Send for CommandBuffer {}
unsafe impl Sync for CommandBuffer {}

#[derive(Debug)]
pub struct Buffer {
    resource: native::Resource,
    size: wgt::BufferAddress,
}

unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

impl crate::BufferBinding<'_, Api> {
    fn resolve_size(&self) -> wgt::BufferAddress {
        match self.size {
            Some(size) => size.get(),
            None => self.buffer.size - self.offset,
        }
    }

    fn resolve_address(&self) -> wgt::BufferAddress {
        self.buffer.resource.gpu_virtual_address() + self.offset
    }
}

#[derive(Debug)]
pub struct Texture {
    resource: native::Resource,
    format: wgt::TextureFormat,
    dimension: wgt::TextureDimension,
    size: wgt::Extent3d,
    mip_level_count: u32,
    sample_count: u32,
}

unsafe impl Send for Texture {}
unsafe impl Sync for Texture {}

impl Texture {
    fn array_layer_count(&self) -> u32 {
        match self.dimension {
            wgt::TextureDimension::D1 | wgt::TextureDimension::D2 => {
                self.size.depth_or_array_layers
            }
            wgt::TextureDimension::D3 => 1,
        }
    }

    fn calc_subresource(&self, mip_level: u32, array_layer: u32, plane: u32) -> u32 {
        mip_level + (array_layer + plane * self.array_layer_count()) * self.mip_level_count
    }

    fn calc_subresource_for_copy(&self, base: &crate::TextureCopyBase) -> u32 {
        self.calc_subresource(base.mip_level, base.array_layer, 0)
    }
}

#[derive(Debug)]
pub struct TextureView {
    raw_format: dxgiformat::DXGI_FORMAT,
    target_base: (native::Resource, u32),
    handle_srv: Option<descriptor::Handle>,
    handle_uav: Option<descriptor::Handle>,
    handle_rtv: Option<descriptor::Handle>,
    handle_dsv_ro: Option<descriptor::Handle>,
    handle_dsv_rw: Option<descriptor::Handle>,
}

unsafe impl Send for TextureView {}
unsafe impl Sync for TextureView {}

#[derive(Debug)]
pub struct Sampler {
    handle: descriptor::Handle,
}

unsafe impl Send for Sampler {}
unsafe impl Sync for Sampler {}

#[derive(Debug)]
pub struct QuerySet {
    raw: native::QueryHeap,
    raw_ty: d3d12::D3D12_QUERY_TYPE,
}

unsafe impl Send for QuerySet {}
unsafe impl Sync for QuerySet {}

#[derive(Debug)]
pub struct Fence {
    raw: native::Fence,
}

unsafe impl Send for Fence {}
unsafe impl Sync for Fence {}

pub struct BindGroupLayout {
    /// Sorted list of entries.
    entries: Vec<wgt::BindGroupLayoutEntry>,
}

enum BufferViewKind {
    Constant,
    ShaderResource,
    UnorderedAccess,
}

pub struct BindGroup {
    gpu_views: d3d12::D3D12_GPU_DESCRIPTOR_HANDLE,
    gpu_samplers: d3d12::D3D12_GPU_DESCRIPTOR_HANDLE,
    dynamic_buffers: Vec<native::GpuAddress>,
}

impl fmt::Debug for BindGroup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BindGroup")
            .field("gpu_views", &self.gpu_views.ptr)
            .field("gpu_samplers", &self.gpu_samplers.ptr)
            .field("dynamic_buffers", &self.dynamic_buffers)
            .finish()
    }
}

bitflags::bitflags! {
    struct TableTypes: u8 {
        const SRV_CBV_UAV = 0x1;
        const SAMPLERS = 0x2;
    }
}

struct BindGroupInfo {
    base_root_index: u32,
    tables: TableTypes,
    dynamic_buffers: Vec<BufferViewKind>,
}

pub struct PipelineLayout {
    raw: native::RootSignature,
    // Storing for each associated bind group, which tables we created
    // in the root signature. This is required for binding descriptor sets.
    bind_group_infos: ArrayVec<BindGroupInfo, crate::MAX_BIND_GROUPS>,
}

unsafe impl Send for PipelineLayout {}
unsafe impl Sync for PipelineLayout {}

impl SwapChain {
    unsafe fn release_resources(self) -> native::WeakPtr<dxgi1_4::IDXGISwapChain3> {
        for resource in self.resources {
            resource.destroy();
        }
        self.raw
    }

    unsafe fn wait(&mut self, timeout_ms: u32) -> Result<bool, crate::SurfaceError> {
        match synchapi::WaitForSingleObject(self.waitable, timeout_ms) {
            winbase::WAIT_ABANDONED | winbase::WAIT_FAILED => Err(crate::SurfaceError::Lost),
            winbase::WAIT_OBJECT_0 => Ok(true),
            winerror::WAIT_TIMEOUT => Ok(false),
            other => {
                log::error!("Unexpected wait status: 0x{:x}", other);
                Err(crate::SurfaceError::Lost)
            }
        }
    }
}

impl crate::Surface<Api> for Surface {
    unsafe fn configure(
        &mut self,
        device: &Device,
        config: &crate::SurfaceConfiguration,
    ) -> Result<(), crate::SurfaceError> {
        let mut flags = dxgi::DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
        match config.present_mode {
            wgt::PresentMode::Immediate => {
                flags |= dxgi::DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
            }
            _ => {}
        }

        let non_srgb_format = conv::map_texture_format_nosrgb(config.format);

        let swap_chain = match self.swap_chain.take() {
            Some(sc) => {
                // can't have image resources in flight used by GPU
                let _ = device.wait_idle();

                let raw = sc.release_resources();
                let result = raw.ResizeBuffers(
                    config.swap_chain_size,
                    config.extent.width,
                    config.extent.height,
                    non_srgb_format,
                    flags,
                );
                if let Err(err) = result.into_result() {
                    log::error!("ResizeBuffers failed: {}", err);
                    return Err(crate::SurfaceError::Other("window is in use"));
                }
                raw
            }
            None => {
                let mut swap_chain1 = native::WeakPtr::<dxgi1_2::IDXGISwapChain1>::null();

                let raw_desc = dxgi1_2::DXGI_SWAP_CHAIN_DESC1 {
                    AlphaMode: conv::map_acomposite_alpha_mode(config.composite_alpha_mode),
                    BufferCount: config.swap_chain_size,
                    Width: config.extent.width,
                    Height: config.extent.height,
                    Format: non_srgb_format,
                    Flags: flags,
                    BufferUsage: dxgitype::DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    SampleDesc: dxgitype::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Scaling: dxgi1_2::DXGI_SCALING_STRETCH,
                    Stereo: 0,
                    SwapEffect: dxgi::DXGI_SWAP_EFFECT_FLIP_DISCARD,
                };

                let hr = self.factory.CreateSwapChainForHwnd(
                    device.present_queue.as_mut_ptr() as *mut _,
                    self.wnd_handle,
                    &raw_desc,
                    ptr::null(),
                    ptr::null_mut(),
                    swap_chain1.mut_void() as *mut *mut _,
                );

                if let Err(err) = hr.into_result() {
                    log::error!("SwapChain creation error: {}", err);
                    return Err(crate::SurfaceError::Other("swap chain creation"));
                }

                match swap_chain1.cast::<dxgi1_4::IDXGISwapChain3>().into_result() {
                    Ok(swap_chain3) => {
                        swap_chain1.destroy();
                        swap_chain3
                    }
                    Err(err) => {
                        log::error!("Unable to cast swap chain: {}", err);
                        return Err(crate::SurfaceError::Other("swap chain cast to 3"));
                    }
                }
            }
        };

        // Disable automatic Alt+Enter handling by DXGI.
        const DXGI_MWA_NO_WINDOW_CHANGES: u32 = 1;
        const DXGI_MWA_NO_ALT_ENTER: u32 = 2;
        self.factory.MakeWindowAssociation(
            self.wnd_handle,
            DXGI_MWA_NO_WINDOW_CHANGES | DXGI_MWA_NO_ALT_ENTER,
        );

        swap_chain.SetMaximumFrameLatency(config.swap_chain_size);
        let waitable = swap_chain.GetFrameLatencyWaitableObject();

        let mut resources = vec![native::Resource::null(); config.swap_chain_size as usize];
        for (i, res) in resources.iter_mut().enumerate() {
            swap_chain.GetBuffer(i as _, &d3d12::ID3D12Resource::uuidof(), res.mut_void());
        }

        self.swap_chain = Some(SwapChain {
            raw: swap_chain,
            resources,
            waitable,
            acquired_count: 0,
            //format: config.format,
            //size: config.extent,
            //mode: config.present_mode,
        });

        Ok(())
    }

    unsafe fn unconfigure(&mut self, device: &Device) {
        if let Some(mut sc) = self.swap_chain.take() {
            let _ = sc.wait(winbase::INFINITE);
            //TODO: this shouldn't be needed,
            // but it complains that the queue is still used otherwise
            let _ = device.wait_idle();
            let raw = sc.release_resources();
            raw.destroy();
        }
    }

    unsafe fn acquire_texture(
        &mut self,
        timeout_ms: u32,
    ) -> Result<Option<crate::AcquiredSurfaceTexture<Api>>, crate::SurfaceError> {
        Ok(None)
    }
    unsafe fn discard_texture(&mut self, texture: Texture) {}
}

impl crate::Queue<Api> for Queue {
    unsafe fn submit(
        &mut self,
        command_buffers: &[&CommandBuffer],
        signal_fence: Option<(&mut Fence, crate::FenceValue)>,
    ) -> Result<(), crate::DeviceError> {
        Ok(())
    }
    unsafe fn present(
        &mut self,
        surface: &mut Surface,
        texture: Texture,
    ) -> Result<(), crate::SurfaceError> {
        Ok(())
    }
}
