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
            // Como PO 2024: tag inexistente = error 424 ("Tag not found").
            unsafe { SetLastError(CT_ERR + 424) };
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
        unsafe { SetLastError(CT_ERR + 424) };
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctCicode(_h: *mut c_void, cmd: *const c_char, _w: u32, _m: u32, buf: *mut c_char, len: u32, _o: *mut c_void) -> u32 {
    let c = unsafe { s(cmd) };
    // Como PO 2024: ProjectCurrentGet() da error Citect 292 y el proyecto sale de ProjectInfo(0).
    if c == "ProjectCurrentGet()" {
        unsafe { SetLastError(CT_ERR + 292) };
        return 0;
    }
    let r = if c.starts_with("Time(") { "10:15:00".to_string() } else if c == "ProjectInfo(0)" { "PO_Demo".into() } else if c.starts_with("AlarmAckTag(") { "0".into() } else { format!("eval:{c}") };
    unsafe { put(buf, len, &r) as u32 }
}

struct Find { rows: Vec<Vec<(String, String)>>, idx: usize }

fn kv(p: &[(&str, String)]) -> Vec<(String, String)> { p.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }

unsafe fn first(table: *const c_char, filter: *const c_char, pobj: *mut *mut c_void) -> *mut c_void {
    let (t, f) = unsafe { (s(table), s(filter)) };
    // Como PO 2024: TYPE de la tabla Tag es un código (0 digital, 2 real), sin UNIT/ADDR.
    let rows: Vec<Vec<(String, String)>> = if t == "Tag" {
        let pre = f.strip_prefix("TAG=").unwrap_or("").trim_end_matches('*').to_string();
        [("Feeder1\\MMXU1\\TotW", "2", "kW"), ("Feeder1\\XCBR1\\Pos", "0", ""), ("Feeder2\\MMXU1\\TotW", "2", "kW")]
            .iter()
            .filter(|r| r.0.starts_with(&pre))
            .map(|r| kv(&[("TAG", r.0.into()), ("TYPE", r.1.into()), ("ENG_UNITS", r.2.into()), ("CLUSTER", "c1".into())]))
            .collect()
    } else if let Some(q) = t.strip_prefix("TRNQUERY,") {
        assert!(filter.is_null(), "TRNQUERY debe ir con filtro NULL");
        let a: Vec<&str> = q.split(',').collect();
        assert!(a.len() == 7 || a.len() == 9, "TRNQUERY con {} argumentos: {q}", a.len());
        // Como PO 2024: InstantTrend=0 y SamplePeriod=0 al final se rechazan (error 274).
        if a.len() == 9 && a[7] == "0" && a[8] == "0" {
            unsafe { SetLastError(CT_ERR + 274) };
            return std::ptr::null_mut();
        }
        let end: i64 = a[0].parse().unwrap();
        let period: f64 = a[2].parse().unwrap();
        let n: usize = a[3].parse().unwrap();
        // Como el servidor real: de la muestra más nueva a la más vieja.
        (0..n)
            .rev()
            .map(|i| {
                let secs = end - ((n - 1 - i) as f64 * period) as i64;
                kv(&[("DATETIME", secs.to_string()), ("MSECONDS", "0".into()), ("VALUE", format!("{}", i as f64 * 1.5)), ("QUALITY", if i == 0 { "2".into() } else { "18".into() })])
            })
            .collect()
    } else if t == "Alarm" {
        vec![kv(&[("TAG", "Feeder1_Trip".into()), ("NAME", "Disparo".into()), ("STATE", "ON".into()), ("ALARMTYPE", "Digital".into())])]
    } else if t == "AlarmSummary" {
        vec![kv(&[("TAG", "Feeder1_Trip".into()), ("NAME", "Disparo".into()), ("ONTIME", "10:14:00".into()), ("OFFTIME", "10:14:10".into()), ("USERNAME", "System".into())])]
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

// ---- ctTagReadEx, ctTagGetProperty y listas (ctapi.h de 8.x) ----

/// CT_TAGVALUE_ITEMS (pack 1, 38 bytes).
#[repr(C, packed)]
pub struct Items {
    len: u32,
    ts: u64,
    vts: u64,
    qts: u64,
    general: u8,
    sub: u8,
    limit: u8,
    ext: u8,
    ds: u32,
    ovr: u8,
    ctl: u8,
}

/// Horas distintas según el camino, para que los tests sepan si se leyó por lista o por ctTagReadEx:
/// 2026-11-27T10:00:00Z (ctTagReadEx) y 10:00:05Z (lista), como FILETIME.
const FT_EX: u64 = (1_795_773_600 + 11_644_473_600) * 10_000_000;
const FT_LIST: u64 = FT_EX + 50_000_000;

/// Valor y calidad (general, substatus, extendido) de un tag conocido.
fn reading(tag: &str) -> Option<(String, u8, u8, u8)> {
    Some(match tag {
        "Feeder1\\MMXU1\\TotW" => ("1234.5".into(), 3, 0, 0),
        "Feeder1\\XCBR1\\Pos" => (POS.lock().unwrap().clone(), 3, 0, 0),
        // Como un dispositivo sin comunicación: BAD/COMM_FAILURE, extendido COMM_DEV_BAD.
        "Feeder2\\MMXU1\\TotW" => ("0".into(), 0, 6, 0x11),
        _ => return None,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctTagReadEx(h: *mut c_void, tag: *const c_char, buf: *mut c_char, len: u32, items: *mut Items) -> i32 {
    assert_eq!(h as usize, 0x1234);
    // Como PO 2024 (visto el 2026-11-27): un tag inexistente no falla; da TRUE, valor vacío, BAD e INVALID_TAG, sin horas.
    let (v, general, sub, ext) = reading(&unsafe { s(tag) }).unwrap_or_else(|| (String::new(), 0, 0, 2));
    let ft = if ext == 2 { 0 } else { FT_EX };
    if !items.is_null() {
        let it = unsafe { &mut *items };
        assert_eq!({ it.len }, 38, "dwLength debe ser sizeof(CT_TAGVALUE_ITEMS)");
        *it = Items { len: 38, ts: ft, vts: ft, qts: ft, general, sub, limit: 0, ext, ds: 0, ovr: 0, ctl: 0 };
    }
    unsafe { put(buf, len, &v) as i32 }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctTagGetProperty(_h: *mut c_void, tag: *const c_char, prop: *const c_char, data: *mut c_void, len: u32, ty: u32) -> i32 {
    assert_eq!(ty, 129, "se piden como DBTYPE_STR");
    let v = match (unsafe { s(tag) }.as_str(), unsafe { s(prop) }.as_str()) {
        ("Feeder1\\MMXU1\\TotW", "Units") => "kW",
        ("Feeder1\\MMXU1\\TotW", "Description") => "Potencia total",
        ("Feeder1\\MMXU1\\TotW", "EngUnitsLow") => "0",
        ("Feeder1\\MMXU1\\TotW", "EngUnitsHigh") => "1000",
        ("Feeder1\\MMXU1\\TotW", "Type") => "5",
        _ => {
            unsafe { SetLastError(CT_ERR + 424) };
            return 0;
        }
    };
    unsafe { put(data as *mut c_char, len, v) as i32 }
}

struct ListTag { name: String, read: Option<(String, u8, u8, u8)> }
struct List { tags: Vec<*mut ListTag> }

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctListNew(h: *mut c_void, _mode: u32) -> *mut c_void {
    assert_eq!(h as usize, 0x1234);
    Box::into_raw(Box::new(List { tags: vec![] })) as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctListAdd(l: *mut c_void, tag: *const c_char) -> *mut c_void {
    let list = unsafe { &mut *(l as *mut List) };
    let t = Box::into_raw(Box::new(ListTag { name: unsafe { s(tag) }, read: None }));
    list.tags.push(t);
    t as *mut c_void
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctListRead(l: *mut c_void, ov: *mut c_void) -> i32 {
    assert!(ov.is_null(), "lectura síncrona");
    let list = unsafe { &mut *(l as *mut List) };
    for &t in &list.tags {
        let t = unsafe { &mut *t };
        // Como PO 2024 (visto el 2026-11-27): un tag inexistente no hace fallar la lista; vuelve vacío, BAD y con
        // extendido INVALID_TAG (0x02), sin horas.
        t.read = Some(reading(&t.name).unwrap_or_else(|| (String::new(), 0, 0, 2)));
    }
    1
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctListData(t: *mut c_void, buf: *mut c_void, len: u32, _mode: u32) -> i32 {
    match &unsafe { &*(t as *const ListTag) }.read {
        Some((v, ..)) => unsafe { put(buf as *mut c_char, len, v) as i32 },
        None => {
            unsafe { SetLastError(CT_ERR + 424) };
            0
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctListItem(t: *mut c_void, item: u32, buf: *mut c_void, len: u32, _mode: u32) -> i32 {
    let Some((_, general, sub, ext)) = &unsafe { &*(t as *const ListTag) }.read else {
        unsafe { SetLastError(CT_ERR + 424) };
        return 0;
    };
    let v = match item {
        2..=4 if *ext == 2 => String::new(),
        2..=4 => FT_LIST.to_string(),
        5 => general.to_string(),
        6 => sub.to_string(),
        8 => ext.to_string(),
        7 | 9..=11 => "0".into(),
        _ => panic!("ctListItem con elemento {item}"),
    };
    unsafe { put(buf as *mut c_char, len, &v) as i32 }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn ctListFree(l: *mut c_void) -> i32 {
    let list = unsafe { Box::from_raw(l as *mut List) };
    for t in list.tags {
        drop(unsafe { Box::from_raw(t) });
    }
    1
}
