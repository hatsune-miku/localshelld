use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io,
    marker::PhantomData,
    mem::size_of,
    ptr::{null, null_mut},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};

use windows_sys::{
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::{
            CreateFontIndirectW, DEFAULT_GUI_FONT, DeleteObject, GetStockObject, HFONT,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{BST_CHECKED, EM_SETLIMITTEXT},
            HiDpi::*,
            Input::KeyboardAndMouse::SetFocus,
            WindowsAndMessaging::*,
        },
    },
    core::w,
};

const WAKE: u32 = WM_APP + 13;
const QUEUE_CAPACITY: usize = 64;
const MAX_WINDOWS: usize = 8;
const NOTE_LIMIT: usize = 4096;
const WINDOW_STYLE: u32 = WS_CAPTION
    | WS_SYSMENU
    | WS_THICKFRAME
    | WS_MINIMIZEBOX
    | WS_CLIPCHILDREN
    | DS_MODALFRAME as u32
    | DS_CENTER as u32;
const WINDOW_EX_STYLE: u32 = WS_EX_APPWINDOW | WS_EX_TOPMOST;
const SS_NOPREFIX_STYLE: u32 = 0x80;
const HEADER: i32 = 101;
const COUNTDOWN: i32 = 102;
const AGENT: i32 = 103;
const SCOPE: i32 = 104;
const REASON_LABEL: i32 = 105;
const REASON: i32 = 106;
const NOTE_LABEL: i32 = 107;
const NOTE: i32 = 108;
const REMEMBER: i32 = 109;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Accept,
    Deny,
    Ignore,
}

#[derive(Debug)]
pub struct Outcome {
    pub choice: Choice,
    pub remember: bool,
    pub note: String,
}

type Completion = Box<dyn FnOnce(Outcome) + Send>;

pub struct Request {
    pub id: String,
    pub agent: String,
    pub reason: String,
    pub scope: String,
    pub deadline: Instant,
    pub complete: Completion,
}

pub enum Event {
    Show(Request),
    Close(String),
    Stop(i32),
}

struct Mailbox {
    // Never dereferenced on worker threads. PostMessage is Windows' supported
    // cross-thread wake-up; the HWND carries no authorization decision/data.
    window: usize,
    queue: mpsc::SyncSender<Event>,
    stop: Option<i32>,
}

#[derive(Clone)]
pub struct Sender(Arc<Mutex<Option<Mailbox>>>);

impl Sender {
    pub fn send(&self, event: Event) -> io::Result<()> {
        let mut mailbox = self
            .0
            .lock()
            .map_err(|_| io::Error::other("UI mailbox poisoned"))?;
        let mailbox = mailbox
            .as_mut()
            .ok_or_else(|| io::Error::other("UI stopped"))?;
        if let Event::Stop(code) = event {
            mailbox.stop = Some(code);
        } else {
            mailbox
                .queue
                .try_send(event)
                .map_err(|_| io::Error::other("UI queue unavailable"))?;
        }
        // SAFETY: the mutex keeps the owning event loop from destroying or
        // recycling this HWND until PostMessage returns. No pointer is sent.
        if unsafe { PostMessageW(mailbox.window as HWND, WAKE, 0, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

pub struct EventLoop {
    dispatcher: HWND,
    receiver: mpsc::Receiver<Event>,
    sender: Sender,
    windows: HashMap<String, Window>,
    previous_dpi: DPI_AWARENESS_CONTEXT,
    // The UI loop and all HWND/GDI ownership must remain on their creator thread.
    _thread_bound: PhantomData<Rc<()>>,
}

impl EventLoop {
    pub fn new() -> io::Result<Self> {
        // SAFETY: changes only this UI thread's DPI context. The message-only
        // STATIC window is an OS class, with valid static UTF-16 strings.
        unsafe {
            let previous_dpi =
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let dispatcher = CreateWindowExW(
                0,
                w!("STATIC"),
                w!("localshelld event dispatcher"),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            if dispatcher.is_null() {
                let error = io::Error::last_os_error();
                if !previous_dpi.is_null() {
                    SetThreadDpiAwarenessContext(previous_dpi);
                }
                return Err(error);
            }
            let (queue, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
            let sender = Sender(Arc::new(Mutex::new(Some(Mailbox {
                window: dispatcher as usize,
                queue,
                stop: None,
            }))));
            Ok(Self {
                dispatcher,
                receiver,
                sender,
                windows: HashMap::new(),
                previous_dpi,
                _thread_bound: PhantomData,
            })
        }
    }

    pub fn sender(&self) -> Sender {
        self.sender.clone()
    }

    pub fn run(mut self) -> i32 {
        loop {
            // SAFETY: MSG is writable, and this is the owning thread's message
            // queue. IsDialogMessage consumes keyboard messages exactly once.
            let mut message = MSG::default();
            let status = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
            if status <= 0 {
                return if status == 0 {
                    message.wParam as i32
                } else {
                    1
                };
            }
            if let Some(code) =
                self.sender.0.lock().ok().and_then(|mut mailbox| {
                    mailbox.as_mut().and_then(|mailbox| mailbox.stop.take())
                })
            {
                return code;
            }
            while let Ok(event) = self.receiver.try_recv() {
                match event {
                    Event::Show(request) => {
                        self.windows.retain(|_, window| window.alive());
                        if request.deadline <= Instant::now() || self.windows.len() >= MAX_WINDOWS {
                            continue;
                        }
                        let id = request.id.clone();
                        // Same ID replacement first closes the old window. There
                        // can never be two live dialogs for one request.
                        self.windows.remove(&id);
                        match Window::new(request) {
                            Ok(window) => {
                                window.show();
                                self.windows.insert(id, window);
                            }
                            Err(error) => eprintln!("native consent window failed: {error}"),
                        }
                    }
                    Event::Close(id) => {
                        self.windows.remove(&id);
                    }
                    Event::Stop(code) => return code,
                }
            }
            if message.hwnd != self.dispatcher || message.message != WAKE {
                let handled = self.windows.values().any(|window| {
                    // SAFETY: Window owns the HWND on this thread. Windows can
                    // synchronously call dialog_proc, whose state is heap-stable.
                    window.alive() && unsafe { IsDialogMessageW(window.hwnd, &message) } != 0
                });
                if !handled {
                    unsafe {
                        TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                }
            }
            self.windows.retain(|_, window| window.alive());
        }
    }
}

impl Drop for EventLoop {
    fn drop(&mut self) {
        // Invalidate sender before HWND destruction to prevent stale-handle use.
        if let Ok(mut mailbox) = self.sender.0.lock() {
            mailbox.take();
        }
        self.windows.clear();
        // SAFETY: owning thread, and no worker can still post using dispatcher.
        unsafe {
            DestroyWindow(self.dispatcher);
            if !self.previous_dpi.is_null() {
                SetThreadDpiAwarenessContext(self.previous_dpi);
            }
        }
    }
}

// A DWORD-aligned DLGTEMPLATE followed by empty menu, class and title strings.
// Child controls are created in WM_INITDIALOG using Unicode Win32 classes.
#[repr(C, align(4))]
struct Template([u16; 12]);

impl Template {
    fn new() -> Self {
        Self([
            WINDOW_STYLE as u16,
            (WINDOW_STYLE >> 16) as u16,
            WINDOW_EX_STYLE as u16,
            (WINDOW_EX_STYLE >> 16) as u16,
            0,
            0,
            0,
            340,
            320,
            0,
            0,
            0,
        ])
    }
}

struct Window {
    hwnd: HWND,
    // Kept until AFTER DestroyWindow returns, including WM_NCDESTROY. A Box's
    // allocation remains stable when Window moves into/out of the map.
    _state: Box<WindowState>,
}

struct WindowState {
    agent: String,
    reason: String,
    scope: String,
    deadline: Instant,
    complete: RefCell<Option<Completion>>,
    dpi: Cell<u32>,
    initialized: Cell<bool>,
    failed: Cell<bool>,
    fonts: RefCell<Vec<HFONT>>,
}

impl Window {
    fn new(request: Request) -> io::Result<Self> {
        let state = Box::new(WindowState {
            agent: request.agent,
            reason: request.reason,
            scope: request.scope,
            deadline: request.deadline,
            complete: RefCell::new(Some(request.complete)),
            dpi: Cell::new(96),
            initialized: Cell::new(false),
            failed: Cell::new(false),
            fonts: RefCell::new(Vec::new()),
        });
        let template = Template::new();
        // SAFETY: template is DWORD-aligned and complete; Windows copies it
        // synchronously. state stays alive through creation and HWND destruction.
        let hwnd = unsafe {
            CreateDialogIndirectParamW(
                GetModuleHandleW(null()),
                template.0.as_ptr().cast(),
                null_mut(),
                Some(dialog_proc),
                (&*state as *const WindowState) as LPARAM,
            )
        };
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        let window = Self {
            hwnd,
            _state: state,
        };
        if window._state.failed.get() {
            return Err(io::Error::other("native controls could not be created"));
        }
        Ok(window)
    }

    fn show(&self) {
        // SAFETY: this window belongs to the current UI thread. The first
        // ShowWindow call can honor a launcher's hidden/minimized STARTUPINFO
        // instead of SW_SHOWNORMAL, so explicitly restore before presenting it.
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNORMAL);
            ShowWindow(self.hwnd, SW_RESTORE);
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            // Windows may decline foreground activation; do not bypass its rules.
            SetForegroundWindow(self.hwnd);
            SetFocus(GetDlgItem(self.hwnd, IDCANCEL));
        }
    }

    fn alive(&self) -> bool {
        // SAFETY: handle is never used after Window ownership ends.
        unsafe { IsWindow(self.hwnd) != 0 }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: same UI thread; state and font resources outlive child windows.
        if self.alive() {
            unsafe {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

impl Drop for WindowState {
    fn drop(&mut self) {
        // SAFETY: only owned CreateFontIndirectW fonts are stored here, and the
        // dialog and its children no longer exist when the state is dropped.
        for font in self.fonts.get_mut().drain(..) {
            unsafe {
                DeleteObject(font);
            }
        }
    }
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, message: u32, wp: WPARAM, lp: LPARAM) -> isize {
    // SAFETY: called by the Windows dialog manager on the UI thread. lp for
    // INITDIALOG is our stable Box pointer; Windows provides the documented
    // RECT/MINMAXINFO pointers for the messages handled below. No mutable state
    // borrow is held across a re-entrant SendMessage/SetWindowPos call.
    unsafe {
        if message == WM_INITDIALOG {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, lp);
            let state = &*(lp as *const WindowState);
            if initialize(hwnd, state).is_err() {
                state.failed.set(true);
            }
            return 0; // initialize explicitly sets keyboard focus.
        }
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowState;
        if pointer.is_null() {
            return 0;
        }
        let state = &*pointer;
        match message {
            WM_COMMAND => {
                let id = (wp & 0xffff) as i32;
                let notification = ((wp >> 16) & 0xffff) as u32;
                if notification == BN_CLICKED {
                    let choice = match id {
                        IDOK => Some(Choice::Accept),
                        IDNO => Some(Choice::Deny),
                        IDCANCEL => Some(Choice::Ignore),
                        _ => None,
                    };
                    if let Some(choice) = choice {
                        finish(hwnd, state, choice);
                        return 1;
                    }
                }
            }
            WM_CLOSE => {
                finish(hwnd, state, Choice::Ignore);
                return 1;
            }
            WM_TIMER => {
                if Instant::now() >= state.deadline {
                    finish(hwnd, state, Choice::Ignore);
                } else {
                    update_countdown(hwnd, state);
                }
                return 1;
            }
            WM_SIZE => {
                layout(hwnd, state);
                return 1;
            }
            WM_GETMINMAXINFO => {
                let bounds = &mut *(lp as *mut MINMAXINFO);
                let size = outer_size(500, 560, state.dpi.get());
                bounds.ptMinTrackSize.x = size.0;
                bounds.ptMinTrackSize.y = size.1;
                return 1;
            }
            WM_DPICHANGED => {
                state.dpi.set((wp & 0xffff) as u32);
                set_fonts(hwnd, state);
                let rect = &*(lp as *const RECT);
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                layout(hwnd, state);
                return 1;
            }
            WM_SETTINGCHANGE => {
                set_fonts(hwnd, state);
                layout(hwnd, state);
            }
            WM_NCDESTROY => {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            _ => {}
        }
        0 // Let DefDlgProc provide standard dialog behavior and system painting.
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn multiline(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r\n")
}

fn child(
    hwnd: HWND,
    class: *const u16,
    text: &str,
    id: i32,
    style: u32,
    ex: u32,
) -> io::Result<()> {
    // SAFETY: parent HWND belongs to this thread, class is a static OS class
    // name, and CreateWindow copies the NUL-terminated UTF-16 text synchronously.
    let control = unsafe {
        CreateWindowExW(
            ex,
            class,
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            1,
            1,
            hwnd,
            id as usize as _,
            GetModuleHandleW(null()),
            null(),
        )
    };
    if control.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn initialize(hwnd: HWND, state: &WindowState) -> io::Result<()> {
    // STATIC text uses SS_NOPREFIX so agent-supplied '&' is displayed literally.
    child(
        hwnd,
        w!("STATIC"),
        "本机命令授权",
        HEADER,
        SS_NOPREFIX_STYLE,
        0,
    )?;
    child(hwnd, w!("STATIC"), "", COUNTDOWN, SS_NOPREFIX_STYLE | 2, 0)?;
    child(
        hwnd,
        w!("STATIC"),
        &state.agent,
        AGENT,
        SS_NOPREFIX_STYLE,
        0,
    )?;
    child(
        hwnd,
        w!("STATIC"),
        &format!("授权范围：{}", state.scope),
        SCOPE,
        SS_NOPREFIX_STYLE,
        0,
    )?;
    child(hwnd, w!("STATIC"), "请求缘由(&R)：", REASON_LABEL, 0, 0)?;
    let edit_style = WS_TABSTOP | WS_VSCROLL | ES_MULTILINE as u32 | ES_AUTOVSCROLL as u32;
    child(
        hwnd,
        w!("EDIT"),
        &multiline(&state.reason),
        REASON,
        edit_style | ES_READONLY as u32,
        WS_EX_CLIENTEDGE,
    )?;
    child(hwnd, w!("STATIC"), "附加说明(&N)：", NOTE_LABEL, 0, 0)?;
    child(
        hwnd,
        w!("EDIT"),
        "",
        NOTE,
        edit_style | ES_WANTRETURN as u32,
        WS_EX_CLIENTEDGE,
    )?;
    let agent = state.agent.replace('&', "&&");
    child(
        hwnd,
        w!("BUTTON"),
        &format!("记住我的选择，后续来自 {agent} 的请求按本次选择处理(&M)"),
        REMEMBER,
        WS_TABSTOP | BS_AUTOCHECKBOX as u32 | BS_MULTILINE as u32,
        0,
    )?;
    child(
        hwnd,
        w!("BUTTON"),
        "忽略(&I)",
        IDCANCEL,
        WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
        0,
    )?;
    child(
        hwnd,
        w!("BUTTON"),
        "拒绝(&D)",
        IDNO,
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        0,
    )?;
    child(
        hwnd,
        w!("BUTTON"),
        "接受(&A)",
        IDOK,
        WS_TABSTOP | BS_PUSHBUTTON as u32,
        0,
    )?;
    // SAFETY: all controls exist. Messages use integer values, not borrowed
    // pointers. The UI timer only updates text; the broker owns authorization.
    unsafe {
        SetWindowTextW(hwnd, w!("本机命令授权 — localshelld"));
        SendMessageW(GetDlgItem(hwnd, NOTE), EM_SETLIMITTEXT, NOTE_LIMIT, 0);
        SendMessageW(hwnd, DM_SETDEFID, IDCANCEL as usize, 0);
        state.dpi.set(GetDpiForWindow(hwnd).max(96));
        set_fonts(hwnd, state);
        let (width, height) = outer_size(640, 620, state.dpi.get());
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        state.initialized.set(true);
        layout(hwnd, state);
        update_countdown(hwnd, state);
        if SetTimer(hwnd, 1, 200, None) == 0 {
            return Err(io::Error::last_os_error());
        }
        SetFocus(GetDlgItem(hwnd, IDCANCEL));
    }
    Ok(())
}

fn outer_size(width: i32, height: i32, dpi: u32) -> (i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: scale(width, dpi),
        bottom: scale(height, dpi),
    };
    // SAFETY: writable RECT; styles match the dialog created above.
    unsafe {
        AdjustWindowRectExForDpi(&mut rect, WINDOW_STYLE, 0, WINDOW_EX_STYLE, dpi);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

fn scale(value: i32, dpi: u32) -> i32 {
    ((i64::from(value) * i64::from(dpi) + 48) / 96) as i32
}

fn layout(hwnd: HWND, state: &WindowState) {
    if !state.initialized.get() {
        return;
    }
    // SAFETY: controls are children of this HWND and only moved on their UI
    // thread. No pointers to Rust allocations are passed to child messages.
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect);
        let dpi = state.dpi.get();
        let width = rect.right * 96 / dpi as i32;
        let height = rect.bottom * 96 / dpi as i32;
        let move_control = |id, x, y, w, h| {
            SetWindowPos(
                GetDlgItem(hwnd, id),
                null_mut(),
                scale(x, dpi),
                scale(y, dpi),
                scale(w, dpi),
                scale(h, dpi),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        };
        let content = width - 40;
        move_control(HEADER, 20, 18, content - 100, 28);
        move_control(COUNTDOWN, width - 112, 22, 92, 24);
        move_control(AGENT, 20, 54, content, 52);
        move_control(SCOPE, 20, 111, content, 38);
        move_control(REASON_LABEL, 20, 155, content, 22);
        let bottom = height - 20;
        let note_y = height - 240;
        move_control(REASON, 20, 181, content, (note_y - 218).max(80));
        move_control(NOTE_LABEL, 20, note_y - 27, content, 22);
        move_control(NOTE, 20, note_y, content, 72);
        move_control(REMEMBER, 20, note_y + 86, content, 86);
        move_control(IDCANCEL, width - 324, bottom - 36, 92, 34);
        move_control(IDNO, width - 222, bottom - 36, 92, 34);
        move_control(IDOK, width - 120, bottom - 36, 100, 34);
    }
}

fn set_fonts(hwnd: HWND, state: &WindowState) {
    // SAFETY: NONCLIENTMETRICS has its correct size; fonts are owned until all
    // controls have switched to the replacement. The stock fallback is borrowed
    // and never deleted. Respect system ClearType/font-smoothing preferences.
    unsafe {
        let mut metrics = NONCLIENTMETRICSW {
            cbSize: size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let mut fonts = Vec::new();
        let mut normal = GetStockObject(DEFAULT_GUI_FONT) as HFONT;
        let mut heading = normal;
        if SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            metrics.cbSize,
            (&mut metrics as *mut NONCLIENTMETRICSW).cast(),
            0,
            state.dpi.get(),
        ) != 0
        {
            let font = CreateFontIndirectW(&metrics.lfMessageFont);
            if !font.is_null() {
                normal = font;
                fonts.push(font);
            }
            metrics.lfMessageFont.lfWeight = 700;
            let font = CreateFontIndirectW(&metrics.lfMessageFont);
            if !font.is_null() {
                heading = font;
                fonts.push(font);
            }
        }
        for id in [
            HEADER,
            COUNTDOWN,
            AGENT,
            SCOPE,
            REASON_LABEL,
            REASON,
            NOTE_LABEL,
            NOTE,
            REMEMBER,
            IDCANCEL,
            IDNO,
            IDOK,
        ] {
            let font = if id == HEADER { heading } else { normal };
            SendMessageW(GetDlgItem(hwnd, id), WM_SETFONT, font as usize, 1);
        }
        let previous = state.fonts.replace(fonts);
        for font in previous {
            DeleteObject(font);
        }
    }
}

fn update_countdown(hwnd: HWND, state: &WindowState) {
    let remaining = state
        .deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .div_ceil(1000);
    // SAFETY: valid child HWND and temporary string is copied synchronously.
    unsafe {
        SetWindowTextW(
            GetDlgItem(hwnd, COUNTDOWN),
            wide(&format!("{remaining} 秒")).as_ptr(),
        );
    }
}

fn read_note(hwnd: HWND) -> String {
    let mut buffer = vec![0u16; NOTE_LIMIT + 1];
    // SAFETY: buffer is writable for the declared count and UTF-16 is decoded
    // lossily if a user somehow leaves an incomplete surrogate in the edit box.
    let length = unsafe {
        GetWindowTextW(
            GetDlgItem(hwnd, NOTE),
            buffer.as_mut_ptr(),
            buffer.len() as i32,
        )
    };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn finish(hwnd: HWND, state: &WindowState, choice: Choice) {
    // Deadline is monotonic. A queued late click cannot grant or deny access.
    let choice = if Instant::now() >= state.deadline {
        Choice::Ignore
    } else {
        choice
    };
    let complete = state.complete.borrow_mut().take();
    let Some(complete) = complete else {
        return;
    };
    let outcome = Outcome {
        choice,
        remember: choice != Choice::Ignore
            && unsafe {
                SendMessageW(GetDlgItem(hwnd, REMEMBER), BM_GETCHECK, 0, 0) == BST_CHECKED as isize
            },
        note: read_note(hwnd),
    };
    // SAFETY: the Window owner retains state through DestroyWindow's reentrant
    // notifications. Callback runs after teardown, without any RefCell borrow.
    unsafe {
        DestroyWindow(hwnd);
    }
    complete(outcome);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture(deadline: Instant) -> (Window, mpsc::Receiver<Outcome>) {
        let (send, receive) = mpsc::channel();
        let window = Window::new(Request {
            id: "test".into(),
            agent: "测试 & Agent 😀".into(),
            reason: "原样 <script> & 中文\n第二行".repeat(500),
            scope: "无（只测试界面，不授予权限）".into(),
            deadline,
            complete: Box::new(move |outcome| {
                let _ = send.send(outcome);
            }),
        })
        .unwrap();
        (window, receive)
    }

    fn text(hwnd: HWND) -> String {
        // SAFETY: test-owned window/control, sized buffer including terminator.
        unsafe {
            let mut value = vec![0; GetWindowTextLengthW(hwnd) as usize + 1];
            let length = GetWindowTextW(hwnd, value.as_mut_ptr(), value.len() as i32);
            String::from_utf16_lossy(&value[..length as usize])
        }
    }

    #[test]
    fn unicode_and_line_breaks_are_preserved() {
        let text = "中文 & <script> 😀\nsecond\r\nthird";
        let encoded = wide(text);
        assert_eq!(encoded.last(), Some(&0));
        assert_eq!(
            String::from_utf16(&encoded[..encoded.len() - 1]).unwrap(),
            text
        );
        assert_eq!(multiline(text), "中文 & <script> 😀\r\nsecond\r\nthird");
    }

    #[test]
    fn layout_uses_logical_pixels_and_aligned_dialog_template() {
        assert_eq!(scale(640, 96), 640);
        assert_eq!(scale(640, 144), 960);
        assert_eq!(scale(640, 192), 1280);
        assert_eq!(std::mem::align_of::<Template>(), 4);
        assert_eq!(size_of::<Template>(), 24);
    }

    #[test]
    fn controls_are_real_unicode_win32_controls_with_dialog_defaults() {
        let (window, _) = fixture(Instant::now() + Duration::from_secs(60));
        unsafe {
            for (id, expected) in [
                (REASON, "Edit"),
                (NOTE, "Edit"),
                (REMEMBER, "Button"),
                (IDOK, "Button"),
            ] {
                let mut class = [0u16; 64];
                let count = GetClassNameW(
                    GetDlgItem(window.hwnd, id),
                    class.as_mut_ptr(),
                    class.len() as i32,
                );
                assert_eq!(String::from_utf16_lossy(&class[..count as usize]), expected);
            }
            assert_eq!(
                text(GetDlgItem(window.hwnd, REASON)),
                multiline(&window._state.reason)
            );
            assert_eq!(text(GetDlgItem(window.hwnd, AGENT)), "测试 & Agent 😀");
            assert_eq!(
                SendMessageW(window.hwnd, DM_GETDEFID, 0, 0) & 0xffff,
                IDCANCEL as isize
            );
            assert!(
                GetWindowLongPtrW(GetDlgItem(window.hwnd, NOTE), GWL_STYLE) as u32
                    & ES_WANTRETURN as u32
                    != 0
            );
            assert_eq!(
                SendMessageW(
                    GetDlgItem(window.hwnd, NOTE),
                    windows_sys::Win32::UI::Controls::EM_GETLIMITTEXT,
                    0,
                    0
                ),
                NOTE_LIMIT as isize
            );
            assert_ne!(
                SendMessageW(GetDlgItem(window.hwnd, NOTE), WM_GETFONT, 0, 0),
                0
            );
        }
    }

    #[test]
    fn consent_is_topmost_and_restored_even_after_hidden_startup() {
        const MODE: &str = "LOCALSHELLD_DIALOG_SHOW_TEST";
        if std::env::var_os(MODE).is_none() {
            use std::{os::windows::process::CommandExt, process::Command};
            let directory = tempfile::tempdir().unwrap();
            let stdout = directory.path().join("stdout.log");
            let stderr = directory.path().join("stderr.log");
            let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join("System32/WindowsPowerShell/v1.0/powershell.exe");
            let output = Command::new(powershell)
                .env_remove("PSModulePath")
                .args(["-NoProfile", "-NonInteractive", "-Command", r#"
$ErrorActionPreference='Stop'
$child=Start-Process -FilePath $env:LOCALSHELLD_SHOW_TEST_EXE -ArgumentList @('--exact','windows::tests::consent_is_topmost_and_restored_even_after_hidden_startup','--nocapture') -WindowStyle Hidden -Wait -PassThru -RedirectStandardOutput $env:LOCALSHELLD_SHOW_TEST_STDOUT -RedirectStandardError $env:LOCALSHELLD_SHOW_TEST_STDERR
exit $child.ExitCode
"#])
                .env(MODE, "child")
                .env("LOCALSHELLD_SHOW_TEST_EXE", std::env::current_exe().unwrap())
                .env("LOCALSHELLD_SHOW_TEST_STDOUT", &stdout)
                .env("LOCALSHELLD_SHOW_TEST_STDERR", &stderr)
                .creation_flags(0x08000000)
                .output()
                .unwrap();
            let text = std::fs::read_to_string(stdout).unwrap_or_default();
            let errors = std::fs::read_to_string(stderr).unwrap_or_default();
            assert!(output.status.success(), "{output:?}\n{text}\n{errors}");
            assert!(text.contains("topmost and restored"), "{text}");
            return;
        }
        let (window, outcome) = fixture(Instant::now() + Duration::from_secs(60));
        for minimize in [false, true] {
            if minimize {
                unsafe {
                    ShowWindow(window.hwnd, SW_MINIMIZE);
                    assert_ne!(IsIconic(window.hwnd), 0);
                }
            }
            window.show();
            unsafe {
                assert_ne!(IsWindowVisible(window.hwnd), 0);
                assert_eq!(IsIconic(window.hwnd), 0);
                assert_ne!(
                    GetWindowLongPtrW(window.hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST,
                    0
                );
                assert_eq!(
                    SendMessageW(window.hwnd, DM_GETDEFID, 0, 0) & 0xffff,
                    IDCANCEL as isize
                );
            }
            assert!(matches!(outcome.try_recv(), Err(mpsc::TryRecvError::Empty)));
        }
        println!("topmost and restored");
    }

    #[test]
    fn accept_and_deny_are_single_use_and_preserve_note_and_checkbox() {
        for (id, choice) in [(IDOK, Choice::Accept), (IDNO, Choice::Deny)] {
            let (window, outcome) = fixture(Instant::now() + Duration::from_secs(60));
            unsafe {
                SetWindowTextW(
                    GetDlgItem(window.hwnd, NOTE),
                    wide("中文备注 😀\r\nsecond line").as_ptr(),
                );
                SendMessageW(
                    GetDlgItem(window.hwnd, REMEMBER),
                    BM_SETCHECK,
                    BST_CHECKED as usize,
                    0,
                );
                SendMessageW(
                    window.hwnd,
                    WM_COMMAND,
                    id as usize,
                    GetDlgItem(window.hwnd, id) as isize,
                );
            }
            let outcome = outcome.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(outcome.choice, choice);
            assert!(outcome.remember);
            assert_eq!(outcome.note, "中文备注 😀\r\nsecond line");
            assert!(!window.alive());
            assert!(window._state.complete.borrow().is_none());
        }
    }

    #[test]
    fn ignore_escape_and_close_never_remember_but_keep_the_note() {
        for message in [WM_CLOSE, WM_COMMAND] {
            let (window, outcome) = fixture(Instant::now() + Duration::from_secs(60));
            unsafe {
                SetWindowTextW(GetDlgItem(window.hwnd, NOTE), w!("稍后再说"));
                SendMessageW(
                    GetDlgItem(window.hwnd, REMEMBER),
                    BM_SETCHECK,
                    BST_CHECKED as usize,
                    0,
                );
                SendMessageW(window.hwnd, message, IDCANCEL as usize, 0);
            }
            let outcome = outcome.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(outcome.choice, Choice::Ignore);
            assert!(!outcome.remember);
            assert_eq!(outcome.note, "稍后再说");
        }
    }

    #[test]
    fn late_click_cannot_authorize_and_timeout_closes_the_dialog() {
        for message in [WM_COMMAND, WM_TIMER] {
            let (window, outcome) = fixture(Instant::now() - Duration::from_millis(1));
            unsafe {
                SendMessageW(window.hwnd, message, IDOK as usize, 0);
            }
            assert_eq!(
                outcome.recv_timeout(Duration::from_secs(1)).unwrap().choice,
                Choice::Ignore
            );
            assert!(!window.alive());
        }
    }

    #[test]
    fn cancellation_drops_reply_without_fabricating_a_choice() {
        let (window, outcome) = fixture(Instant::now() + Duration::from_secs(60));
        drop(window);
        assert!(matches!(
            outcome.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn dpi_change_resizes_controls_and_replaces_owned_fonts() {
        let (window, _) = fixture(Instant::now() + Duration::from_secs(60));
        unsafe {
            let rect = RECT {
                left: 10,
                top: 10,
                right: 1290,
                bottom: 1290,
            };
            SendMessageW(
                window.hwnd,
                WM_DPICHANGED,
                192 | (192 << 16),
                (&rect as *const RECT) as isize,
            );
            assert_eq!(window._state.dpi.get(), 192);
            let mut note = RECT::default();
            GetClientRect(GetDlgItem(window.hwnd, NOTE), &mut note);
            assert!(note.right > 900 && note.bottom > 100);
            assert_eq!(window._state.fonts.borrow().len(), 2);
        }
    }

    #[test]
    fn shutdown_bypasses_full_queue_and_invalidates_all_senders() {
        let desktop = EventLoop::new().unwrap();
        let sender = desktop.sender();
        for _ in 0..QUEUE_CAPACITY {
            sender.send(Event::Close("test".into())).unwrap();
        }
        assert!(sender.send(Event::Close("overflow".into())).is_err());
        sender.send(Event::Stop(7)).unwrap();
        assert_eq!(desktop.run(), 7);
        assert!(sender.send(Event::Stop(0)).is_err());
    }
}
