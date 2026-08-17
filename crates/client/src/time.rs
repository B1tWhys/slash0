use slash0_core::timestamp::Timestamp;
use web_sys::js_sys::Date;

/// Current wall-clock time as a [`Timestamp`], for feeding the shader's
/// "time since last update" fade each frame.
pub fn now_timestamp() -> Timestamp {
    Timestamp::from_millis(Date::now() as u64)
}
