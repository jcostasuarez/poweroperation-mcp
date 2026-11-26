//! CtApi.dll falso para probar el FFI del MCP sin Power Operation.
#![allow(non_snake_case, clippy::missing_safety_doc)]
use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use windows_sys::Win32::Foundation::SetLastError;

static POS: Mutex<String> = Mutex::new(String::new());
const CT_ERR: u32 = 0x1000_0000;

unsafe fn s(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}
unsafe fn put(buf: *mut c_char, len: u32, v: &str) -> bool {
    let b = v.as_bytes();
    if b.len() + 1 > len as usize { return false; }
    unsafe {
        std::ptr::copy_nonoverlapping(b.as_ptr(), buf as *mut u8, b.len());
        *buf.add(b.len()) = 0;
    }
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctSetManagedBinDirectory(_p: *const c_char) -> i32 { 1 }

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctOpen(c: *const c_char, _u: *const c_char, _p: *const c_char, mode: u32) -> *mut c_void {
    if unsafe { s(c) } == "fail" || mode & 0x2 == 0 {
        unsafe { SetLastError(CT_ERR + 266) };
        return std::ptr::null_mut();
    }
    *POS.lock().unwrap() = "2".into();
    0x1234 as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctClose(_h: *mut c_void) -> i32 { 1 }

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctTagRead(h: *mut c_void, tag: *const c_char, buf: *mut c_char, len: u32) -> i32 {
    assert_eq!(h as usize, 0x1234);
    let v = match unsafe { s(tag) }.as_str() {
        "Feeder1\\MMXU1\\TotW" => "1234.5".to_string(),
        "Feeder1\\XCBR1\\Pos" => POS.lock().unwrap().clone(),
        _ => {
            unsafe { SetLastError(CT_ERR + 257) };
            return 0;
        }
    };
    unsafe { put(buf, len, &v) as i32 }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctTagWrite(_h: *mut c_void, tag: *const c_char, val: *const c_char) -> i32 {
    if unsafe { s(tag) } == "Feeder1\\XCBR1\\Pos" {
        *POS.lock().unwrap() = unsafe { s(val) };
        1
    } else {
        unsafe { SetLastError(CT_ERR + 257) };
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctCicode(_h: *mut c_void, cmd: *const c_char, _w: u32, _m: u32, buf: *mut c_char, len: u32, _o: *mut c_void) -> u32 {
    let c = unsafe { s(cmd) };
    let r = if c.starts_with("Time(") { "10:15:00".to_string() } else if c == "ProjectCurrentGet()" { "PO_Demo".into() } else if c.starts_with("AlarmAckTag(") { "0".into() } else { format!("eval:{c}") };
    unsafe { put(buf, len, &r) as u32 }
}

struct Find { rows: Vec<Vec<(String, String)>>, idx: usize }

fn kv(p: &[(&str, String)]) -> Vec<(String, String)> { p.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }

unsafe fn first(table: *const c_char, filter: *const c_char, pobj: *mut *mut c_void) -> *mut c_void {
    let (t, f) = unsafe { (s(table), s(filter)) };
    let rows: Vec<Vec<(String, String)>> = if t == "Tag" {
        let pre = f.strip_prefix("TAG=").unwrap_or("").trim_end_matches('*').to_string();
        [("Feeder1\\MMXU1\\TotW", "REAL", "kW"), ("Feeder1\\XCBR1\\Pos", "DIGITAL", ""), ("Feeder2\\MMXU1\\TotW", "REAL", "kW")]
            .iter()
            .filter(|r| r.0.starts_with(&pre))
            .map(|r| kv(&[("TAG", r.0.into()), ("TYPE", r.1.into()), ("ENG_UNITS", r.2.into())]))
            .collect()
    } else if let Some(q) = t.strip_prefix("TRNQUERY,") {
        assert!(filter.is_null(), "TRNQUERY debe ir con filtro NULL");
        let a: Vec<&str> = q.split(',').collect();
        assert_eq!(a.len(), 9, "TRNQUERY con {} argumentos: {q}", a.len());
        let end: i64 = a[0].parse().unwrap();
        let period: f64 = a[2].parse().unwrap();
        let n: usize = a[3].parse().unwrap();
        (0..n)
            .map(|i| {
                let secs = end - ((n - 1 - i) as f64 * period) as i64;
                kv(&[("DATETIME", secs.to_string()), ("MSECONDS", "0".into()), ("VALUE", format!("{}", i as f64 * 1.5)), ("QUALITY", if i == 0 { "2".into() } else { "18".into() })])
            })
            .collect()
    } else if t == "Alarm" {
        vec![kv(&[("TAG", "Feeder1_Trip".into()), ("NAME", "Disparo".into()), ("STATE", "ON".into())])]
    } else {
        vec![]
    };
    if rows.is_empty() {
        unsafe { SetLastError(CT_ERR + 275) };
        return std::ptr::null_mut();
    }
    let b = Box::into_raw(Box::new(Find { rows, idx: 0 })) as *mut c_void;
    unsafe { *pobj = b };
    b
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctFindFirstEx(_h: *mut c_void, t: *const c_char, f: *const c_char, _c: *const c_char, pobj: *mut *mut c_void, _fl: u32) -> *mut c_void {
    unsafe { first(t, f, pobj) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctFindFirst(_h: *mut c_void, t: *const c_char, f: *const c_char, pobj: *mut *mut c_void, _fl: u32) -> *mut c_void {
    unsafe { first(t, f, pobj) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctFindNext(h: *mut c_void, pobj: *mut *mut c_void) -> i32 {
    let f = unsafe { &mut *(h as *mut Find) };
    if f.idx + 1 >= f.rows.len() { return 0; }
    f.idx += 1;
    unsafe { *pobj = h };
    1
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctFindClose(h: *mut c_void) -> i32 {
    drop(unsafe { Box::from_raw(h as *mut Find) });
    1
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctGetProperty(obj: *mut c_void, name: *const c_char, data: *mut c_void, len: u32, reslen: *mut u32, ty: u32) -> i32 {
    assert_eq!(ty, 129);
    let f = unsafe { &*(obj as *const Find) };
    let n = unsafe { s(name) };
    match f.rows[f.idx].iter().find(|(k, _)| *k == n) {
        Some((_, v)) => {
            unsafe { *reslen = v.len() as u32 };
            unsafe { put(data as *mut c_char, len, v) as i32 }
        }
        None => 0,
    }
}
