//! Резервный захват окна Windows через поверхность композитора по точному HWND.

use std::io;

#[cfg(windows)]
pub(super) use native::capture;

// GetWindowRect can include invisible resize borders. Preserve its origin for input and OCR.
fn frame_offset(full: [i64; 4], visible: [i64; 4], size: (u32, u32)) -> io::Result<(u32, u32)> {
    let dimensions = |rect: [i64; 4]| (rect[2] - rect[0], rect[3] - rect[1]);
    let content = (i64::from(size.0), i64::from(size.1));
    if content == dimensions(full) {
        return Ok((0, 0));
    }
    if content == dimensions(visible)
        && visible[0] >= full[0]
        && visible[1] >= full[1]
        && visible[2] <= full[2]
        && visible[3] <= full[3]
    {
        return Ok(((visible[0] - full[0]) as u32, (visible[1] - full[1]) as u32));
    }
    Err(io::Error::other("размер поверхности не совпадает с окном"))
}

#[cfg(windows)]
mod native {
    use std::ptr::from_mut;
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;

    use image::RgbaImage;
    use windows::Foundation::TypedEventHandler;
    use windows::Graphics::Capture::{
        Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
        GraphicsCaptureSession,
    };
    use windows::Graphics::DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat};
    use windows::Graphics::SizeInt32;
    use windows::Win32::Foundation::{ERROR_MOD_NOT_FOUND, HWND, RECT, RPC_E_CHANGED_MODE};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
    use windows::Win32::Graphics::Direct3D11::{
        self as d3d, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_PIN, GetModuleHandleExW,
    };
    use windows::Win32::System::WinRT::Direct3D11::{
        CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
    };
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    };
    use windows::core::{HRESULT, IInspectable, Interface, factory, w};

    use super::{frame_offset, io};
    use crate::screen::{Window, admin, win};

    struct WinRt(bool);

    impl WinRt {
        fn init() -> io::Result<Self> {
            match unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
                Ok(()) => Ok(Self(true)),
                Err(error) if error.code() == RPC_E_CHANGED_MODE => Ok(Self(false)),
                Err(error) => Err(io::Error::other(error)),
            }
        }
    }

    impl Drop for WinRt {
        fn drop(&mut self) {
            if self.0 {
                unsafe { RoUninitialize() };
            }
        }
    }

    #[derive(PartialEq, Eq)]
    struct Geometry {
        thread: u32,
        full: [i64; 4],
        visible: [i64; 4],
    }

    fn bounds(rect: RECT) -> [i64; 4] {
        [rect.left, rect.top, rect.right, rect.bottom].map(i64::from)
    }

    fn rect(window: HWND) -> io::Result<RECT> {
        let mut value = RECT::default();
        unsafe { GetWindowRect(window, &mut value) }.map_err(io::Error::other)?;
        Ok(value)
    }

    fn cloaked(window: HWND) -> io::Result<bool> {
        let mut flags = 0u32;
        unsafe {
            DwmGetWindowAttribute(
                window,
                DWMWA_CLOAKED,
                from_mut(&mut flags).cast(),
                size_of::<u32>() as u32,
            )
        }
        .map_err(io::Error::other)?;
        Ok(flags != 0)
    }

    fn geometry(hwnd: HWND, expected: &Window) -> io::Result<Geometry> {
        if !admin::desktop_available() {
            return Err(io::Error::other(
                "рабочий стол Windows заблокирован или защищён",
            ));
        }
        let mut pid = 0;
        let thread = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if thread == 0
            || i64::from(pid) != expected.pid
            || unsafe { !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() }
            || cloaked(hwnd)?
        {
            return Err(io::Error::other("выбранное окно исчезло или скрыто"));
        }
        let mut title = [0u16; 512];
        let title_len = unsafe { GetWindowTextW(hwnd, &mut title) };
        if title_len <= 0
            || String::from_utf16_lossy(&title[..title_len as usize]) != expected.title
        {
            return Err(io::Error::other("заголовок окна изменился"));
        }
        let (desktop_x, desktop_y, _, _) = win::desktop();
        let full = bounds(rect(hwnd)?);
        let [left, top, right, bottom] = full;
        let (width, height) = (right - left, bottom - top);
        if Some((
            left - i64::from(desktop_x),
            top - i64::from(desktop_y),
            width,
            height,
        )) != expected.rect
            || width <= 0
            || height <= 0
            || width.saturating_mul(height) > 40_000_000
        {
            return Err(io::Error::other("размер или положение окна изменились"));
        }
        let mut visible = RECT::default();
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                from_mut(&mut visible).cast(),
                size_of::<RECT>() as u32,
            )
        }
        .map_err(io::Error::other)?;
        let visible = bounds(visible);
        Ok(Geometry {
            thread,
            full,
            visible,
        })
    }

    fn device() -> io::Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
        let (mut device, mut context) = (None, None);
        let mut create = |driver| unsafe {
            d3d::D3D11CreateDevice(
                None,
                driver,
                Default::default(),
                d3d::D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                d3d::D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        create(D3D_DRIVER_TYPE_HARDWARE)
            .or_else(|_| create(D3D_DRIVER_TYPE_WARP))
            .map_err(io::Error::other)?;
        let device = device.ok_or_else(|| io::Error::other("недоступно устройство Direct3D"))?;
        let context = context.ok_or_else(|| io::Error::other("недоступен контекст Direct3D"))?;
        let dxgi: IDXGIDevice = device.cast().map_err(io::Error::other)?;
        let runtime = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|value| value.cast())
            .map_err(io::Error::other)?;
        Ok((device, context, runtime))
    }

    fn pixels(
        frame: &Direct3D11CaptureFrame,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> io::Result<RgbaImage> {
        let access = frame
            .Surface()
            .and_then(|surface| surface.cast::<IDirect3DDxgiInterfaceAccess>())
            .map_err(io::Error::other)?;
        let texture: ID3D11Texture2D =
            unsafe { access.GetInterface() }.map_err(io::Error::other)?;
        let mut desc = d3d::D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Width < width
            || desc.Height < height
            || u64::from(desc.Width) * u64::from(desc.Height) > 40_000_000
        {
            return Err(io::Error::other("неверный размер буфера окна"));
        }
        desc.Usage = d3d::D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = d3d::D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .map_err(io::Error::other)?;
        let staging = staging.ok_or_else(|| io::Error::other("не удалось создать буфер окна"))?;
        let mut map = d3d::D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopyResource(&staging, &texture);
            context
                .Map(&staging, 0, d3d::D3D11_MAP_READ, 0, Some(&mut map))
                .map_err(io::Error::other)?;
        }
        let row_bytes = width as usize * 4;
        let pitch = map.RowPitch as usize;
        let mut rgba = Vec::with_capacity(row_bytes * height as usize);
        if !map.pData.is_null() && pitch >= row_bytes {
            for row in 0..height as usize {
                let src = unsafe {
                    std::slice::from_raw_parts(map.pData.cast::<u8>().add(row * pitch), row_bytes)
                };
                for b in src.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&if b[3] == 0 {
                        [0; 4]
                    } else {
                        [b[2], b[1], b[0], b[3]]
                    });
                }
            }
        }
        unsafe { context.Unmap(&staging, 0) };
        RgbaImage::from_raw(width, height, rgba)
            .ok_or_else(|| io::Error::other("не удалось прочитать пиксели окна"))
    }

    fn frame_size(frame: &Direct3D11CaptureFrame, geometry: &Geometry) -> io::Result<(u32, u32)> {
        let content = frame.ContentSize().map_err(io::Error::other)?;
        let size = (content.Width.max(0) as u32, content.Height.max(0) as u32);
        if frame_offset(geometry.full, geometry.visible, size).is_err() {
            let [left, top, right, bottom] = geometry.full;
            let [x1, y1, x2, y2] = geometry.visible;
            return Err(io::Error::other(format!(
                "кадр {}×{} не совпал с окном {}×{} или видимой рамкой {}×{}",
                content.Width,
                content.Height,
                right - left,
                bottom - top,
                x2 - x1,
                y2 - y1
            )));
        }
        Ok(size)
    }

    fn surface(hwnd: HWND, geometry: &Geometry) -> io::Result<RgbaImage> {
        let _runtime = WinRt::init()?;
        if !GraphicsCaptureSession::IsSupported().map_err(io::Error::other)? {
            return Err(io::Error::other("захват окна через композитор недоступен"));
        }
        let interop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(io::Error::other)?;
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(hwnd) }.map_err(io::Error::other)?;
        // On Windows 10 a capture callback can outlive Close and access the unloaded DLL.
        let mut module = Default::default();
        if let Err(error) = unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_PIN,
                w!("GraphicsCapture.dll"),
                &mut module,
            )
        } && error.code() != HRESULT::from_win32(ERROR_MOD_NOT_FOUND.0)
        {
            return Err(io::Error::other(error));
        }
        let [left, top, right, bottom] = geometry.full;
        let size = SizeInt32 {
            Width: (right - left) as i32,
            Height: (bottom - top) as i32,
        };
        let (device, context, runtime) = device()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &runtime,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            1,
            size,
        )
        .map_err(io::Error::other)?;
        let result = (|| {
            let session = pool.CreateCaptureSession(&item).map_err(io::Error::other)?;
            let result = (|| {
                let (send, receive) = sync_channel(1);
                pool.FrameArrived(
                    &TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                        move |_, _| {
                            let _ = send.try_send(());
                            Ok(())
                        },
                    ),
                )
                .map_err(io::Error::other)?;
                let _ = session.SetIsCursorCaptureEnabled(false);
                session.StartCapture().map_err(io::Error::other)?;
                receive
                    .recv_timeout(Duration::from_millis(1500))
                    .map_err(|_| io::Error::other("Windows не вернула кадр выбранного окна"))?;
                let frame = pool.TryGetNextFrame().map_err(io::Error::other)?;
                // Pixels outside ContentSize are undefined, even when the texture is larger.
                let result = frame_size(&frame, geometry)
                    .and_then(|(width, height)| pixels(&frame, &device, &context, width, height));
                let _ = frame.Close();
                result
            })();
            let _ = session.Close();
            result
        })();
        let _ = pool.Close();
        result
    }

    pub(in crate::screen) fn capture(window: &Window) -> io::Result<RgbaImage> {
        let handle = isize::try_from(window.id).map_err(io::Error::other)?;
        let hwnd = HWND(handle as *mut _);
        let before = geometry(hwnd, window)?;
        let image = surface(hwnd, &before)
            .map_err(|e| io::Error::other(format!("захват окна Windows: {e}")))?;
        if geometry(hwnd, window)? != before {
            return Err(io::Error::other("окно изменилось во время снимка"));
        }
        let [left, top, right, bottom] = before.full;
        let (width, height) = ((right - left) as u32, (bottom - top) as u32);
        if image.dimensions() == (width, height) {
            return Ok(image);
        }
        let (x, y) = frame_offset(before.full, before.visible, image.dimensions())?;
        let mut output = RgbaImage::new(width, height);
        image::imageops::replace(&mut output, &image, i64::from(x), i64::from(y));
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::frame_offset;

    #[test]
    fn maps_exact_compositor_content_without_stretching_the_visible_border() {
        let full = [-308, 11, 308, 489];
        let frame = [-300, 11, 300, 481];
        assert_eq!(frame_offset(full, frame, (616, 478)).unwrap(), (0, 0));
        assert_eq!(frame_offset(full, frame, (600, 470)).unwrap(), (8, 0));
        assert!(frame_offset(full, frame, (600, 471)).is_err());
        assert!(frame_offset(full, [-310, 11, 290, 481], (600, 470)).is_err());
    }
}
