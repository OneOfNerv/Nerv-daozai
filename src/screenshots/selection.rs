//! 桌面像素快照与原生拖框窗口。选区始终以物理像素为单位。

use windows::core::w;
use windows::Win32::Foundation::{
    COLORREF, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::{Dwm::DwmFlush, Gdi::*};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::*;

pub(super) fn capture() -> Result<Option<image::RgbaImage>, String> {
    unsafe {
        // 截图线程独立于主窗口的 UI 缩放。
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        // 窗口鼠标消息的两个坐标各占一个有符号 16 位整数。
        if !(1..=i16::MAX as i32).contains(&width) || !(1..=i16::MAX as i32).contains(&height) {
            return Err("桌面尺寸超出截图范围".into());
        }

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // 在遮罩窗口出现前冻结桌面，预览和最终图片使用同一份像素。
        let _ = DwmFlush();
        let original = snapshot(x, y, width, height, &info)?;
        let mut dimmed = original.clone();
        for pixel in dimmed.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                *channel /= 2;
            }
        }
        let mut cursor = POINT { x, y };
        let _ = GetPhysicalCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut display = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mut hint = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        if GetMonitorInfoW(monitor, &mut display).as_bool() {
            hint.left = display.rcMonitor.left - x;
            hint.right = display.rcMonitor.right - x;
            hint.top = display.rcMonitor.top - y;
        }
        let mut overlay = Overlay {
            info,
            x,
            y,
            width,
            height,
            original,
            dimmed,
            hint,
            editor: Editor::default(),
            result: None,
        };
        select(x, y, &mut overlay)?;
        Ok(overlay
            .result
            .map(|rect| crop(&overlay.original, width, rect)))
    }
}

unsafe fn snapshot(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    info: &BITMAPINFO,
) -> Result<Vec<u8>, String> {
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .filter(|&n| n <= isize::MAX as usize)
        .ok_or("桌面尺寸过大")?;
    let desktop = GetDC(None);
    if desktop.is_invalid() {
        return Err("无法读取桌面".into());
    }
    let result = (|| {
        let buffer = CreateCompatibleDC(desktop);
        if buffer.is_invalid() {
            return Err("无法创建截图缓冲区".into());
        }
        let result = (|| {
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(
                desktop,
                info,
                DIB_RGB_COLORS,
                &mut bits,
                HANDLE::default(),
                0,
            )
            .map_err(|error| error.to_string())?;
            let bitmap = HGDIOBJ(bitmap.0);
            let previous = SelectObject(buffer, bitmap);
            let result = if previous.is_invalid() || bits.is_null() {
                Err("无法准备桌面像素".into())
            } else {
                BitBlt(
                    buffer,
                    0,
                    0,
                    width,
                    height,
                    desktop,
                    x,
                    y,
                    SRCCOPY | CAPTUREBLT,
                )
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    if GdiFlush().as_bool() {
                        Ok(std::slice::from_raw_parts(bits.cast::<u8>(), bytes).to_vec())
                    } else {
                        Err("无法读取桌面像素".into())
                    }
                })
            };
            if !previous.is_invalid() {
                SelectObject(buffer, previous);
            }
            let _ = DeleteObject(bitmap);
            result
        })();
        let _ = DeleteDC(buffer);
        result
    })();
    ReleaseDC(None, desktop);
    result
}

struct Overlay {
    info: BITMAPINFO,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    original: Vec<u8>,
    dimmed: Vec<u8>,
    hint: RECT,
    editor: Editor,
    result: Option<RECT>,
}

impl Overlay {
    fn buttons(&self) -> Option<Buttons> {
        let rect = self
            .editor
            .selected
            .filter(|_| self.editor.drag.is_none())?;
        let monitor = unsafe {
            MonitorFromPoint(
                POINT {
                    x: self.x + rect.right - 1,
                    y: self.y + rect.bottom - 1,
                },
                MONITOR_DEFAULTTONEAREST,
            )
        };
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let bounds = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let screen = info.rcMonitor;
            RECT {
                left: screen.left - self.x,
                top: screen.top - self.y,
                right: screen.right - self.x,
                bottom: screen.bottom - self.y,
            }
        } else {
            RECT {
                left: 0,
                top: 0,
                right: self.width,
                bottom: self.height,
            }
        };
        Some(Buttons::place(rect, bounds))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Cancel,
    Confirm,
}

const BUTTON_WIDTH: i32 = 72;
const BUTTON_HEIGHT: i32 = 34;
const BUTTON_GAP: i32 = 6;

#[derive(Clone, Copy)]
struct Buttons {
    cancel: RECT,
    confirm: RECT,
}

impl Buttons {
    fn place(rect: RECT, bounds: RECT) -> Self {
        let width = BUTTON_WIDTH * 2 + BUTTON_GAP;
        let left = rect
            .right
            .saturating_sub(width)
            .clamp(bounds.left, (bounds.right - width).max(bounds.left));
        let below = rect.bottom + BUTTON_GAP;
        let above = rect.top - BUTTON_GAP - BUTTON_HEIGHT;
        let top = if below + BUTTON_HEIGHT <= bounds.bottom {
            below
        } else if above >= bounds.top {
            above
        } else {
            (rect.bottom - BUTTON_HEIGHT - BUTTON_GAP).min(bounds.bottom - BUTTON_HEIGHT)
        }
        .max(bounds.top);
        Self {
            cancel: RECT {
                left,
                top,
                right: left + BUTTON_WIDTH,
                bottom: top + BUTTON_HEIGHT,
            },
            confirm: RECT {
                left: left + BUTTON_WIDTH + BUTTON_GAP,
                top,
                right: left + width,
                bottom: top + BUTTON_HEIGHT,
            },
        }
    }

    fn hit(self, point: POINT) -> Option<Action> {
        if contains(self.confirm, point) {
            Some(Action::Confirm)
        } else if contains(self.cancel, point) {
            Some(Action::Cancel)
        } else {
            None
        }
    }
}

fn contains(rect: RECT, p: POINT) -> bool {
    (rect.left..rect.right).contains(&p.x) && (rect.top..rect.bottom).contains(&p.y)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Draw,
    Move(RECT),
    Resize(RECT, i32, i32),
}

#[derive(Clone, Copy)]
struct Drag {
    mode: Edit,
    start: POINT,
    current: POINT,
}

impl Drag {
    fn rect(self, width: i32, height: i32) -> Option<RECT> {
        let dx = self.current.x - self.start.x;
        let dy = self.current.y - self.start.y;
        match self.mode {
            Edit::Draw => selection(self.start, self.current, width, height),
            Edit::Move(mut rect) => {
                let dx = dx.clamp(-rect.left, width - rect.right);
                let dy = dy.clamp(-rect.top, height - rect.bottom);
                rect.left += dx;
                rect.right += dx;
                rect.top += dy;
                rect.bottom += dy;
                Some(rect)
            }
            Edit::Resize(mut rect, horizontal, vertical) => {
                match horizontal {
                    -1 => rect.left = (rect.left + dx).clamp(0, rect.right - 1),
                    1 => rect.right = (rect.right + dx).clamp(rect.left + 1, width),
                    _ => {}
                }
                match vertical {
                    -1 => rect.top = (rect.top + dy).clamp(0, rect.bottom - 1),
                    1 => rect.bottom = (rect.bottom + dy).clamp(rect.top + 1, height),
                    _ => {}
                }
                Some(rect)
            }
        }
    }
}

#[derive(Default)]
struct Editor {
    selected: Option<RECT>,
    drag: Option<Drag>,
    pressed: Option<Action>,
}

impl Editor {
    fn edit_at(&self, point: POINT) -> Edit {
        const SLOP: i32 = 6;
        let Some(rect) = self.selected else {
            return Edit::Draw;
        };
        if !(rect.left - SLOP..=rect.right + SLOP).contains(&point.x)
            || !(rect.top - SLOP..=rect.bottom + SLOP).contains(&point.y)
        {
            return Edit::Draw;
        }
        let edge = |position: i32, low: i32, high: i32| {
            let near = (position - low).abs();
            let far = (position - high).abs();
            if near <= SLOP && near <= far {
                -1
            } else if far <= SLOP {
                1
            } else {
                0
            }
        };
        let horizontal = edge(point.x, rect.left, rect.right);
        let vertical = edge(point.y, rect.top, rect.bottom);
        if horizontal != 0 || vertical != 0 {
            Edit::Resize(rect, horizontal, vertical)
        } else {
            Edit::Move(rect)
        }
    }

    fn press(&mut self, point: POINT, buttons: Option<Buttons>) {
        self.pressed = buttons.and_then(|buttons| buttons.hit(point));
        if self.pressed.is_none() {
            self.drag = Some(Drag {
                mode: self.edit_at(point),
                start: point,
                current: point,
            });
        }
    }

    fn release(
        &mut self,
        point: POINT,
        buttons: Option<Buttons>,
        width: i32,
        height: i32,
    ) -> Option<Action> {
        if let Some(mut drag) = self.drag.take() {
            drag.current = point;
            if let Some(rect) = drag.rect(width, height) {
                self.selected = Some(rect);
            }
        }
        self.pressed
            .take()
            .filter(|action| buttons.and_then(|buttons| buttons.hit(point)) == Some(*action))
    }
}

unsafe fn select(x: i32, y: i32, overlay: &mut Overlay) -> Result<(), String> {
    let instance = HINSTANCE(GetModuleHandleW(None).map_err(|error| error.to_string())?.0);
    let class = w!("DaozaiScreenshotSelection");
    let cursor = LoadCursorW(None, IDC_CROSS).map_err(|error| error.to_string())?;
    if RegisterClassW(&WNDCLASSW {
        hInstance: instance,
        lpszClassName: class,
        hCursor: cursor,
        lpfnWndProc: Some(window_proc),
        ..Default::default()
    }) == 0
    {
        return Err(windows::core::Error::from_win32().to_string());
    }

    let result = (|| {
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            class,
            w!("岛仔截图选区"),
            WS_POPUP,
            x,
            y,
            overlay.width,
            overlay.height,
            None,
            None,
            instance,
            None,
        )
        .map_err(|error| error.to_string())?;
        // 消息循环退出前 overlay 一直有效，且只有当前线程访问。
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, overlay as *mut Overlay as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let mut message = MSG::default();
        loop {
            match GetMessageW(&mut message, None, 0, 0).0 {
                0 => break Ok(()),
                -1 => {
                    let error = windows::core::Error::from_win32();
                    let _ = DestroyWindow(hwnd);
                    break Err(error.to_string());
                }
                _ => {
                    DispatchMessageW(&message);
                }
            }
        }
    })();
    let _ = UnregisterClassW(class, instance);
    result
}

fn mouse_point(lp: LPARAM) -> POINT {
    POINT {
        x: lp.0 as i16 as i32,
        y: (lp.0 >> 16) as i16 as i32,
    }
}

fn selection(start: POINT, end: POINT, width: i32, height: i32) -> Option<RECT> {
    let rect = RECT {
        left: start.x.min(end.x).clamp(0, width),
        right: start.x.max(end.x).clamp(0, width),
        top: start.y.min(end.y).clamp(0, height),
        bottom: start.y.max(end.y).clamp(0, height),
    };
    (rect.left < rect.right && rect.top < rect.bottom).then_some(rect)
}

fn crop(bgra: &[u8], desktop_width: i32, rect: RECT) -> image::RgbaImage {
    let width = (rect.right - rect.left) as u32;
    let height = (rect.bottom - rect.top) as u32;
    image::RgbaImage::from_fn(width, height, |x, y| {
        let offset = ((rect.top as usize + y as usize) * desktop_width as usize
            + rect.left as usize
            + x as usize)
            * 4;
        image::Rgba([bgra[offset + 2], bgra[offset + 1], bgra[offset], 255])
    })
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    let data = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Overlay;
    if data.is_null() {
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    // DestroyWindow 会同步发出失焦和鼠标捕获变化消息。
    let close = || {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
    };
    match msg {
        WM_LBUTTONDOWN => {
            let buttons = (*data).buttons();
            (*data).editor.press(mouse_point(lp), buttons);
            SetCapture(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
        }
        WM_MOUSEMOVE => {
            if let Some(drag) = (*data).editor.drag.as_mut() {
                drag.current = mouse_point(lp);
                let _ = InvalidateRect(hwnd, None, false);
            }
        }
        WM_LBUTTONUP => {
            let buttons = (*data).buttons();
            let action =
                (*data)
                    .editor
                    .release(mouse_point(lp), buttons, (*data).width, (*data).height);
            let _ = ReleaseCapture();
            match action {
                Some(Action::Confirm) => {
                    (*data).result = (*data).editor.selected;
                    close();
                }
                Some(Action::Cancel) => close(),
                None => {
                    let _ = InvalidateRect(hwnd, None, false);
                }
            }
        }
        WM_SETCURSOR => {
            let mut point = POINT::default();
            if GetPhysicalCursorPos(&mut point).is_err() {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            point.x -= (*data).x;
            point.y -= (*data).y;
            let editor = &(*data).editor;
            let cursor = if (*data)
                .buttons()
                .and_then(|buttons| buttons.hit(point))
                .is_some()
            {
                IDC_HAND
            } else {
                match editor
                    .drag
                    .map_or_else(|| editor.edit_at(point), |drag| drag.mode)
                {
                    Edit::Draw => IDC_CROSS,
                    Edit::Move(_) => IDC_SIZEALL,
                    Edit::Resize(_, 0, _) => IDC_SIZENS,
                    Edit::Resize(_, _, 0) => IDC_SIZEWE,
                    Edit::Resize(_, h, v) if h == v => IDC_SIZENWSE,
                    Edit::Resize(..) => IDC_SIZENESW,
                }
            };
            if let Ok(cursor) = LoadCursorW(None, cursor) {
                SetCursor(cursor);
            }
            return LRESULT(1);
        }
        WM_PAINT => {
            let mut state = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut state);
            if !dc.is_invalid() {
                paint(&*data, dc);
            }
            let _ = EndPaint(hwnd, &state);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_KEYDOWN | WM_SYSKEYDOWN if wp.0 == VK_ESCAPE.0 as usize => close(),
        WM_KEYDOWN if wp.0 == VK_RETURN.0 as usize && (*data).editor.drag.is_none() => {
            if let Some(rect) = (*data).editor.selected {
                (*data).result = Some(rect);
                close();
            }
        }
        WM_RBUTTONUP | WM_CANCELMODE | WM_DISPLAYCHANGE | WM_CLOSE => close(),
        WM_ACTIVATE if wp.0 as u16 == WA_INACTIVE as u16 => close(),
        WM_CAPTURECHANGED => {
            (*data).editor.drag = None;
            (*data).editor.pressed = None;
            let _ = InvalidateRect(hwnd, None, false);
        }
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    LRESULT(0)
}

unsafe fn paint(overlay: &Overlay, dc: HDC) {
    let draw_bitmap = |pixels: &[u8]| {
        SetDIBitsToDevice(
            dc,
            0,
            0,
            overlay.width as u32,
            overlay.height as u32,
            0,
            0,
            0,
            overlay.height as u32,
            pixels.as_ptr().cast(),
            &overlay.info,
            DIB_RGB_COLORS,
        );
    };
    draw_bitmap(&overlay.dimmed);
    let edit = &overlay.editor;
    let preview = edit
        .drag
        .and_then(|drag| drag.rect(overlay.width, overlay.height));
    if let Some(rect) = preview.or(edit.selected) {
        let saved = SaveDC(dc);
        if saved != 0 {
            IntersectClipRect(dc, rect.left, rect.top, rect.right, rect.bottom);
            draw_bitmap(&overlay.original);
            let _ = RestoreDC(dc, saved);
        }
        FrameRect(dc, &rect, HBRUSH(GetStockObject(WHITE_BRUSH).0));
        if edit.drag.is_none() {
            let mid_x = (rect.left + rect.right) / 2;
            let mid_y = (rect.top + rect.bottom) / 2;
            let handle = HBRUSH(GetStockObject(WHITE_BRUSH).0);
            for (x, y) in [
                (rect.left, rect.top),
                (mid_x, rect.top),
                (rect.right, rect.top),
                (rect.left, mid_y),
                (rect.right, mid_y),
                (rect.left, rect.bottom),
                (mid_x, rect.bottom),
                (rect.right, rect.bottom),
            ] {
                FillRect(
                    dc,
                    &RECT {
                        left: x - 2,
                        top: y - 2,
                        right: x + 3,
                        bottom: y + 3,
                    },
                    handle,
                );
            }
        }
        if let Some(buttons) = overlay.buttons() {
            paint_button(dc, buttons.cancel, Action::Cancel, edit.pressed);
            paint_button(dc, buttons.confirm, Action::Confirm, edit.pressed);
        }
    } else if edit.drag.is_none() {
        let center = (overlay.hint.left + overlay.hint.right) / 2;
        let mut bounds = RECT {
            left: center - 142,
            right: center + 142,
            top: overlay.hint.top + 48,
            bottom: overlay.hint.top + 82,
        };
        FillRect(dc, &bounds, HBRUSH(GetStockObject(BLACK_BRUSH).0));
        SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
        SetTextColor(dc, COLORREF(0xffffff));
        SetBkMode(dc, TRANSPARENT);
        let mut text: Vec<u16> = "拖动选择截图区域  ·  Esc 取消".encode_utf16().collect();
        DrawTextW(
            dc,
            &mut text,
            &mut bounds,
            DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
        );
    }
}

unsafe fn paint_button(dc: HDC, rect: RECT, action: Action, pressed: Option<Action>) {
    // COLORREF 使用 0x00BBGGRR。
    let color = match (action, pressed == Some(action)) {
        (Action::Confirm, false) => 0x9e6928,
        (Action::Confirm, true) => 0x784e1d,
        (Action::Cancel, false) => 0x302b28,
        (Action::Cancel, true) => 0x473e39,
    };
    SetDCBrushColor(dc, COLORREF(color));
    FillRect(dc, &rect, HBRUSH(GetStockObject(DC_BRUSH).0));

    SelectObject(dc, GetStockObject(WHITE_PEN));
    let x = rect.left + 13;
    let y = (rect.top + rect.bottom) / 2;
    if action == Action::Confirm {
        let _ = MoveToEx(dc, x, y, None);
        let _ = LineTo(dc, x + 4, y + 5);
        let _ = LineTo(dc, x + 12, y - 5);
    } else {
        let _ = MoveToEx(dc, x + 1, y - 5, None);
        let _ = LineTo(dc, x + 11, y + 5);
        let _ = MoveToEx(dc, x + 11, y - 5, None);
        let _ = LineTo(dc, x + 1, y + 5);
    }

    let mut label: Vec<u16> = if action == Action::Confirm {
        "保存"
    } else {
        "取消"
    }
    .encode_utf16()
    .collect();
    let mut bounds = RECT {
        left: rect.left + 26,
        ..rect
    };
    SelectObject(dc, GetStockObject(DEFAULT_GUI_FONT));
    SetTextColor(dc, COLORREF(0xffffff));
    SetBkMode(dc, TRANSPARENT);
    DrawTextW(
        dc,
        &mut label,
        &mut bounds,
        DT_SINGLELINE | DT_CENTER | DT_VCENTER | DT_NOPREFIX,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screenshot_selection_handles_reverse_drag_edges_and_empty_click() {
        let start = POINT { x: 7, y: 6 };
        let end = POINT { x: -4, y: 2 };
        assert_eq!(
            selection(start, end, 5, 8),
            Some(RECT {
                left: 0,
                top: 2,
                right: 5,
                bottom: 6
            })
        );
        assert_eq!(selection(end, start, 5, 8), selection(start, end, 5, 8));
        assert_eq!(selection(start, start, 5, 8), None);
    }

    #[test]
    fn screenshot_selection_crops_original_pixels_without_the_overlay() {
        let bgra: Vec<u8> = (0..16).flat_map(|n| [n, n + 20, n + 40, 0]).collect();
        let selected = crop(
            &bgra,
            4,
            RECT {
                left: 1,
                top: 1,
                right: 3,
                bottom: 3,
            },
        );
        assert_eq!(selected.dimensions(), (2, 2));
        assert_eq!(selected.get_pixel(0, 0).0, [45, 25, 5, 255]);
        assert_eq!(selected.get_pixel(1, 1).0, [50, 30, 10, 255]);
    }

    #[test]
    fn screenshot_selection_stays_editable_until_confirmation() {
        let mut editor = Editor::default();
        let point = |x, y| POINT { x, y };
        let drag = |editor: &mut Editor, start, end| {
            editor.press(start, None);
            assert_eq!(editor.release(end, None, 400, 300), None);
        };
        drag(&mut editor, point(180, 140), point(80, 60));
        drag(&mut editor, point(130, 100), point(365, 280));
        assert_eq!(
            editor.selected,
            Some(RECT {
                left: 300,
                top: 220,
                right: 400,
                bottom: 300
            })
        );
        drag(&mut editor, point(300, 220), point(265, 192));
        drag(&mut editor, point(25, 25), point(25, 25));
        let rect = editor.selected.unwrap();
        assert_eq!(
            rect,
            RECT {
                left: 265,
                top: 192,
                right: 400,
                bottom: 300
            }
        );
        let buttons = Buttons::place(
            rect,
            RECT {
                left: 0,
                top: 0,
                right: 400,
                bottom: 300,
            },
        );
        assert!(buttons.confirm.bottom < rect.top && buttons.confirm.right <= 400);
        for (target, action) in [
            (buttons.cancel, Action::Cancel),
            (buttons.confirm, Action::Confirm),
        ] {
            let p = point(target.left + 1, target.top + 1);
            editor.press(p, Some(buttons));
            assert_eq!(editor.release(point(5, 5), Some(buttons), 400, 300), None);
            editor.press(p, Some(buttons));
            assert_eq!(editor.release(p, Some(buttons), 400, 300), Some(action));
        }
    }
}
