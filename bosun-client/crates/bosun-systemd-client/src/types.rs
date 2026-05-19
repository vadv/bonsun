//! Plain data types returned by `SystemdManager` methods.

use zbus::zvariant::OwnedObjectPath;

/// Opaque handle to a systemd job, returned by every state-changing method
/// (`start_unit`, `restart_unit`, `reload_unit`, ...). Wraps an
/// `OwnedObjectPath` so the inner representation stays private but consumers
/// can still pass the value back into `wait_for_job`.
#[derive(Debug, Clone)]
pub struct JobHandle(pub(crate) OwnedObjectPath);

impl JobHandle {
    /// Construct from a raw dbus object path. Made public so callers (and
    /// tests) can reconstruct a handle from a value retrieved out-of-band.
    pub fn new(path: OwnedObjectPath) -> Self {
        Self(path)
    }

    /// Borrow the object path as a string slice (e.g.
    /// `"/org/freedesktop/systemd1/job/42"`).
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Borrow the inner object path.
    pub fn object_path(&self) -> &OwnedObjectPath {
        &self.0
    }
}

/// Outcome of a single systemd job, as observed via the `JobRemoved` signal
/// followed by a property read.
///
/// `result` carries the systemd-defined string
/// (`"done" | "canceled" | "timeout" | "failed" | "dependency" | "skipped"`).
/// `active_state` is the unit's `ActiveState` queried immediately after the
/// signal arrived; this is needed to defend against Debian bug 996911 where
/// `JobRemoved` reports `done` for a unit that subsequently failed to start.
#[derive(Debug, Clone)]
pub struct JobResult {
    pub result: String,
    pub active_state: String,
}

/// Minimal snapshot of a systemd unit, sufficient for `InvocationID`-based
/// restart verification.
///
/// `invocation_id` is rendered as a lower-case hex string of the 16-byte
/// systemd UUID. It changes on every (re)start, so two snapshots taken
/// before/after `restart_unit` allow the caller to decide whether a restart
/// actually executed.
///
/// `exec_main_start_timestamp` is the realtime-clock micros from the
/// `Service` interface; absent for non-service units (timers, sockets, ...),
/// hence `Option`.
#[derive(Debug, Clone)]
pub struct UnitInfo {
    pub name: String,
    pub active_state: String,
    pub sub_state: String,
    pub invocation_id: String,
    pub exec_main_start_timestamp: Option<u64>,
}

/// Render a 16-byte systemd InvocationID as a lowercase hex string. Only the
/// canonical 16-byte length is accepted; any other length is treated as
/// "unavailable" and returns an empty string so the caller can detect it via
/// `is_empty()` without panicking.
///
/// Documented as crate-public because the same routine is reused by tests
/// that simulate the property fetch.
pub(crate) fn render_invocation_id(bytes: &[u8]) -> String {
    if bytes.len() != 16 {
        return String::new();
    }
    let mut out = String::with_capacity(32);
    for b in bytes {
        // Hex digits are pure ASCII, never panics. Use write! to avoid format
        // machinery cost is unnecessary for 16 iterations.
        let hi = HEX[(b >> 4) as usize];
        let lo = HEX[(b & 0x0f) as usize];
        out.push(hi as char);
        out.push(lo as char);
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_invocation_id_known_vector() {
        // Standard test vector: 16 bytes 0x00..0x0f → "000102030405060708090a0b0c0d0e0f".
        let v: Vec<u8> = (0u8..16u8).collect();
        assert_eq!(render_invocation_id(&v), "000102030405060708090a0b0c0d0e0f");
    }

    #[test]
    fn render_invocation_id_real_uuid_shape() {
        // Mimic a real systemd InvocationID — random-looking 16 bytes.
        let v: [u8; 16] = [
            0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e,
            0x8f, 0x90,
        ];
        assert_eq!(render_invocation_id(&v), "a1b2c3d4e5f60718293a4b5c6d7e8f90");
    }

    #[test]
    fn render_invocation_id_rejects_wrong_length() {
        assert_eq!(render_invocation_id(&[]), "");
        assert_eq!(render_invocation_id(&[0u8; 8]), "");
        assert_eq!(render_invocation_id(&[0u8; 32]), "");
    }
}
