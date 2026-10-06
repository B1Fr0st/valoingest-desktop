//! Notification-area icon, context menu and balloon notifications, built on
//! the Win32 shell API directly.

use crate::{
    engine::{Command, Shared, Snapshot},
    platform::{self, fill, wide},
    store::DeleteAfter,
};
use std::{
    cell::RefCell,
    ffi::c_void,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicPtr, Ordering},
        mpsc::Sender,
    },
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{CreateBitmap, DeleteObject},
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Shell::{
            NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO,
            NIIF_RESPECT_QUIET_TIME, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
            NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
        },
        WindowsAndMessaging::{
            AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
            DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, HICON,
            ICONINFO, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
            PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetForegroundWindow,
            TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
            TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WM_NULL,
            WNDCLASSEXW, WS_OVERLAPPED,
        },
    },
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;

static WINDOW: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

/// Asks the tray thread to redraw from the shared snapshot. Safe from any thread.
pub fn wake() {
    let hwnd = WINDOW.load(Ordering::Acquire);
    if !hwnd.is_null() {
        unsafe { PostMessageW(hwnd, WM_REFRESH, 0, 0) };
    }
}

#[repr(usize)]
#[derive(Clone, Copy)]
enum Item {
    SignIn = 1,
    CancelSignIn,
    AutoUpload,
    Publish,
    RedactNames,
    RedactPids,
    UploadExisting,
    RetryFailed,
    OpenFolder,
    OpenWebsite,
    Autostart,
    Notifications,
    SignOut,
    Quit,
    DeleteNever,
    DeleteAfterUpload,
    DeleteAfterProcessing,
}

const ITEMS: [Item; 17] = [
    Item::SignIn,
    Item::CancelSignIn,
    Item::AutoUpload,
    Item::Publish,
    Item::RedactNames,
    Item::RedactPids,
    Item::UploadExisting,
    Item::RetryFailed,
    Item::OpenFolder,
    Item::OpenWebsite,
    Item::Autostart,
    Item::Notifications,
    Item::SignOut,
    Item::Quit,
    Item::DeleteNever,
    Item::DeleteAfterUpload,
    Item::DeleteAfterProcessing,
];

struct Tray {
    hwnd: HWND,
    active_icon: HICON,
    idle_icon: HICON,
    taskbar_created: u32,
    shared: Arc<Shared>,
    commands: Sender<Command>,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

pub fn run(shared: Arc<Shared>, commands: Sender<Command>) {
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let class_name = wide("ValoingestTray");
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        RegisterClassExW(&class);
        let title = wide("Valoingest");
        // A hidden top-level window: it receives icon callbacks and owns the menu.
        let hwnd = CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null(),
        );
        if hwnd.is_null() {
            log::error!(
                "could not create tray window: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        TRAY.with(|tray| {
            *tray.borrow_mut() = Some(Tray {
                hwnd,
                active_icon: make_icon([0x55, 0x46, 0xff]),
                idle_icon: make_icon([0xa3, 0x97, 0x8b]),
                taskbar_created,
                shared,
                commands,
            });
        });
        WINDOW.store(hwnd, Ordering::Release);
        with_tray(|tray| tray.add_icon());
        wake();

        let mut message = MSG::default();
        while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        WINDOW.store(ptr::null_mut(), Ordering::Release);
        with_tray(|tray| tray.remove_icon());
    }
}

fn with_tray<R>(f: impl FnOnce(&Tray) -> R) -> Option<R> {
    TRAY.with(|tray| tray.borrow().as_ref().map(f))
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lparam.
            let event = (lparam as u32) & 0xffff;
            if (event == WM_CONTEXTMENU || event == NIN_SELECT || event == WM_LBUTTONUP)
                && let Some(Some(item)) = with_tray(|tray| tray.show_menu())
            {
                activate(item);
            }
            0
        }
        WM_REFRESH => {
            with_tray(|tray| tray.refresh());
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ if with_tray(|tray| tray.taskbar_created == message).unwrap_or(false) => {
            // Explorer restarted: the icon must be added again.
            with_tray(|tray| tray.add_icon());
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn activate(item: Item) {
    let Some((snapshot, commands, hwnd)) = with_tray(|tray| {
        (
            tray.shared.snapshot.lock().unwrap().clone(),
            tray.commands.clone(),
            tray.hwnd,
        )
    }) else {
        return;
    };
    let settings = &snapshot.settings;
    let send = |command| {
        let _ = commands.send(command);
    };
    match item {
        Item::SignIn => send(Command::SignIn),
        Item::CancelSignIn => send(Command::CancelSignIn),
        Item::AutoUpload => send(Command::SetAutoUpload(!settings.auto_upload)),
        Item::Publish => send(Command::SetPublish(!settings.publish)),
        Item::RedactNames => send(Command::SetRedactNames(!settings.redact_names)),
        Item::RedactPids => send(Command::SetRedactPids(!settings.redact_pids)),
        Item::UploadExisting => send(Command::UploadExisting),
        Item::RetryFailed => send(Command::RetryFailed),
        Item::OpenFolder => platform::open_folder(&settings.demos_dir()),
        Item::OpenWebsite => platform::open(settings.api()),
        Item::Autostart => {
            let enable = !platform::autostart_enabled();
            if let Err(error) = platform::set_autostart(enable) {
                log::error!("could not change start-with-Windows: {error}");
            }
        }
        Item::Notifications => send(Command::SetNotifications(!settings.notifications)),
        Item::DeleteNever => send(Command::SetDeleteAfter(DeleteAfter::Never)),
        Item::DeleteAfterUpload => send(Command::SetDeleteAfter(DeleteAfter::Uploaded)),
        Item::DeleteAfterProcessing => send(Command::SetDeleteAfter(DeleteAfter::Processed)),
        Item::SignOut => send(Command::SignOut),
        Item::Quit => unsafe {
            DestroyWindow(hwnd);
        },
    }
}

impl Tray {
    fn icon_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    fn add_icon(&self) {
        let mut data = self.icon_data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.idle_icon;
        fill(&mut data.szTip, "Valoingest");
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &data);
            data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_SETVERSION, &data);
        }
    }

    fn remove_icon(&self) {
        let data = self.icon_data();
        unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
    }

    fn refresh(&self) {
        let snapshot = self.shared.snapshot.lock().unwrap().clone();
        let mut data = self.icon_data();
        data.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        let active = snapshot.signed_in && snapshot.settings.auto_upload;
        data.hIcon = if active {
            self.active_icon
        } else {
            self.idle_icon
        };
        fill(
            &mut data.szTip,
            &format!("Valoingest\n{}", snapshot.activity),
        );
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };

        for notice in self.shared.take_notices() {
            let mut balloon = self.icon_data();
            balloon.uFlags = NIF_INFO;
            fill(&mut balloon.szInfoTitle, &notice.title);
            fill(&mut balloon.szInfo, &notice.body);
            balloon.dwInfoFlags = NIIF_RESPECT_QUIET_TIME
                | if notice.warning {
                    NIIF_WARNING
                } else {
                    NIIF_INFO
                };
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &balloon) };
        }
    }

    /// Builds the menu from the current snapshot and returns the chosen item.
    fn show_menu(&self) -> Option<Item> {
        let snapshot = self.shared.snapshot.lock().unwrap().clone();
        let entries = menu_entries(&snapshot, platform::autostart_enabled());
        unsafe {
            let menu = build_menu(&entries);
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            // Required so the menu closes when the user clicks elsewhere.
            SetForegroundWindow(self.hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_NONOTIFY,
                point.x,
                point.y,
                0,
                self.hwnd,
                ptr::null(),
            );
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            ITEMS.iter().copied().find(|item| *item as i32 == chosen)
        }
    }
}

/// Builds a popup menu; submenus are owned by (and destroyed with) their parent.
unsafe fn build_menu(entries: &[MenuEntry]) -> windows_sys::Win32::UI::WindowsAndMessaging::HMENU {
    unsafe {
        let menu = CreatePopupMenu();
        for entry in entries {
            match entry {
                MenuEntry::Separator => {
                    AppendMenuW(menu, MF_SEPARATOR, 0, ptr::null());
                }
                MenuEntry::Label(text) => {
                    let text = wide(text);
                    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, text.as_ptr());
                }
                MenuEntry::Submenu { text, entries } => {
                    let submenu = build_menu(entries);
                    let text = wide(text);
                    AppendMenuW(menu, MF_STRING | MF_POPUP, submenu as usize, text.as_ptr());
                }
                MenuEntry::Action {
                    item,
                    text,
                    checked,
                    enabled,
                } => {
                    let text = wide(text);
                    let mut flags = MF_STRING;
                    if *checked {
                        flags |= MF_CHECKED;
                    }
                    if !*enabled {
                        flags |= MF_GRAYED;
                    }
                    AppendMenuW(menu, flags, *item as usize, text.as_ptr());
                }
            }
        }
        menu
    }
}

enum MenuEntry {
    Separator,
    Label(String),
    Submenu {
        text: String,
        entries: Vec<MenuEntry>,
    },
    Action {
        item: Item,
        text: String,
        checked: bool,
        enabled: bool,
    },
}

fn action(item: Item, text: impl Into<String>, checked: bool) -> MenuEntry {
    MenuEntry::Action {
        item,
        text: text.into(),
        checked,
        enabled: true,
    }
}

fn menu_entries(snapshot: &Snapshot, autostart: bool) -> Vec<MenuEntry> {
    let settings = &snapshot.settings;
    let mut entries = vec![MenuEntry::Label(format!(
        "Valoingest: {}",
        snapshot.activity
    ))];
    if snapshot.signed_in {
        let who = snapshot
            .account
            .clone()
            .unwrap_or_else(|| "your account".into());
        entries.push(MenuEntry::Label(format!("Signed in as {who}")));
    } else if snapshot.signing_in {
        entries.push(action(
            Item::CancelSignIn,
            "Cancel sign-in (finish in your browser)",
            false,
        ));
    } else {
        entries.push(action(Item::SignIn, "Sign in with Google…", false));
    }
    if !snapshot.folder_found {
        entries.push(MenuEntry::Label(
            "VALORANT replay folder not found yet".into(),
        ));
    }
    entries.push(MenuEntry::Separator);
    entries.push(action(
        Item::AutoUpload,
        "Upload new replays automatically",
        settings.auto_upload,
    ));
    entries.push(action(
        Item::Publish,
        "Publish to the public dataset",
        settings.publish,
    ));
    entries.push(action(
        Item::RedactNames,
        "Redact player names",
        settings.redact_names,
    ));
    entries.push(action(
        Item::RedactPids,
        "Redact player and match IDs",
        settings.redact_pids,
    ));
    let mode = settings.delete_after;
    entries.push(MenuEntry::Submenu {
        text: match mode {
            DeleteAfter::Never => "Delete replays after upload: never".into(),
            DeleteAfter::Uploaded => "Delete replays after upload: once uploaded".into(),
            DeleteAfter::Processed => "Delete replays after upload: once processed".into(),
        },
        entries: vec![
            action(Item::DeleteNever, "Never", mode == DeleteAfter::Never),
            action(
                Item::DeleteAfterUpload,
                "Once uploaded",
                mode == DeleteAfter::Uploaded,
            ),
            action(
                Item::DeleteAfterProcessing,
                "Once processed successfully (failed replays stay)",
                mode == DeleteAfter::Processed,
            ),
            MenuEntry::Separator,
            MenuEntry::Label("Deleted replays go to the Recycle Bin".into()),
            MenuEntry::Label("and disappear from VALORANT's replay list".into()),
        ],
    });
    entries.push(MenuEntry::Separator);
    if snapshot.existing > 0 {
        entries.push(action(
            Item::UploadExisting,
            format!("Upload {} earlier replays", snapshot.existing),
            false,
        ));
    }
    if snapshot.failed > 0 {
        entries.push(action(
            Item::RetryFailed,
            format!("Retry {} failed uploads", snapshot.failed),
            false,
        ));
    }
    entries.push(MenuEntry::Label(format!(
        "{} uploaded · {} processing · {} waiting",
        snapshot.ready, snapshot.processing, snapshot.pending
    )));
    entries.push(MenuEntry::Action {
        item: Item::OpenFolder,
        text: "Open replays folder".into(),
        checked: false,
        enabled: snapshot.folder_found,
    });
    entries.push(action(Item::OpenWebsite, "Open Valoingest website", false));
    entries.push(MenuEntry::Separator);
    entries.push(action(Item::Autostart, "Start with Windows", autostart));
    entries.push(action(
        Item::Notifications,
        "Show notifications",
        settings.notifications,
    ));
    if snapshot.signed_in {
        entries.push(action(Item::SignOut, "Sign out", false));
    }
    entries.push(action(Item::Quit, "Quit Valoingest", false));
    entries
}

/// Draws the 32×32 icon: a rounded square in `bgr` with a white chevron.
fn make_icon(bgr: [u8; 3]) -> HICON {
    const SIZE: i32 = 32;
    let mut pixels = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let square = coverage(rounded_rect_distance(px, py, 16.0, 16.0, 14.5, 6.0));
            let chevron = coverage(
                segment_distance(px, py, (8.5, 9.0), (16.0, 24.0)).min(segment_distance(
                    px,
                    py,
                    (23.5, 9.0),
                    (16.0, 24.0),
                )) - 2.2,
            );
            let alpha = square;
            let white = chevron * square;
            let offset = ((y * SIZE + x) * 4) as usize;
            for channel in 0..3 {
                let color = bgr[channel] as f32 * (1.0 - white) + 255.0 * white;
                // Premultiplied alpha.
                pixels[offset + channel] = (color * alpha) as u8;
            }
            pixels[offset + 3] = (alpha * 255.0) as u8;
        }
    }
    let mask = vec![0_u8; (SIZE * SIZE / 8) as usize];
    unsafe {
        let color = CreateBitmap(SIZE, SIZE, 1, 32, pixels.as_ptr().cast());
        let mask = CreateBitmap(SIZE, SIZE, 1, 1, mask.as_ptr().cast());
        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        DeleteObject(color);
        DeleteObject(mask);
        icon
    }
}

fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

fn rounded_rect_distance(x: f32, y: f32, cx: f32, cy: f32, half: f32, radius: f32) -> f32 {
    let qx = (x - cx).abs() - (half - radius);
    let qy = (y - cy).abs() - (half - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - radius
}

fn segment_distance(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((x - a.0 - t * dx).powi(2) + (y - a.1 - t * dy).powi(2)).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_reflects_state() {
        let mut snapshot = Snapshot::default();
        snapshot.settings.auto_upload = true;
        snapshot.existing = 3;
        let labels = |entries: &[MenuEntry]| {
            entries
                .iter()
                .filter_map(|entry| match entry {
                    MenuEntry::Action { text, .. }
                    | MenuEntry::Label(text)
                    | MenuEntry::Submenu { text, .. } => Some(text.clone()),
                    MenuEntry::Separator => None,
                })
                .collect::<Vec<_>>()
        };
        let signed_out = labels(&menu_entries(&snapshot, false));
        assert!(
            signed_out
                .iter()
                .any(|t| t == "Delete replays after upload: never")
        );
        assert!(
            signed_out
                .iter()
                .any(|t| t.starts_with("Sign in with Google"))
        );
        assert!(signed_out.iter().any(|t| t == "Upload 3 earlier replays"));
        assert!(!signed_out.iter().any(|t| t == "Sign out"));

        snapshot.signed_in = true;
        snapshot.account = Some("me@example.com".into());
        let signed_in = labels(&menu_entries(&snapshot, false));
        assert!(signed_in.iter().any(|t| t == "Signed in as me@example.com"));
        assert!(signed_in.iter().any(|t| t == "Sign out"));
    }

    #[test]
    fn icon_shape_is_opaque_inside_and_clear_at_corners() {
        assert!(coverage(rounded_rect_distance(16.0, 16.0, 16.0, 16.0, 14.5, 6.0)) > 0.99);
        assert_eq!(
            coverage(rounded_rect_distance(0.5, 0.5, 16.0, 16.0, 14.5, 6.0)),
            0.0
        );
        assert!(segment_distance(16.0, 24.0, (8.5, 9.0), (16.0, 24.0)) < 0.01);
    }
}
