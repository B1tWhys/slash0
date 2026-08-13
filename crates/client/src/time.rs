use std::ops::Add;
use std::time::{Duration, SystemTime};
use web_sys::js_sys::Date;

pub fn now_ms() -> SystemTime {
    // let window = web_sys::window().unwrap()
    //     .performance()
    //     .unwrap()
    //     .now();
    // Instant::now()
    let epoch_ms = Date::now() as u64;
    SystemTime::UNIX_EPOCH.add(Duration::from_millis(epoch_ms))
}
