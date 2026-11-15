#![allow(unsafe_op_in_unsafe_fn)]
//! Native accessible controls, CPU GDI drawing, and a sleeping Win32 message loop.
use file_recovery::{
    Result,
    engine::{Confidence, Phase, Progress, RecoveryKind, ScanOptions, ScanState},
    i18n::{Catalog, LANGUAGES},
    settings::{Settings, Theme},
    source,
    updates::{self, AvailableUpdate},
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    mem::size_of,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::{Com::*, LibraryLoader::GetModuleHandleW},
        UI::{
            Controls::{Dialogs::*, *},
            HiDpi::*,
            Input::KeyboardAndMouse::*,
            Shell::*,
            WindowsAndMessaging::*,
        },
    },
    core::PCWSTR,
};

const WAKE: u32 = WM_APP + 1;
const BODY_PAINT: u32 = WM_APP + 2;
const BODY_SCROLL: u32 = WM_APP + 3;
const BODY_WHEEL: u32 = WM_APP + 4;
const ANIMATION: usize = 1;
const NAV_SOURCE: u16 = 100;
const NAV_RESULTS: u16 = 101;
const NAV_RESTORE: u16 = 102;
const SETTINGS: u16 = 103;
const TITLE: u16 = 110;
const HELP: u16 = 111;
const PATH: u16 = 200;
const BROWSE: u16 = 201;
const ADVANCED: u16 = 202;
const OFFSET_LABEL: u16 = 203;
const OFFSET: u16 = 204;
const ADV_HELP: u16 = 205;
const SAFETY: u16 = 206;
const SAFETY_HELP: u16 = 207;
const SUPPORTED: u16 = 208;
const SUPPORTED_HELP: u16 = 209;
const SCAN: u16 = 210;
const RESUME: u16 = 211;
const PATH_LABEL: u16 = 212;
const SEARCH_LABEL: u16 = 300;
const SEARCH: u16 = 301;
const FILTER_LABEL: u16 = 302;
const FILTER: u16 = 303;
const LIST: u16 = 304;
const COUNT: u16 = 305;
const SELECT_ALL: u16 = 306;
const CLEAR: u16 = 307;
const PREVIEW_TITLE: u16 = 308;
const PREVIEW_META: u16 = 309;
const PREVIEW: u16 = 310;
const PREVIEW_NOTE: u16 = 311;
const NEXT: u16 = 312;
const DEST_LABEL: u16 = 400;
const DEST: u16 = 401;
const DEST_BROWSE: u16 = 402;
const RESTORE: u16 = 403;
const UNCERTAIN: u16 = 404;
const SUMMARY: u16 = 405;
const LANGUAGE_LABEL: u16 = 500;
const LANGUAGE: u16 = 501;
const THEME_LABEL: u16 = 502;
const THEME: u16 = 503;
const MOTION: u16 = 504;
const STARTUP: u16 = 505;
const VERSION: u16 = 506;
const UPDATE_HELP: u16 = 507;
const UPDATE_STATUS: u16 = 508;
const CHECK: u16 = 509;
const INSTALL: u16 = 510;
const DIAG_LABEL: u16 = 511;
const DIAG: u16 = 512;
const LICENSES: u16 = 513;
const STATUS: u16 = 600;
const CANCEL: u16 = 601;

#[derive(Debug, PartialEq, Eq)]
enum NativeAction {
    Focus,
    ComboChange,
    Selection,
    Search,
    Click,
    Other,
}
fn native_action(id: u16, code: u16) -> NativeAction {
    // Native control families reuse integer notification values (notably value 1).
    if code == CBN_SELCHANGE as u16 && matches!(id, FILTER | LANGUAGE | THEME) {
        NativeAction::ComboChange
    } else if id == LIST && code == LBN_SELCHANGE as u16 {
        NativeAction::Selection
    } else if id == SEARCH && code == EN_CHANGE as u16 {
        NativeAction::Search
    } else if [
        BN_SETFOCUS as u16,
        CBN_SETFOCUS as u16,
        LBN_SETFOCUS as u16,
        EN_SETFOCUS as u16,
    ]
    .contains(&code)
    {
        NativeAction::Focus
    } else if code == BN_CLICKED as u16 {
        NativeAction::Click
    } else {
        NativeAction::Other
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Source,
    Results,
    Restore,
    Settings,
}
#[derive(Clone, Copy)]
struct Palette {
    bg: u32,
    surface: u32,
    text: u32,
    muted: u32,
    border: u32,
    accent: u32,
    tint: u32,
}
impl Palette {
    fn new(theme: Theme) -> Self {
        if theme == Theme::Dark {
            Self {
                bg: rgb(18, 29, 32),
                surface: rgb(28, 42, 46),
                text: rgb(234, 243, 242),
                muted: rgb(174, 194, 196),
                border: rgb(66, 87, 91),
                accent: rgb(88, 214, 192),
                tint: rgb(32, 62, 62),
            }
        } else {
            Self {
                bg: rgb(245, 248, 247),
                surface: rgb(255, 255, 255),
                text: rgb(23, 49, 52),
                muted: rgb(77, 103, 106),
                border: rgb(181, 199, 196),
                accent: rgb(5, 111, 102),
                tint: rgb(223, 241, 235),
            }
        }
    }
}

enum Event {
    ScanDone(Box<ScanState>, Option<String>, Option<String>),
    Failure(String),
    Preview(u64, usize, Result<String>),
    Restored(file_recovery::restore::RestoreSummary),
    Update(Result<Option<AvailableUpdate>>),
    Downloaded(Result<(PathBuf, String)>),
}
enum WorkProgress {
    Scan(Progress),
    Restore(u64, u64),
    Download(u64, u64),
}
#[derive(Default)]
struct Mailbox {
    progress: Option<WorkProgress>,
    events: VecDeque<Event>,
    posted: bool,
}
#[derive(Clone)]
struct Wake {
    hwnd: usize,
    mailbox: Arc<Mutex<Mailbox>>,
    alive: Arc<AtomicBool>,
}
impl Wake {
    fn publish(&self, event: Option<Event>, progress: Option<WorkProgress>) {
        if !self.alive.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut box_) = self.mailbox.lock() {
            if let Some(e) = event {
                box_.events.push_back(e);
            }
            if let Some(p) = progress {
                box_.progress = Some(p);
            }
            if !box_.posted {
                box_.posted = true;
                unsafe {
                    PostMessageW(self.hwnd as HWND, WAKE, 0, 0);
                }
            }
        }
    }
    fn event(&self, e: Event) {
        self.publish(Some(e), None);
    }
    fn progress(&self, p: WorkProgress) {
        self.publish(None, Some(p));
    }
}

struct Control {
    id: u16,
    hwnd: HWND,
}
struct Ui {
    hwnd: HWND,
    body: HWND,
    controls: Vec<Control>,
    settings: Settings,
    strings: Catalog,
    page: Page,
    palette: Palette,
    brush: HBRUSH,
    surface_brush: HBRUSH,
    tint_brush: HBRUSH,
    fonts: [HFONT; 4],
    dpi: u32,
    advanced: bool,
    layout_dirty: bool,
    results_dirty: bool,
    placed: Vec<u16>,
    scroll_y: i32,
    scroll_max: i32,
    visible_rows: Vec<usize>,
    selected: Vec<usize>,
    state: Option<ScanState>,
    source_locator: String,
    source_generation: u64,
    preview_index: Option<usize>,
    preview_text: String,
    preview_running: bool,
    pending_preview: Option<usize>,
    summary: Option<file_recovery::restore::RestoreSummary>,
    diagnostic: String,
    show_licenses: bool,
    status_key: &'static str,
    status_detail: String,
    progress: Option<WorkProgress>,
    busy: bool,
    closing: bool,
    cancel: Arc<AtomicBool>,
    wake: Wake,
    update: Option<AvailableUpdate>,
    update_key: &'static str,
    update_busy: bool,
    downloading: bool,
    animation: Option<Instant>,
    layouts: u64,
    paints: u64,
}
thread_local! { static STATE:RefCell<Option<Ui>>=const {RefCell::new(None)}; }

pub fn run() -> Result<()> {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        let settings = Settings::load(&Settings::directory().join("settings.json"));
        let diagnostic = settings.as_ref().err().cloned().unwrap_or_default();
        let settings = settings.unwrap_or_default();
        let class = wide("FileRecovery.NativeWorkspace");
        let instance = GetModuleHandleW(null());
        let icon = LoadIconW(instance, 1usize as PCWSTR);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hIcon: icon,
            hIconSm: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let body_class = wide("FileRecovery.ScrollBody");
        let body_wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(body_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: body_class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassExW(&body_wc) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let palette = Palette::new(settings.theme);
        let alive = Arc::new(AtomicBool::new(true));
        let wake = Wake {
            hwnd: 0,
            mailbox: Arc::new(Mutex::new(Mailbox::default())),
            alive,
        };
        let strings = Catalog::new(&settings.language);
        STATE.with(|s| {
            *s.borrow_mut() = Some(Ui {
                hwnd: null_mut(),
                body: null_mut(),
                controls: vec![],
                settings,
                strings,
                page: Page::Source,
                palette,
                brush: CreateSolidBrush(palette.bg),
                surface_brush: CreateSolidBrush(palette.surface),
                tint_brush: CreateSolidBrush(palette.tint),
                fonts: [null_mut(); 4],
                dpi: 96,
                advanced: false,
                layout_dirty: true,
                results_dirty: true,
                placed: vec![],
                scroll_y: 0,
                scroll_max: 0,
                visible_rows: vec![],
                selected: vec![],
                state: None,
                source_locator: String::new(),
                source_generation: 0,
                preview_index: None,
                preview_text: String::new(),
                preview_running: false,
                pending_preview: None,
                summary: None,
                diagnostic,
                show_licenses: false,
                status_key: "ready",
                status_detail: String::new(),
                progress: None,
                busy: false,
                closing: false,
                cancel: Arc::new(AtomicBool::new(false)),
                wake,
                update: None,
                update_key: "",
                update_busy: false,
                downloading: false,
                animation: None,
                layouts: 0,
                paints: 0,
            })
        });
        let dpi = GetDpiForSystem().max(96) as i32;
        let mut work: RECT = std::mem::zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work as *mut RECT).cast(), 0);
        let initial_width = (1180 * dpi / 96).min((work.right - work.left - 32).max(840));
        let initial_height = (920 * dpi / 96).min((work.bottom - work.top - 32).max(600));
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            class.as_ptr(),
            wide("File Recovery").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            initial_width,
            initial_height,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error().to_string());
        }
        ShowWindow(hwnd, SW_SHOW);
        STATE.with(|s| {
            if let Some(ui) = s.borrow_mut().as_mut() {
                ui.refresh();
                if ui.settings.startup_updates {
                    ui.check_updates();
                }
            }
        });
        let mut message: MSG = std::mem::zeroed();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result <= 0 {
                break;
            }
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        STATE.with(|s| {
            if let Some(ui) = s.borrow_mut().take() {
                ui.wake.alive.store(false, Ordering::Release);
                DeleteObject(ui.brush);
                DeleteObject(ui.surface_brush);
                DeleteObject(ui.tint_brush);
                for f in ui.fonts {
                    if !f.is_null() {
                        DeleteObject(f);
                    }
                }
            }
        });
        CoUninitialize();
        Ok(())
    }
}

pub fn fatal(error: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(error).as_ptr(),
            wide("File Recovery").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // Modal native dialogs can reenter this procedure. Never alias mutable UI state.
    STATE.with(|cell| {
        if let Ok(mut state) = cell.try_borrow_mut()
            && let Some(ui) = state.as_mut()
        {
            return ui.message(hwnd, msg, w, l);
        }
        DefWindowProcW(hwnd, msg, w, l)
    })
}

unsafe extern "system" fn body_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let parent = GetParent(hwnd);
    match msg {
        WM_COMMAND | WM_DRAWITEM | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX
        | WM_CTLCOLORBTN => SendMessageW(parent, msg, w, l),
        WM_VSCROLL => SendMessageW(parent, BODY_SCROLL, w, l),
        WM_MOUSEWHEEL => SendMessageW(parent, BODY_WHEEL, w, l),
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            SendMessageW(parent, BODY_PAINT, dc as usize, 0);
            EndPaint(hwnd, &ps);
            0
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

impl Ui {
    fn tr(&self, key: &str) -> String {
        self.strings.text(key).into()
    }
    fn control(&self, id: u16) -> HWND {
        self.controls
            .iter()
            .find(|c| c.id == id)
            .map_or(null_mut(), |c| c.hwnd)
    }
    unsafe fn text(&self, id: u16) -> String {
        let hwnd = self.control(id);
        let len = GetWindowTextLengthW(hwnd).clamp(0, 2 * 1024 * 1024) as usize;
        let mut data = vec![0; len + 1];
        GetWindowTextW(hwnd, data.as_mut_ptr(), data.len() as i32);
        String::from_utf16_lossy(&data[..len])
    }
    unsafe fn set(&self, id: u16, text: &str) {
        if self.text(id) != text {
            SetWindowTextW(self.control(id), wide(text).as_ptr());
        }
    }
    unsafe fn create(&mut self, id: u16, class: &str, style: u32) {
        let parent = if matches!(
            id,
            NAV_SOURCE | NAV_RESULTS | NAV_RESTORE | SETTINGS | STATUS | CANCEL
        ) {
            self.hwnd
        } else {
            self.body
        };
        let hwnd = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide("").as_ptr(),
            WS_CHILD | style,
            0,
            0,
            0,
            0,
            parent,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        self.controls.push(Control { id, hwnd });
    }
    unsafe fn initialize(&mut self) {
        self.body = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            wide("FileRecovery.ScrollBody").as_ptr(),
            wide("").as_ptr(),
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_VSCROLL,
            0,
            0,
            0,
            0,
            self.hwnd,
            50usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        self.dpi = GetDpiForWindow(self.hwnd).max(96);
        self.make_fonts();
        for id in [
            NAV_SOURCE,
            NAV_RESULTS,
            NAV_RESTORE,
            SETTINGS,
            BROWSE,
            ADVANCED,
            SCAN,
            RESUME,
            SELECT_ALL,
            CLEAR,
            NEXT,
            DEST_BROWSE,
            RESTORE,
            MOTION,
            STARTUP,
            CHECK,
            INSTALL,
            LICENSES,
            CANCEL,
        ] {
            self.create(
                id,
                "BUTTON",
                WS_TABSTOP | BS_OWNERDRAW as u32 | BS_NOTIFY as u32,
            );
        }
        for id in [
            TITLE,
            HELP,
            OFFSET_LABEL,
            ADV_HELP,
            SAFETY,
            SAFETY_HELP,
            SUPPORTED,
            SUPPORTED_HELP,
            PATH_LABEL,
            SEARCH_LABEL,
            FILTER_LABEL,
            COUNT,
            PREVIEW_TITLE,
            PREVIEW_META,
            PREVIEW_NOTE,
            DEST_LABEL,
            UNCERTAIN,
            LANGUAGE_LABEL,
            THEME_LABEL,
            VERSION,
            UPDATE_HELP,
            UPDATE_STATUS,
            DIAG_LABEL,
            STATUS,
        ] {
            self.create(id, "STATIC", 0);
        }
        for id in [PATH, OFFSET, SEARCH, DEST] {
            self.create(id, "EDIT", WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32);
            SendMessageW(self.control(id), EM_SETLIMITTEXT, 32760, 0);
        }
        for id in [PREVIEW, SUMMARY, DIAG] {
            self.create(
                id,
                "EDIT",
                WS_TABSTOP
                    | WS_BORDER
                    | WS_VSCROLL
                    | WS_HSCROLL
                    | ES_READONLY as u32
                    | ES_MULTILINE as u32
                    | ES_AUTOVSCROLL as u32,
            );
            SendMessageW(self.control(id), EM_SETLIMITTEXT, 2 * 1024 * 1024, 0);
        }
        self.create(
            LIST,
            "LISTBOX",
            WS_TABSTOP
                | WS_BORDER
                | WS_VSCROLL
                | WS_HSCROLL
                | LBS_EXTENDEDSEL as u32
                | LBS_NOTIFY as u32
                | LBS_NOINTEGRALHEIGHT as u32,
        );
        SendMessageW(self.control(LIST), LB_SETHORIZONTALEXTENT, 2500, 0);
        for id in [FILTER, LANGUAGE, THEME] {
            self.create(
                id,
                "COMBOBOX",
                WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            );
        }
        for c in &self.controls {
            SendMessageW(c.hwnd, WM_SETFONT, self.fonts[0] as usize, 0);
        }
        SendMessageW(self.control(TITLE), WM_SETFONT, self.fonts[2] as usize, 0);
        SendMessageW(self.control(PREVIEW), WM_SETFONT, self.fonts[3] as usize, 0);
        SendMessageW(self.control(DIAG), WM_SETFONT, self.fonts[3] as usize, 0);
        self.localize();
    }
    unsafe fn make_fonts(&mut self) {
        for f in self.fonts {
            if !f.is_null() {
                DeleteObject(f);
            }
        }
        for (i, (points, weight)) in [(16, 400), (20, 600), (28, 600), (13, 400)]
            .into_iter()
            .enumerate()
        {
            self.fonts[i] = CreateFontW(
                -(points * self.dpi as i32 / 96),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                DEFAULT_PITCH as u32,
                wide(if i == 3 { "Consolas" } else { "Segoe UI" }).as_ptr(),
            );
        }
    }
    unsafe fn combo(&self, id: u16, values: &[String], selected: usize) {
        SendMessageW(self.control(id), CB_RESETCONTENT, 0, 0);
        for text in values {
            SendMessageW(
                self.control(id),
                CB_ADDSTRING,
                0,
                wide(text).as_ptr() as isize,
            );
        }
        SendMessageW(self.control(id), CB_SETCURSEL, selected, 0);
    }
    unsafe fn localize(&mut self) {
        for (id, key) in [
            (NAV_SOURCE, "source"),
            (NAV_RESULTS, "results"),
            (NAV_RESTORE, "restore"),
            (SETTINGS, "settings"),
            (PATH_LABEL, "image"),
            (BROWSE, "browse"),
            (ADVANCED, "advanced"),
            (OFFSET_LABEL, "offset"),
            (ADV_HELP, "advanced_help"),
            (SAFETY, "safety_title"),
            (SAFETY_HELP, "safety_help"),
            (SUPPORTED, "supported"),
            (SUPPORTED_HELP, "supported_help"),
            (SCAN, "scan"),
            (RESUME, "resume"),
            (SEARCH_LABEL, "search"),
            (FILTER_LABEL, "filter"),
            (SELECT_ALL, "select_all"),
            (CLEAR, "clear"),
            (PREVIEW_TITLE, "preview"),
            (NEXT, "restore_selected"),
            (DEST_LABEL, "destination"),
            (DEST_BROWSE, "choose_folder"),
            (RESTORE, "start_restore"),
            (UNCERTAIN, "uncertain"),
            (LANGUAGE_LABEL, "language"),
            (THEME_LABEL, "appearance"),
            (UPDATE_HELP, "update_help"),
            (CHECK, "check"),
            (INSTALL, "install"),
            (DIAG_LABEL, "details"),
            (LICENSES, "licenses"),
            (CANCEL, "cancel"),
        ] {
            self.set(id, &self.tr(key));
        }
        let filter = SendMessageW(self.control(FILTER), CB_GETCURSEL, 0, 0).max(0) as usize;
        self.combo(
            FILTER,
            &[
                self.tr("all"),
                self.tr("fat"),
                self.tr("png"),
                self.tr("partial"),
            ],
            filter,
        );
        self.combo(
            LANGUAGE,
            &LANGUAGES
                .iter()
                .map(|(_, name)| name.to_string())
                .collect::<Vec<_>>(),
            LANGUAGES
                .iter()
                .position(|(code, _)| *code == self.settings.language)
                .unwrap_or(0),
        );
        self.combo(
            THEME,
            &[self.tr("light"), self.tr("dark")],
            usize::from(self.settings.theme == Theme::Dark),
        );
        self.set(
            VERSION,
            &format!("{}  {}", self.tr("version"), env!("CARGO_PKG_VERSION")),
        );
        self.layout_dirty = true;
        self.rebuild_results();
    }
    unsafe fn visible(&self) -> bool {
        IsWindowVisible(self.hwnd) != 0 && IsIconic(self.hwnd) == 0
    }
    unsafe fn place(&mut self, id: u16, x: i32, y: i32, w: i32, h: i32) {
        self.placed.push(id);
        let y = if GetParent(self.control(id)) == self.body {
            y - 144 - self.scroll_y
        } else {
            y
        };
        let s = |n: i32| n * self.dpi as i32 / 96;
        MoveWindow(self.control(id), s(x), s(y), s(w.max(1)), s(h.max(1)), 1);
        ShowWindow(self.control(id), SW_SHOWNA);
    }
    unsafe fn layout(&mut self) {
        if !self.visible() {
            return;
        }
        self.layouts += 1;
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(self.hwnd, &mut r);
        let w = r.right * 96 / self.dpi as i32;
        let actual_h = r.bottom * 96 / self.dpi as i32;
        let h = actual_h.max(860);
        self.placed.clear();
        self.scroll_max = h - actual_h;
        self.scroll_y = self.scroll_y.clamp(0, self.scroll_max);
        let s = |n: i32| n * self.dpi as i32 / 96;
        MoveWindow(
            self.body,
            0,
            s(144),
            r.right,
            s((actual_h - 220).max(100)),
            1,
        );
        let info = SCROLLINFO {
            cbSize: size_of::<SCROLLINFO>() as u32,
            fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
            nMin: 0,
            nMax: h - 221,
            nPage: (actual_h - 220).max(1) as u32,
            nPos: self.scroll_y,
            nTrackPos: 0,
        };
        SetScrollInfo(self.body, SB_VERT, &info, 1);
        for (id, x, width) in [
            (NAV_SOURCE, 24, 140),
            (NAV_RESULTS, 180, 170),
            (NAV_RESTORE, 366, 200),
            (SETTINGS, w - 218, 186),
        ] {
            self.place(id, x, 76, width, 40);
        }
        self.place(TITLE, 32, 144, w - 64, 44);
        self.place(HELP, 32, 194, w - 64, 56);
        match self.page {
            Page::Source => {
                self.place(PATH_LABEL, 32, 258, w - 64, 24);
                self.place(PATH, 32, 288, w - 266, 42);
                self.place(BROWSE, w - 218, 288, 186, 42);
                self.place(ADVANCED, 32, 348, 430, 38);
                if self.advanced {
                    self.place(OFFSET_LABEL, 32, 414, w - 64, 24);
                    self.place(OFFSET, 32, 446, 300, 42);
                    self.place(ADV_HELP, 32, 510, w - 64, 104);
                } else {
                    self.place(SAFETY, 48, 420, w - 96, 28);
                    self.place(SAFETY_HELP, 48, 460, w - 96, 68);
                    self.place(SUPPORTED, 32, 558, w - 64, 28);
                    self.place(SUPPORTED_HELP, 32, 598, w - 64, 90);
                }
                self.place(RESUME, 32, h - 122, 380, 46);
                self.place(SCAN, w - 302, h - 122, 270, 46);
            }
            Page::Results => {
                let left = (w - 96) * 53 / 100;
                let right = left + 64;
                let rw = w - right - 32;
                self.place(SEARCH_LABEL, 32, 258, left / 2 - 8, 24);
                self.place(FILTER_LABEL, 48 + left / 2, 258, left / 2 - 16, 24);
                self.place(SEARCH, 32, 288, left / 2 - 8, 40);
                self.place(FILTER, 48 + left / 2, 288, left / 2 - 16, 240);
                self.place(LIST, 32, 344, left, h - 560);
                self.place(COUNT, 32, h - 204, left, 32);
                self.place(SELECT_ALL, 32, h - 162, (left - 16) / 2, 36);
                self.place(CLEAR, 48 + (left - 16) / 2, h - 162, (left - 16) / 2, 36);
                self.place(RESUME, 32, h - 112, 370, 38);
                self.place(PREVIEW_TITLE, right, 258, rw, 28);
                self.place(PREVIEW_META, right, 298, rw, 80);
                self.place(PREVIEW_NOTE, right, 390, rw, 102);
                self.place(PREVIEW, right, 510, rw, h - 664);
                self.place(NEXT, w - 332, h - 112, 300, 46);
            }
            Page::Restore => {
                self.place(DEST_LABEL, 32, 284, w - 64, 28);
                self.place(DEST, 32, 322, w - 326, 42);
                self.place(DEST_BROWSE, w - 278, 322, 246, 42);
                self.place(UNCERTAIN, 32, 392, w - 64, 80);
                self.place(SUMMARY, 32, 494, w - 64, h - 666);
                self.place(RESTORE, w - 332, h - 112, 300, 46);
            }
            Page::Settings => {
                let x = w / 2 + 24;
                let left = w / 2 - 64;
                let right = w - x - 32;
                self.place(THEME_LABEL, 32, 266, left, 28);
                self.place(THEME, 32, 304, 300, 200);
                self.place(LANGUAGE_LABEL, 32, 370, left, 28);
                self.place(LANGUAGE, 32, 408, 330, 240);
                self.place(MOTION, 32, 474, left, 44);
                self.place(STARTUP, 32, 536, left, 64);
                self.place(UPDATE_HELP, 32, 626, left, 112);
                self.place(VERSION, x, 266, right, 28);
                self.place(CHECK, x, 308, right, 44);
                if self.update.is_some() {
                    self.place(INSTALL, x, 368, right, 44);
                }
                self.place(UPDATE_STATUS, x, 430, right, 68);
                self.place(DIAG_LABEL, x, 526, right - 210, 28);
                self.place(LICENSES, x + right - 204, 518, 204, 36);
                self.place(DIAG, x, 564, right, h - 682);
            }
        }
        self.place(STATUS, 32, actual_h - 56, w - 272, 44);
        if self.busy || self.downloading {
            self.place(CANCEL, w - 224, actual_h - 54, 192, 38);
        }
        let mut order = vec![NAV_SOURCE, NAV_RESULTS, NAV_RESTORE, SETTINGS, TITLE, HELP];
        order.extend(match self.page {
            Page::Source => vec![
                PATH_LABEL,
                PATH,
                BROWSE,
                ADVANCED,
                OFFSET_LABEL,
                OFFSET,
                ADV_HELP,
                SAFETY,
                SAFETY_HELP,
                SUPPORTED,
                SUPPORTED_HELP,
                RESUME,
                SCAN,
            ],
            Page::Results => vec![
                SEARCH_LABEL,
                SEARCH,
                FILTER_LABEL,
                FILTER,
                LIST,
                COUNT,
                SELECT_ALL,
                CLEAR,
                RESUME,
                PREVIEW_TITLE,
                PREVIEW_META,
                PREVIEW_NOTE,
                PREVIEW,
                NEXT,
            ],
            Page::Restore => vec![DEST_LABEL, DEST, DEST_BROWSE, UNCERTAIN, SUMMARY, RESTORE],
            Page::Settings => vec![
                THEME_LABEL,
                THEME,
                LANGUAGE_LABEL,
                LANGUAGE,
                MOTION,
                STARTUP,
                UPDATE_HELP,
                VERSION,
                CHECK,
                INSTALL,
                UPDATE_STATUS,
                DIAG_LABEL,
                LICENSES,
                DIAG,
            ],
        });
        for c in &self.controls {
            if !self.placed.contains(&c.id) {
                ShowWindow(c.hwnd, SW_HIDE);
            }
        }
        let body_order = order
            .into_iter()
            .map(|id| self.control(id))
            .filter(|control| GetParent(*control) == self.body)
            .collect::<Vec<_>>();
        let root_order = vec![
            self.control(NAV_SOURCE),
            self.control(NAV_RESULTS),
            self.control(NAV_RESTORE),
            self.control(SETTINGS),
            self.body,
            self.control(STATUS),
            self.control(CANCEL),
        ];
        for group in [body_order, root_order] {
            let mut previous = HWND_TOP;
            for control in group {
                SetWindowPos(
                    control,
                    previous,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                previous = control;
            }
        }
        InvalidateRect(self.body, null(), 0);
        self.layout_dirty = false;
    }
    unsafe fn refresh(&mut self) {
        if !self.visible() {
            self.layout_dirty = true;
            return;
        }
        if self.layout_dirty {
            self.layout();
        }
        if self.results_dirty {
            self.rebuild_results();
        }
        let (title, help) = match self.page {
            Page::Source => ("source_title", "source_help"),
            Page::Results => ("result_title", "result_help"),
            Page::Restore => ("restore_title", "restore_help"),
            Page::Settings => ("settings_title", "settings_help"),
        };
        self.set(TITLE, &self.tr(title));
        self.set(HELP, &self.tr(help));
        self.set(
            MOTION,
            &format!(
                "{}   ·   {}",
                self.tr("motion"),
                self.tr(if self.settings.reduced_motion {
                    "on"
                } else {
                    "off"
                })
            ),
        );
        self.set(
            STARTUP,
            &format!(
                "{}   ·   {}",
                self.tr("startup"),
                self.tr(if self.settings.startup_updates {
                    "on"
                } else {
                    "off"
                })
            ),
        );
        self.set(
            ADVANCED,
            &format!(
                "{}   {}",
                if self.advanced { "−" } else { "+" },
                self.tr("advanced")
            ),
        );
        self.set(
            COUNT,
            &format!(
                "{}: {}   ·   {}: {}",
                self.tr("files"),
                self.visible_rows.len(),
                self.tr("selected"),
                self.selected.len()
            ),
        );
        self.set(STATUS, &self.status_text());
        let update_text = if let Some(update) = &self.update {
            format!("{}  {}", self.tr("available"), update.version)
        } else {
            self.tr(self.update_key)
        };
        self.set(UPDATE_STATUS, &update_text);
        if self.page == Page::Settings {
            self.set(DIAG, &self.diagnostic_text());
        }
        if self.page == Page::Results {
            self.refresh_preview();
        }
        if self.page == Page::Restore {
            self.refresh_summary();
        }
        for id in [
            BROWSE,
            PATH,
            OFFSET,
            SCAN,
            RESUME,
            DEST,
            DEST_BROWSE,
            RESTORE,
            NEXT,
        ] {
            EnableWindow(self.control(id), i32::from(!self.busy && !self.downloading));
        }
        EnableWindow(self.control(NAV_SOURCE), i32::from(!self.busy));
        EnableWindow(
            self.control(NAV_RESULTS),
            i32::from(self.state.is_some() || self.busy),
        );
        EnableWindow(
            self.control(NAV_RESTORE),
            i32::from(!self.selected.is_empty() && !self.busy),
        );
        EnableWindow(
            self.control(NEXT),
            i32::from(!self.selected.is_empty() && !self.busy),
        );
        EnableWindow(
            self.control(RESTORE),
            i32::from(!self.selected.is_empty() && !self.busy),
        );
        EnableWindow(
            self.control(CHECK),
            i32::from(!self.update_busy && !self.downloading),
        );
        EnableWindow(
            self.control(INSTALL),
            i32::from(!self.downloading && !self.busy),
        );
        InvalidateRect(self.hwnd, null(), 0);
    }

    fn status_text(&self) -> String {
        match &self.progress {
            Some(WorkProgress::Scan(p)) => format!(
                "{}  ·  {}: {}  ·  {}: {} / {}",
                self.tr(if p.phase == Phase::Metadata {
                    "scan_metadata"
                } else {
                    "scan_signatures"
                }),
                self.tr("files"),
                p.found,
                self.tr(if p.phase == Phase::Metadata {
                    "directories"
                } else {
                    "scanned"
                }),
                if p.phase == Phase::Metadata {
                    p.directories
                } else {
                    p.bytes_scanned
                },
                if p.phase == Phase::Metadata {
                    String::from("—")
                } else {
                    p.total_bytes.to_string()
                }
            ),
            Some(WorkProgress::Restore(done, total)) => {
                format!("{}  ·  {} / {} B", self.tr("restoring"), done, total)
            }
            Some(WorkProgress::Download(done, total)) => {
                format!("{}  ·  {} / {} B", self.tr("downloading"), done, total)
            }
            None => format!(
                "{}{}",
                self.tr(self.status_key),
                if self.status_detail.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {}", self.status_detail)
                }
            ),
        }
    }
    fn diagnostic_text(&self) -> String {
        if self.show_licenses {
            return include_str!("../assets/third-party-notices.txt").into();
        }
        let mut text = self.diagnostic.clone();
        if let Some(state) = &self.state {
            for warning in &state.warnings {
                text.push_str("\r\n");
                text.push_str(warning);
            }
            text.push_str(&format!("\r\nFAT volumes: {}; checkpoint schema: 1; source: {} B\r\nIdentity uses length, modification time and 3 sparse SHA-256 samples, not a full-source hash.",state.volumes.len(),state.identity.length));
        }
        text.push_str(&format!("\r\nRenderer: native Win32 controls + CPU GDI.\r\nMain-window layouts: {}; paint callbacks: {}.\r\nIdle/hidden and accessibility validation pending.",self.layouts,self.paints));
        text
    }
    unsafe fn refresh_preview(&self) {
        if let Some(item) = self
            .preview_index
            .and_then(|i| self.state.as_ref().and_then(|s| s.results.get(i)))
        {
            let quality = match item.confidence {
                Confidence::Partial => "quality_partial",
                Confidence::Unverified => "quality_uncertain",
                Confidence::StructureVerified => "quality_png",
            };
            let kind = match item.kind {
                RecoveryKind::FatChain => "kind_chain",
                RecoveryKind::FatContiguous => "kind_contiguous",
                RecoveryKind::PngCarved => "kind_png",
            };
            self.set(
                PREVIEW_META,
                &format!(
                    "{}\r\n{}   ·   {}\r\n{}: {} / {}",
                    item.original_path,
                    self.tr(kind),
                    self.tr(quality),
                    self.tr("size"),
                    item.recovered_size,
                    item.expected_size
                ),
            );
            let note = if item.confidence == Confidence::Partial {
                "note_partial"
            } else {
                match item.kind {
                    RecoveryKind::FatChain => "note_chain",
                    RecoveryKind::FatContiguous => "note_contiguous",
                    RecoveryKind::PngCarved => "note_png",
                }
            };
            self.set(PREVIEW_NOTE, &self.tr(note));
            self.set(
                PREVIEW,
                if self.preview_text.is_empty() {
                    self.strings.text("no_preview")
                } else {
                    &self.preview_text
                },
            );
        } else {
            self.set(PREVIEW_META, "");
            self.set(
                PREVIEW_NOTE,
                &self.tr(
                    if self.state.as_ref().is_none_or(|s| s.results.is_empty()) {
                        "empty"
                    } else {
                        "preview_empty"
                    },
                ),
            );
            self.set(PREVIEW, "");
        }
    }
    unsafe fn refresh_summary(&self) {
        let text = if let Some(summary) = &self.summary {
            let mut text = format!(
                "{}: {}  ·  {} B  ·  {}: {}\r\n",
                self.tr("restored"),
                summary.files.len(),
                summary.bytes,
                self.tr("failed"),
                summary.failed.len()
            );
            for path in &summary.files {
                text.push_str(&format!("\r\n{}", path.display()));
            }
            if !summary.failed.is_empty() {
                text.push_str(&format!("\r\n\r\n{}", self.tr("details")));
                for e in &summary.failed {
                    text.push_str(&format!("\r\n{e}"));
                }
            }
            text
        } else {
            format!(
                "{}: {}\r\n\r\n{}",
                self.tr("selected"),
                self.selected.len(),
                self.tr("uncertain")
            )
        };
        self.set(SUMMARY, &text);
    }
    unsafe fn rebuild_results(&mut self) {
        if !self.visible() {
            self.results_dirty = true;
            return;
        }
        self.results_dirty = false;
        let search = self.text(SEARCH).to_lowercase();
        let filter = SendMessageW(self.control(FILTER), CB_GETCURSEL, 0, 0);
        SendMessageW(self.control(LIST), WM_SETREDRAW, 0, 0);
        SendMessageW(self.control(LIST), LB_RESETCONTENT, 0, 0);
        self.visible_rows.clear();
        if let Some(state) = &self.state {
            for (index, item) in state.results.iter().enumerate() {
                if !item.original_path.to_lowercase().contains(&search)
                    || (filter == 1 && item.kind == RecoveryKind::PngCarved)
                    || (filter == 2 && item.kind != RecoveryKind::PngCarved)
                    || (filter == 3 && item.confidence != Confidence::Partial)
                {
                    continue;
                }
                let quality = match item.confidence {
                    Confidence::Partial => "quality_partial",
                    Confidence::Unverified => "quality_uncertain",
                    Confidence::StructureVerified => "quality_png",
                };
                let row = format!(
                    "{}   |   {} B   |   {}",
                    item.original_path,
                    item.recovered_size,
                    self.tr(quality)
                );
                let row_index = self.visible_rows.len();
                SendMessageW(
                    self.control(LIST),
                    LB_ADDSTRING,
                    0,
                    wide(&row).as_ptr() as isize,
                );
                if self.selected.contains(&index) {
                    SendMessageW(self.control(LIST), LB_SETSEL, 1, row_index as isize);
                }
                self.visible_rows.push(index);
            }
        }
        SendMessageW(self.control(LIST), WM_SETREDRAW, 1, 0);
        InvalidateRect(self.control(LIST), null(), 1);
    }
    unsafe fn selection(&mut self) {
        self.selected
            .retain(|index| !self.visible_rows.contains(index));
        let count = SendMessageW(self.control(LIST), LB_GETSELCOUNT, 0, 0).max(0) as usize;
        let mut rows = vec![0i32; count.min(10_000)];
        let actual = SendMessageW(
            self.control(LIST),
            LB_GETSELITEMS,
            rows.len(),
            rows.as_mut_ptr() as isize,
        )
        .max(0) as usize;
        for row in rows.iter().take(actual) {
            if let Some(index) = self.visible_rows.get(*row as usize) {
                self.selected.push(*index);
            }
        }
        self.selected.sort_unstable();
        self.selected.dedup();
        let caret = SendMessageW(self.control(LIST), LB_GETCARETINDEX, 0, 0);
        let index = self.visible_rows.get(caret as usize).copied();
        if self.preview_index != index {
            self.preview_index = index;
            self.preview_text = self.tr("working");
            self.pending_preview = index;
            self.start_preview();
        }
        self.refresh();
    }
    fn start_preview(&mut self) {
        if self.preview_running {
            return;
        }
        let Some(index) = self.pending_preview.take() else {
            return;
        };
        let Some(state) = &self.state else {
            return;
        };
        let Some(item) = state.results.get(index).cloned() else {
            return;
        };
        let identity = state.identity.clone();
        let locator = self.source_locator.clone();
        let generation = self.source_generation;
        let wake = self.wake.clone();
        self.preview_running = true;
        std::thread::spawn(move || {
            let result = (|| {
                let mut source = source::open(&locator)?;
                if source.identity()? != identity {
                    return Err("Source identity changed".into());
                }
                file_recovery::preview::preview(source.as_mut(), &item)
            })();
            wake.event(Event::Preview(generation, index, result));
        });
    }
    unsafe fn page(&mut self, page: Page) {
        self.page = page;
        self.scroll_y = 0;
        self.layout_dirty = true;
        if !self.settings.reduced_motion && self.visible() {
            let mut enabled = 1i32;
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                (&mut enabled as *mut i32).cast(),
                0,
            );
            if enabled != 0 {
                self.animation = Some(Instant::now());
                SetTimer(self.hwnd, ANIMATION, 16, None);
            }
        }
        self.refresh();
        SetFocus(self.control(match page {
            Page::Source => PATH,
            Page::Results => LIST,
            Page::Restore => DEST,
            Page::Settings => THEME,
        }));
    }
    fn fail(&mut self, error: String) {
        self.status_key = "error";
        self.status_detail = self.tr("diagnostics");
        self.diagnostic = error;
    }
    fn save_settings(&mut self) {
        if let Err(e) = self
            .settings
            .save(&Settings::directory().join("settings.json"))
        {
            self.status_key = "save_failed";
            self.diagnostic = e;
        }
    }
    unsafe fn start_scan(&mut self, resume: bool) {
        if self.busy || self.downloading {
            return;
        }
        let locator = self.text(PATH).trim().to_string();
        if locator.is_empty() {
            self.status_key = "choose_source";
            self.refresh();
            return;
        }
        let offset = self.text(OFFSET);
        let offset = if self.advanced && !offset.trim().is_empty() {
            match offset.trim().parse::<u64>() {
                Ok(n) => Some(n),
                Err(_) => {
                    self.status_key = "invalid_offset";
                    self.refresh();
                    return;
                }
            }
        } else {
            None
        };
        self.source_locator = locator.clone();
        self.source_generation += 1;
        self.state = None;
        self.summary = None;
        self.selected.clear();
        self.visible_rows.clear();
        self.preview_index = None;
        self.preview_text.clear();
        self.pending_preview = None;
        self.busy = true;
        self.cancel = Arc::new(AtomicBool::new(false));
        self.status_key = "working";
        self.progress = None;
        self.diagnostic.clear();
        self.rebuild_results();
        self.page(Page::Results);
        let cancel = self.cancel.clone();
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let mut source = source::open(&locator)?;
                let identity = source.identity()?;
                let checkpoint = checkpoint_path(&identity);
                let mut state = if resume {
                    ScanState::load(&checkpoint, source.as_mut())?
                } else {
                    ScanState::start(
                        source.as_mut(),
                        ScanOptions {
                            filesystem_offset: offset,
                        },
                    )?
                };
                let mut last = Instant::now() - Duration::from_secs(1);
                let mut saved = Instant::now();
                let mut scan_error = None;
                let mut save_error = None;
                loop {
                    match state.step(source.as_mut(), &cancel) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(e) => {
                            scan_error = Some(e);
                            break;
                        }
                    }
                    if last.elapsed() > Duration::from_millis(120) {
                        wake.progress(WorkProgress::Scan(state.progress()));
                        last = Instant::now();
                    }
                    if saved.elapsed() > Duration::from_secs(5) {
                        if let Err(e) = state.save(&checkpoint) {
                            save_error = Some(e);
                        }
                        saved = Instant::now();
                    }
                }
                if let Err(e) = state.save(&checkpoint) {
                    save_error = Some(e);
                }
                Ok((state, scan_error, save_error))
            })();
            match result {
                Ok((state, error, save_error)) => {
                    wake.event(Event::ScanDone(Box::new(state), error, save_error))
                }
                Err(e) => wake.event(Event::Failure(e)),
            }
        });
    }
    unsafe fn start_restore(&mut self) {
        if self.busy || self.downloading {
            return;
        }
        let Some(state) = &self.state else {
            return;
        };
        if self.selected.is_empty() {
            self.status_key = "choose_files";
            self.refresh();
            return;
        }
        let destination = PathBuf::from(self.text(DEST));
        let chosen = self
            .selected
            .iter()
            .filter_map(|i| state.results.get(*i).cloned())
            .collect::<Vec<_>>();
        let identity = state.identity.clone();
        let locator = self.source_locator.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        let wake = self.wake.clone();
        self.busy = true;
        self.summary = None;
        self.progress = None;
        self.status_key = "restoring";
        self.layout_dirty = true;
        self.refresh();
        std::thread::spawn(move || {
            let mut last = Instant::now() - Duration::from_secs(1);
            let result = (|| {
                let mut source = source::open(&locator)?;
                file_recovery::restore::restore(
                    source.as_mut(),
                    &identity,
                    &chosen,
                    &destination,
                    &cancel,
                    |done, total| {
                        if last.elapsed() > Duration::from_millis(120) {
                            wake.progress(WorkProgress::Restore(done, total));
                            last = Instant::now();
                        }
                    },
                )
            })();
            match result {
                Ok(summary) => wake.event(Event::Restored(summary)),
                Err(e) => wake.event(Event::Failure(e)),
            }
        });
    }
    fn check_updates(&mut self) {
        if self.update_busy || self.downloading {
            return;
        }
        self.update_busy = true;
        self.update_key = "checking";
        let wake = self.wake.clone();
        std::thread::spawn(move || wake.event(Event::Update(updates::check())));
    }
    unsafe fn install(&mut self) {
        if self.busy || self.downloading {
            return;
        }
        let Some(update) = self.update.clone() else {
            return;
        };
        let parent = match std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
        {
            Some(p) => p,
            None => {
                self.fail("Cannot locate application directory".into());
                return;
            }
        };
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        self.downloading = true;
        self.update_key = "downloading";
        self.status_key = "downloading";
        self.layout_dirty = true;
        self.refresh();
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let mut last = Instant::now() - Duration::from_secs(1);
            let result = updates::download(&update, &parent, &cancel, |done, total| {
                if last.elapsed() > Duration::from_millis(120) {
                    wake.progress(WorkProgress::Download(done, total));
                    last = Instant::now();
                }
            })
            .map(|path| (path, update.sha256));
            wake.event(Event::Downloaded(result));
        });
    }
    unsafe fn receive(&mut self) {
        let (progress, events) = if let Ok(mut mailbox) = self.wake.mailbox.lock() {
            mailbox.posted = false;
            (mailbox.progress.take(), std::mem::take(&mut mailbox.events))
        } else {
            return;
        };
        if let Some(p) = progress {
            self.progress = Some(p);
        }
        for event in events {
            match event {
                Event::ScanDone(state, error, save_error) => {
                    self.busy = false;
                    self.progress = None;
                    self.status_detail.clear();
                    self.status_key = if state.phase != Phase::Finished {
                        "scan_paused"
                    } else if state.capped || !state.warnings.is_empty() {
                        "scan_limited"
                    } else {
                        "scan_done"
                    };
                    self.state = Some(*state);
                    if let Some(e) = error {
                        self.fail(e);
                    }
                    if let Some(e) = save_error {
                        self.status_key = "checkpoint_error";
                        self.diagnostic = e;
                    }
                    self.rebuild_results();
                    self.layout_dirty = true;
                }
                Event::Failure(e) => {
                    self.busy = false;
                    self.progress = None;
                    self.fail(e);
                    self.layout_dirty = true;
                }
                Event::Preview(generation, index, result) => {
                    self.preview_running = false;
                    if self.source_generation == generation && self.preview_index == Some(index) {
                        match result {
                            Ok(text) => self.preview_text = text,
                            Err(e) => {
                                self.preview_text = self.tr("error");
                                self.diagnostic = e;
                            }
                        }
                    }
                    self.start_preview();
                }
                Event::Restored(summary) => {
                    self.busy = false;
                    self.progress = None;
                    self.status_key = if summary.cancelled {
                        "restore_stopped"
                    } else {
                        "restore_done"
                    };
                    self.status_detail = format!(
                        "{}: {} · {}: {}",
                        self.tr("restored"),
                        summary.files.len(),
                        self.tr("failed"),
                        summary.failed.len()
                    );
                    self.summary = Some(summary);
                    self.layout_dirty = true;
                }
                Event::Update(result) => {
                    self.update_busy = false;
                    match result {
                        Ok(Some(update)) => {
                            self.update = Some(update);
                            self.update_key = "available";
                            if !self.busy {
                                self.status_key = "available";
                            }
                        }
                        Ok(None) => self.update_key = "up_to_date",
                        Err(e) => {
                            self.update_key = "update_failed";
                            self.diagnostic = e;
                        }
                    }
                    self.layout_dirty = true;
                }
                Event::Downloaded(result) => {
                    self.downloading = false;
                    self.progress = None;
                    self.layout_dirty = true;
                    if self.closing {
                        if let Ok((path, _)) = result {
                            let _ = std::fs::remove_file(path);
                        }
                        DestroyWindow(self.hwnd);
                        return;
                    }
                    match result {
                        Ok((path, hash)) => match updates::launch_installer(&path, &hash) {
                            Ok(()) => {
                                DestroyWindow(self.hwnd);
                                return;
                            }
                            Err(e) => {
                                self.update_key = "install_failed";
                                self.fail(e);
                            }
                        },
                        Err(e) => {
                            self.update_key = "install_failed";
                            self.fail(e);
                        }
                    }
                }
            }
        }
        if self.closing && !self.busy && !self.downloading {
            DestroyWindow(self.hwnd);
            return;
        }
        self.refresh();
    }

    unsafe fn command(&mut self, id: u16, notification: u16) {
        let action = native_action(id, notification);
        if action == NativeAction::Focus {
            self.ensure_visible(id);
            return;
        }
        if action == NativeAction::ComboChange {
            match id {
                FILTER => {
                    self.rebuild_results();
                    self.refresh();
                }
                LANGUAGE => {
                    let i = SendMessageW(self.control(LANGUAGE), CB_GETCURSEL, 0, 0) as usize;
                    if let Some((code, _)) = LANGUAGES.get(i) {
                        self.settings.language = code.to_string();
                        self.strings = Catalog::new(code);
                        self.localize();
                        self.save_settings();
                        self.refresh();
                    }
                }
                THEME => {
                    self.settings.theme =
                        if SendMessageW(self.control(THEME), CB_GETCURSEL, 0, 0) == 1 {
                            Theme::Dark
                        } else {
                            Theme::Light
                        };
                    self.palette = Palette::new(self.settings.theme);
                    DeleteObject(self.brush);
                    DeleteObject(self.surface_brush);
                    DeleteObject(self.tint_brush);
                    self.brush = CreateSolidBrush(self.palette.bg);
                    self.surface_brush = CreateSolidBrush(self.palette.surface);
                    self.tint_brush = CreateSolidBrush(self.palette.tint);
                    self.save_settings();
                    RedrawWindow(
                        self.hwnd,
                        null(),
                        null_mut(),
                        RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_ERASE,
                    );
                    self.refresh();
                }
                _ => {}
            }
            return;
        }
        if action == NativeAction::Search {
            self.rebuild_results();
            self.refresh();
            return;
        }
        if action == NativeAction::Selection {
            self.selection();
            return;
        }
        if action != NativeAction::Click {
            return;
        }
        if id == 1 {
            let focused = GetDlgCtrlID(GetFocus()) as u16;
            let action = if matches!(
                focused,
                SCAN | RESUME | NEXT | RESTORE | CHECK | INSTALL | BROWSE | DEST_BROWSE
            ) {
                focused
            } else {
                match self.page {
                    Page::Source => SCAN,
                    Page::Results => NEXT,
                    Page::Restore => RESTORE,
                    Page::Settings => CHECK,
                }
            };
            self.command(action, BN_CLICKED as u16);
            return;
        }
        if id == 2 {
            if self.busy || self.downloading {
                self.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        match id {
            NAV_SOURCE if !self.busy => self.page(Page::Source),
            NAV_RESULTS if self.state.is_some() || self.busy => self.page(Page::Results),
            NAV_RESTORE | NEXT if !self.busy && !self.selected.is_empty() => {
                self.summary = None;
                self.page(Page::Restore);
            }
            SETTINGS => self.page(Page::Settings),
            BROWSE => {
                if let Some(path) = choose_image(
                    self.hwnd,
                    &self.tr("browse"),
                    &self.tr("image_filter"),
                    &self.tr("all_files"),
                ) {
                    self.set(PATH, &path);
                }
            }
            DEST_BROWSE => {
                if let Some(path) = choose_folder(self.hwnd, &self.tr("destination")) {
                    self.set(DEST, &path);
                }
            }
            ADVANCED => {
                self.advanced = !self.advanced;
                self.layout_dirty = true;
                self.refresh();
            }
            SCAN => self.start_scan(false),
            RESUME => self.start_scan(true),
            SELECT_ALL => {
                SendMessageW(self.control(LIST), LB_SETSEL, 1, -1);
                self.selection();
            }
            CLEAR => {
                self.selected.clear();
                SendMessageW(self.control(LIST), LB_SETSEL, 0, -1);
                self.selection();
            }
            RESTORE => self.start_restore(),
            CANCEL => {
                self.cancel.store(true, Ordering::Relaxed);
            }
            MOTION => {
                self.settings.reduced_motion = !self.settings.reduced_motion;
                if self.settings.reduced_motion {
                    KillTimer(self.hwnd, ANIMATION);
                    self.animation = None;
                }
                self.save_settings();
                self.refresh();
            }
            STARTUP => {
                self.settings.startup_updates = !self.settings.startup_updates;
                self.save_settings();
                self.refresh();
            }
            CHECK => {
                self.check_updates();
                self.refresh();
            }
            INSTALL => self.install(),
            LICENSES => {
                self.show_licenses = !self.show_licenses;
                self.refresh();
            }
            _ => {}
        }
    }

    unsafe fn scroll_to(&mut self, position: i32) {
        let position = position.clamp(0, self.scroll_max);
        if self.scroll_y != position {
            self.scroll_y = position;
            self.layout_dirty = true;
            self.refresh();
        }
    }
    unsafe fn ensure_visible(&mut self, id: u16) {
        let control = self.control(id);
        if control.is_null() || GetParent(control) != self.body {
            return;
        }
        let mut child: RECT = std::mem::zeroed();
        let mut viewport: RECT = std::mem::zeroed();
        GetWindowRect(control, &mut child);
        MapWindowPoints(null_mut(), self.body, (&mut child as *mut RECT).cast(), 2);
        GetClientRect(self.body, &mut viewport);
        let delta = if child.top < 0 || child.bottom - child.top > viewport.bottom {
            child.top
        } else if child.bottom > viewport.bottom {
            child.bottom - viewport.bottom
        } else {
            0
        };
        if delta != 0 {
            self.scroll_to(self.scroll_y + delta * 96 / self.dpi as i32);
        }
    }
    unsafe fn paint_body(&mut self, dc: HDC) {
        if !self.visible() {
            return;
        }
        self.paints += 1;
        let mut r: RECT = std::mem::zeroed();
        GetClientRect(self.body, &mut r);
        FillRect(dc, &r, self.brush);
        if self.page == Page::Source && !self.advanced {
            let s = |n: i32| n * self.dpi as i32 / 96;
            let card = RECT {
                left: s(32),
                top: s(404 - 144 - self.scroll_y),
                right: r.right - s(32),
                bottom: s(540 - 144 - self.scroll_y),
            };
            FillRect(dc, &card, self.tint_brush);
        }
    }
    unsafe fn paint(&mut self) {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let dc = BeginPaint(self.hwnd, &mut ps);
        if self.visible() {
            self.paints += 1;
            let mut r: RECT = std::mem::zeroed();
            GetClientRect(self.hwnd, &mut r);
            FillRect(dc, &r, self.brush);
            let s = |n: i32| n * self.dpi as i32 / 96;
            SetBkMode(dc, TRANSPARENT as i32);
            SetTextColor(dc, self.palette.text);
            SelectObject(dc, self.fonts[1]);
            DrawIconEx(
                dc,
                s(32),
                s(22),
                LoadIconW(GetModuleHandleW(null()), 1usize as PCWSTR),
                s(32),
                s(32),
                0,
                null_mut(),
                DI_NORMAL,
            );
            let mut title = RECT {
                left: s(78),
                top: s(23),
                right: s(360),
                bottom: s(57),
            };
            DrawTextW(
                dc,
                wide("File Recovery").as_ptr(),
                -1,
                &mut title,
                DT_SINGLELINE | DT_VCENTER,
            );
            SelectObject(dc, self.fonts[0]);
            SetTextColor(dc, self.palette.muted);
            let mut tagline = RECT {
                left: s(390),
                top: s(23),
                right: r.right - s(32),
                bottom: s(57),
            };
            DrawTextW(
                dc,
                wide(&self.tr("tagline")).as_ptr(),
                -1,
                &mut tagline,
                DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
            );
            let divider = CreateSolidBrush(self.palette.border);
            let line = RECT {
                left: s(32),
                top: s(126),
                right: r.right - s(32),
                bottom: s(127),
            };
            FillRect(dc, &line, divider);
            DeleteObject(divider);
            let x = match self.page {
                Page::Source => 24,
                Page::Results => 180,
                Page::Restore => 366,
                Page::Settings => r.right * 96 / self.dpi as i32 - 218,
            };
            let progress = self
                .animation
                .map_or(1., |t| (t.elapsed().as_secs_f32() / 0.16).min(1.));
            let accent = CreateSolidBrush(self.palette.accent);
            let marker = RECT {
                left: s(x),
                top: s(123),
                right: s(x + (140.0 * (1.0 - (1.0 - progress).powi(3))) as i32),
                bottom: s(127),
            };
            FillRect(dc, &marker, accent);
            let fraction =
                match self.progress {
                    Some(WorkProgress::Scan(ref p))
                        if p.phase == Phase::Signatures && p.total_bytes > 0 =>
                    {
                        p.bytes_scanned as f64 / p.total_bytes as f64
                    }
                    Some(
                        WorkProgress::Restore(done, total) | WorkProgress::Download(done, total),
                    ) if total > 0 => done as f64 / total as f64,
                    _ => 0.0,
                };
            if fraction > 0.0 {
                let bar = RECT {
                    left: 0,
                    top: r.bottom - s(4),
                    right: (r.right as f64 * fraction.clamp(0.0, 1.0)) as i32,
                    bottom: r.bottom,
                };
                FillRect(dc, &bar, accent);
            }
            DeleteObject(accent);
        }
        EndPaint(self.hwnd, &ps);
    }
    unsafe fn draw_button(&self, draw: &DRAWITEMSTRUCT) {
        let id = draw.CtlID as u16;
        let primary = matches!(id, SCAN | NEXT | RESTORE | INSTALL);
        let active = match self.page {
            Page::Source => id == NAV_SOURCE,
            Page::Results => id == NAV_RESULTS,
            Page::Restore => id == NAV_RESTORE,
            Page::Settings => id == SETTINGS,
        };
        let disabled = draw.itemState & ODS_DISABLED != 0;
        let pressed = draw.itemState & ODS_SELECTED != 0;
        let background = if disabled {
            self.palette.bg
        } else if primary {
            self.palette.accent
        } else if active || pressed {
            self.palette.tint
        } else {
            self.palette.surface
        };
        let brush = CreateSolidBrush(background);
        FillRect(draw.hDC, &draw.rcItem, brush);
        DeleteObject(brush);
        let border = CreateSolidBrush(if primary || active {
            self.palette.accent
        } else {
            self.palette.border
        });
        FrameRect(draw.hDC, &draw.rcItem, border);
        DeleteObject(border);
        SetBkMode(draw.hDC, TRANSPARENT as i32);
        SetTextColor(
            draw.hDC,
            if disabled {
                self.palette.muted
            } else if primary {
                if self.settings.theme == Theme::Dark {
                    rgb(15, 42, 39)
                } else {
                    rgb(255, 255, 255)
                }
            } else {
                self.palette.text
            },
        );
        SelectObject(draw.hDC, self.fonts[0]);
        let mut rect = draw.rcItem;
        rect.left += 8;
        rect.right -= 8;
        DrawTextW(
            draw.hDC,
            wide(&self.text(id)).as_ptr(),
            -1,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_WORDBREAK,
        );
        if draw.itemState & ODS_FOCUS != 0 {
            let mut focus = draw.rcItem;
            InflateRect(&mut focus, -4, -4);
            DrawFocusRect(draw.hDC, &focus);
        }
    }
    unsafe fn message(&mut self, hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_CREATE => {
                self.hwnd = hwnd;
                self.wake.hwnd = hwnd as usize;
                self.initialize();
                0
            }
            WM_SIZE => {
                if w == SIZE_MINIMIZED as usize {
                    KillTimer(hwnd, ANIMATION);
                    self.animation = None;
                } else {
                    self.layout_dirty = true;
                    self.refresh();
                }
                0
            }
            WM_SHOWWINDOW => {
                if w == 0 {
                    KillTimer(hwnd, ANIMATION);
                    self.animation = None;
                } else {
                    self.layout_dirty = true;
                    self.refresh();
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_DPICHANGED => {
                self.dpi = (w & 0xffff) as u32;
                self.make_fonts();
                for c in &self.controls {
                    SendMessageW(
                        c.hwnd,
                        WM_SETFONT,
                        self.fonts[if c.id == TITLE {
                            2
                        } else if c.id == PREVIEW || c.id == DIAG {
                            3
                        } else {
                            0
                        }] as usize,
                        1,
                    );
                }
                let r = &*(l as *const RECT);
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
                self.layout_dirty = true;
                self.refresh();
                0
            }
            WM_GETMINMAXINFO => {
                let info = &mut *(l as *mut MINMAXINFO);
                let dpi = if self.hwnd.is_null() {
                    GetDpiForSystem().max(96)
                } else {
                    self.dpi
                };
                info.ptMinTrackSize = POINT {
                    x: 840 * dpi as i32 / 96,
                    y: 600 * dpi as i32 / 96,
                };
                0
            }
            WM_COMMAND => {
                self.command((w & 0xffff) as u16, ((w >> 16) & 0xffff) as u16);
                0
            }
            WAKE => {
                self.receive();
                0
            }
            BODY_PAINT => {
                self.paint_body(w as HDC);
                0
            }
            BODY_SCROLL => {
                let mut info: SCROLLINFO = std::mem::zeroed();
                info.cbSize = size_of::<SCROLLINFO>() as u32;
                info.fMask = SIF_TRACKPOS;
                GetScrollInfo(self.body, SB_VERT, &mut info);
                let mut r: RECT = std::mem::zeroed();
                GetClientRect(self.body, &mut r);
                let page = (r.bottom * 96 / self.dpi as i32).max(60);
                let position = match (w & 0xffff) as i32 {
                    SB_LINEUP => self.scroll_y - 36,
                    SB_LINEDOWN => self.scroll_y + 36,
                    SB_PAGEUP => self.scroll_y - page,
                    SB_PAGEDOWN => self.scroll_y + page,
                    SB_TOP => 0,
                    SB_BOTTOM => self.scroll_max,
                    SB_THUMBTRACK | SB_THUMBPOSITION => info.nTrackPos,
                    _ => self.scroll_y,
                };
                self.scroll_to(position);
                0
            }
            BODY_WHEEL | WM_MOUSEWHEEL => {
                let delta = ((w >> 16) as u16) as i16 as i32;
                self.scroll_to(self.scroll_y - delta / 120 * 60);
                0
            }
            WM_TIMER if w == ANIMATION => {
                if !self.visible()
                    || self
                        .animation
                        .is_none_or(|t| t.elapsed() >= Duration::from_millis(160))
                {
                    KillTimer(hwnd, ANIMATION);
                    self.animation = None;
                }
                if self.visible() {
                    let rect = RECT {
                        left: 0,
                        top: 118 * self.dpi as i32 / 96,
                        right: 2000 * self.dpi as i32 / 96,
                        bottom: 130 * self.dpi as i32 / 96,
                    };
                    InvalidateRect(hwnd, &rect, 0);
                }
                0
            }
            WM_PAINT => {
                self.paint();
                0
            }
            WM_ERASEBKGND => 1,
            WM_DRAWITEM => {
                let draw = &*(l as *const DRAWITEMSTRUCT);
                self.draw_button(draw);
                1
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORBTN => {
                let dc = w as HDC;
                SetTextColor(dc, self.palette.text);
                let id = GetDlgCtrlID(l as HWND) as u16;
                let surface = matches!(
                    id,
                    PATH | OFFSET
                        | SEARCH
                        | DEST
                        | LIST
                        | PREVIEW
                        | SUMMARY
                        | DIAG
                        | FILTER
                        | LANGUAGE
                        | THEME
                );
                let tint = matches!(id, SAFETY | SAFETY_HELP);
                SetBkColor(
                    dc,
                    if surface {
                        self.palette.surface
                    } else if tint {
                        self.palette.tint
                    } else {
                        self.palette.bg
                    },
                );
                if surface {
                    self.surface_brush as isize
                } else if tint {
                    self.tint_brush as isize
                } else {
                    self.brush as isize
                }
            }
            WM_CLOSE => {
                if self.busy || self.downloading {
                    self.closing = true;
                    self.cancel.store(true, Ordering::Relaxed);
                } else {
                    DestroyWindow(hwnd);
                }
                0
            }
            WM_DESTROY => {
                KillTimer(hwnd, ANIMATION);
                self.cancel.store(true, Ordering::Relaxed);
                self.wake.alive.store(false, Ordering::Release);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }
}

fn checkpoint_path(identity: &source::SourceIdentity) -> PathBuf {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(serde_json::to_vec(identity).unwrap_or_default());
    Settings::directory().join(format!("scan-{hash:x}.json"))
}
unsafe fn choose_image(
    owner: HWND,
    title: &str,
    image_filter: &str,
    all_files: &str,
) -> Option<String> {
    let mut buffer = vec![0u16; 32768];
    let filter = wide(&format!(
        "{image_filter} (*.img;*.raw;*.dd)\0*.img;*.raw;*.dd\0{all_files}\0*.*\0"
    ));
    let title = wide(title);
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: buffer.as_mut_ptr(),
        nMaxFile: buffer.len() as u32,
        lpstrTitle: title.as_ptr(),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_DONTADDTORECENT,
        ..std::mem::zeroed()
    };
    if GetOpenFileNameW(&mut dialog) == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(
        &buffer[..buffer.iter().position(|v| *v == 0)?],
    ))
}
unsafe fn choose_folder(owner: HWND, title: &str) -> Option<String> {
    let title = wide(title);
    let mut name = [0u16; 260];
    let dialog = BROWSEINFOW {
        hwndOwner: owner,
        pszDisplayName: name.as_mut_ptr(),
        lpszTitle: title.as_ptr(),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..std::mem::zeroed()
    };
    let pidl = SHBrowseForFolderW(&dialog);
    if pidl.is_null() {
        return None;
    }
    let ok = SHGetPathFromIDListW(pidl, name.as_mut_ptr());
    CoTaskMemFree(pidl.cast());
    if ok == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(
        &name[..name.iter().position(|v| *v == 0)?],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_selection_notifications_reach_the_correct_workflow() {
        assert_eq!(CBN_SELCHANGE, LBN_SELCHANGE);
        assert_eq!(
            native_action(LIST, LBN_SELCHANGE as u16),
            NativeAction::Selection
        );
        for id in [FILTER, LANGUAGE, THEME] {
            assert_eq!(
                native_action(id, CBN_SELCHANGE as u16),
                NativeAction::ComboChange
            );
        }
        assert_eq!(
            native_action(SEARCH, EN_CHANGE as u16),
            NativeAction::Search
        );
        assert_eq!(native_action(PATH, EN_CHANGE as u16), NativeAction::Other);
        assert_eq!(
            native_action(RESTORE, BN_CLICKED as u16),
            NativeAction::Click
        );
    }
}
