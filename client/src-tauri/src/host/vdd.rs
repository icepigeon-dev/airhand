//! 已确认的 Windows 会话共用一块按需创建的 Parsec 虚拟屏。
//! 驱动接口见 https://github.com/nomi-san/parsec-vdd/blob/main/docs/VDD_LIBRARY_USAGE.md 。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::LazyLock;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_Get_Device_Interface_List_SizeW,
    CM_Get_Device_Interface_ListW, CR_SUCCESS,
};
use windows::Win32::Foundation::{ERROR_IO_PENDING, HANDLE};
use windows::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;
use windows::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::{GUID, HRESULT, PCWSTR};

const INTERFACE: GUID = GUID::from_u128(0x00b41627_04c4_429e_a26e_0265cf50c8fa);
const ADD: u32 = 0x22e004;
const REMOVE: u32 = 0x22a008;
const UPDATE: u32 = 0x22a00c;
const HEARTBEAT: Duration = Duration::from_millis(80);
const WATCHDOG: Duration = Duration::from_secs(1);

enum Event {
    Connected,
    DisplayVisible,
    NeedDisplay,
    Disconnected,
}

static EVENTS: LazyLock<Option<Sender<Event>>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel();
    match std::thread::Builder::new()
        .name("airhand-vdd".into())
        .spawn(move || manage(rx))
    {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::warn!("can't start the virtual display worker: {e}");
            None
        }
    }
});

/// 即使会话先用上了别的会话创建的屏，也要持有它直到自己离开。
pub(super) struct Session(&'static Sender<Event>);

impl Session {
    pub fn connected() -> Option<Self> {
        let tx = EVENTS.as_ref()?;
        tx.send(Event::Connected).ok()?;
        Some(Self(tx))
    }

    pub fn observe(&self, virtual_present: bool, need_virtual: bool) {
        if virtual_present {
            let _ = self.0.send(Event::DisplayVisible);
        } else if need_virtual {
            let _ = self.0.send(Event::NeedDisplay);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.0.send(Event::Disconnected);
    }
}

fn manage(rx: Receiver<Event>) {
    let mut sessions = 0usize;
    let mut owned: Option<(File, u16)> = None;
    let mut last_ping = Instant::now();
    let mut next_attempt = Instant::now();
    let mut last_seen = None::<Instant>;
    loop {
        let event = if owned.is_some() {
            rx.recv_timeout(HEARTBEAT)
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        // 睡眠或驱动重置后，旧的序号可能已被别的程序占用。
        if owned.is_some() && last_ping.elapsed() >= WATCHDOG {
            owned = None;
        }
        match event {
            Ok(Event::Connected) => sessions += 1,
            Ok(Event::DisplayVisible) => {
                if owned.is_some() {
                    last_seen = Some(Instant::now());
                }
            }
            Ok(Event::Disconnected) => {
                sessions -= 1;
                if sessions == 0 {
                    if let Some((file, index)) = owned.take() {
                        let removed = control(&file, REMOVE, index)
                            .and_then(|_| control(&file, UPDATE, 0));
                        if let Err(e) = removed {
                            tracing::warn!("can't remove Parsec display {index}: {e:#}");
                        }
                    }
                    next_attempt = Instant::now();
                }
            }
            Ok(Event::NeedDisplay) if sessions > 0 => {
                // 接口仍能响应心跳，但原先已出现的屏连续多次枚举都不在了。放开旧句柄，
                // 给驱动留出回收时间；不要拿失效的序号再发 REMOVE。
                if owned.is_some() && last_seen.is_some_and(|t| t.elapsed() >= Duration::from_secs(5)) {
                    owned = None;
                    next_attempt = Instant::now() + WATCHDOG;
                }
                if owned.is_none() && Instant::now() >= next_attempt {
                    last_seen = None;
                    next_attempt = Instant::now() + Duration::from_secs(5);
                    match add_display() {
                        Ok(display) => owned = display,
                        Err(e) => tracing::warn!("can't add a Parsec display: {e:#}"),
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Ok(Event::NeedDisplay) | Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some((file, _)) = &owned {
            if let Err(e) = control(file, UPDATE, 0) {
                tracing::warn!("lost the Parsec display: {e:#}");
                owned = None;
            } else {
                last_ping = Instant::now();
            }
        }
    }
}

fn add_display() -> Result<Option<(File, u16)>> {
    let flags = CM_GET_DEVICE_INTERFACE_LIST_PRESENT;
    let mut len = 0;
    let rc = unsafe {
        CM_Get_Device_Interface_List_SizeW(&mut len, &INTERFACE, PCWSTR::null(), flags)
    };
    ensure!(rc == CR_SUCCESS, "CM_Get_Device_Interface_List_SizeW: {}", rc.0);
    if len < 2 {
        return Ok(None);
    }
    let mut paths = vec![0u16; len as usize];
    let rc = unsafe { CM_Get_Device_Interface_ListW(&INTERFACE, PCWSTR::null(), &mut paths, flags) };
    ensure!(rc == CR_SUCCESS, "CM_Get_Device_Interface_ListW: {}", rc.0);
    let mut last_error = None;
    for path in paths.split(|c| *c == 0).take_while(|p| !p.is_empty()) {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .open(OsString::from_wide(path))
        {
            Ok(file) => file,
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        };
        let index = control(&file, ADD, 0)?;
        ensure!(index < 16, "unexpected Parsec VDD index {index}");
        tracing::info!(index, "added a Parsec virtual display");
        return Ok(Some((file, index as u16)));
    }
    match last_error {
        Some(e) => Err(e).context("open Parsec VDD"),
        None => Ok(None),
    }
}

fn control(file: &File, code: u32, index: u16) -> Result<u32> {
    let device = HANDLE(file.as_raw_handle());
    let mut input = [0u8; 32];
    input[..2].copy_from_slice(&index.to_be_bytes());
    let mut output = 0u32;
    let (out_buffer, out_size) = if code == ADD {
        (Some((&raw mut output).cast()), size_of::<u32>() as u32)
    } else {
        (None, 0)
    };
    let mut bytes = 0;
    unsafe {
        let event = OwnedHandle::from_raw_handle(CreateEventW(None, true, false, None)?.0);
        let mut operation = OVERLAPPED {
            hEvent: HANDLE(event.as_raw_handle()),
            ..Default::default()
        };
        let result = DeviceIoControl(
            device,
            code,
            Some(input.as_ptr().cast()),
            input.len() as u32,
            out_buffer,
            out_size,
            None,
            Some(&mut operation),
        );
        let timeout_ms = if code == ADD { 5_000 } else { 1_000 };
        let completed = match result {
            Err(e) if e.code() != HRESULT::from_win32(ERROR_IO_PENDING.0) => Err(e),
            _ => GetOverlappedResultEx(device, &operation, &mut bytes, timeout_ms, false),
        };
        if let Err(e) = completed {
            // 取消后等到内核真正放开 OVERLAPPED 和缓冲区，再让它们离开作用域。
            let _ = CancelIoEx(device, Some(&operation));
            let _ = GetOverlappedResult(device, &operation, &mut bytes, true);
            return Err(e.into());
        }
    }
    ensure!(code != ADD || bytes == 4, "Parsec VDD returned an incomplete display index");
    Ok(output)
}
