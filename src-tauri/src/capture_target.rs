//! App-owned, nonpersistent override for exactly one GUI capture attempt.
//! IPC only reads the last GUI display query; it never starts a helper.
pub use crate::capture_area::{CaptureArea, Region};
pub use crate::capture_engine::DisplayInfo;
use crate::{
    capture::CaptureManager,
    message::AppMessage,
    operation_gate::{OperationGate, Owner},
    selection_session::SelectionManager,
};
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::State;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetTargetRequest {
    pub request_id: String,
    pub display_id: u32,
    pub display_ref: String,
    pub snapshot_token: String,
    pub snapshot_revision: u64,
    #[serde(deserialize_with = "strict_region")]
    pub region: Region,
}
fn strict_region<'de, D: Deserializer<'de>>(d: D) -> Result<Region, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        origin: String,
        unit: String,
    }
    let r = Input::deserialize(d)?;
    Ok(Region {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
        origin: r.origin,
        unit: r.unit,
    })
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetTicket {
    pub target_id: String,
    pub revision: u64,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemporaryTarget {
    pub target_id: String,
    pub revision: u64,
    pub request_id: String,
    pub display_id: u32,
    pub area: CaptureArea,
    pub display: DisplayInfo,
}
impl TemporaryTarget {
    pub fn ticket(&self) -> TargetTicket {
        TargetTicket {
            target_id: self.target_id.clone(),
            revision: self.revision,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetSnapshot {
    pub revision: u64,
    pub phase: String,
    pub pending: Option<TemporaryTarget>,
    pub validation_scope: &'static str,
}
impl Default for TargetSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            phase: "idle".into(),
            pending: None,
            validation_scope: "logical-cached-snapshot",
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplaySnapshot {
    pub revision: u64,
    pub observed_at_unix_ms: u128,
    pub source: &'static str,
    pub displays: Vec<DisplayInfo>,
}
#[derive(Default)]
struct Inner {
    target: TargetSnapshot,
    cache: Option<DisplaySnapshot>,
    cache_revision: u64,
    used: HashSet<String>,
    closing: bool,
}
#[derive(Clone, Default)]
pub struct CaptureTargetManager(Arc<Mutex<Inner>>);
fn now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "target_random_failed".to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
impl CaptureTargetManager {
    pub fn snapshot(&self) -> TargetSnapshot {
        self.0.lock().unwrap().target.clone()
    }
    pub fn displays(&self) -> Result<DisplaySnapshot, String> {
        let s = self.0.lock().map_err(|_| "target_lock_failed")?;
        if s.closing {
            return Err("app_exiting".into());
        }
        s.cache
            .clone()
            .ok_or_else(|| "target_displays_unavailable_refresh_gui".into())
    }
    /// True iff an observable revision changed. An identical refresh only updates time.
    pub fn update_displays(&self, result: Result<Vec<DisplayInfo>, String>) -> bool {
        let mut s = self.0.lock().unwrap();
        if s.closing {
            return false;
        }
        let valid = result.ok().and_then(|mut ds| {
            ds.sort_by_key(|d| d.id);
            if ds.is_empty()
                || ds.iter().any(|d| {
                    d.width_points <= 0
                        || d.height_points <= 0
                        || d.display_ref.is_empty()
                        || d.snapshot_token.is_empty()
                })
                || ds.windows(2).any(|w| w[0].id == w[1].id)
            {
                None
            } else {
                Some(ds)
            }
        });
        let identical = match (&s.cache, &valid) {
            (Some(old), Some(ds)) => old.displays == *ds,
            _ => false,
        };
        if identical {
            s.cache.as_mut().unwrap().observed_at_unix_ms = now();
            return false;
        }
        // Every failed query explicitly invalidates previous knowledge.
        s.cache_revision += 1;
        s.cache = valid.map(|displays| DisplaySnapshot {
            revision: s.cache_revision,
            observed_at_unix_ms: now(),
            source: "last-gui-display-query",
            displays,
        });
        s.target.revision += 1;
        s.target.pending = None;
        s.target.phase = "invalidated".into();
        true
    }
    fn set(&self, r: SetTargetRequest) -> Result<TargetSnapshot, String> {
        let mut s = self.0.lock().map_err(|_| "target_lock_failed")?;
        if s.closing {
            return Err("app_exiting".into());
        }
        if r.request_id.is_empty() || r.request_id.len() > 256 {
            return Err("invalid_request_id".into());
        }
        if s.used.contains(&r.request_id) {
            return Err("request_id_reused".into());
        }
        if s.target.pending.is_some() {
            return Err("target_pending_clear_first".into());
        }
        let cache = s
            .cache
            .as_ref()
            .ok_or("target_displays_unavailable_refresh_gui")?;
        if cache.revision != r.snapshot_revision {
            return Err("target_display_snapshot_stale".into());
        }
        let display = cache
            .displays
            .iter()
            .find(|d| d.id == r.display_id)
            .ok_or("target_display_missing")?
            .clone();
        if display.display_ref != r.display_ref || display.snapshot_token != r.snapshot_token {
            return Err("target_display_identity_mismatch".into());
        }
        let area = CaptureArea {
            mode: "region".into(),
            display_ref: r.display_ref,
            snapshot_token: r.snapshot_token,
            region: Some(r.region),
        };
        area.validate(display.width_points as f64, display.height_points as f64)
            .map_err(|e| format!("target_region_invalid: {e}"))?;
        let target_id = random_id()?;
        s.used.insert(r.request_id.clone());
        s.target.revision += 1;
        s.target.pending = Some(TemporaryTarget {
            target_id,
            revision: s.target.revision,
            request_id: r.request_id,
            display_id: r.display_id,
            area,
            display,
        });
        s.target.phase = "pending".into();
        Ok(s.target.clone())
    }
    fn clear(&self, ticket: &TargetTicket) -> Result<TargetSnapshot, String> {
        let mut s = self.0.lock().map_err(|_| "target_lock_failed")?;
        if s.closing {
            return Err("app_exiting".into());
        }
        if !s
            .target
            .pending
            .as_ref()
            .is_some_and(|p| p.ticket() == *ticket)
        {
            return Err("target_ticket_mismatch".into());
        }
        s.target.pending = None;
        s.target.revision += 1;
        s.target.phase = "cleared".into();
        Ok(s.target.clone())
    }
    /// The capture gate is already held. The GUI must acknowledge both the
    /// exact ticket and contents; never silently replace an unacknowledged area.
    fn consume(
        &self,
        ticket: Option<&TargetTicket>,
        display_id: u32,
        area: Option<&CaptureArea>,
    ) -> Result<Option<TemporaryTarget>, String> {
        let mut s = self.0.lock().map_err(|_| "target_lock_failed")?;
        if s.closing {
            return Err("app_exiting".into());
        }
        match (s.target.pending.as_ref(), ticket) {
            (None, None) => return Ok(None),
            (None, Some(_)) => return Err("target_ticket_stale".into()),
            (Some(_), None) => return Err("target_ticket_required".into()),
            (Some(p), Some(t)) if p.ticket() != *t => return Err("target_ticket_mismatch".into()),
            _ => {}
        }
        let p = s.target.pending.as_ref().unwrap();
        if display_id != p.display_id || area != Some(&p.area) {
            return Err("target_payload_mismatch".into());
        }
        let result = s.target.pending.take();
        s.target.revision += 1;
        s.target.phase = "consumed".into();
        Ok(result)
    }
    /// A failed GUI start may clear only the exact acknowledged target, even if
    /// admission failed. A delayed failure must never clear a newer target.
    fn discard_if_matches(&self, ticket: Option<&TargetTicket>) -> bool {
        let mut s = self.0.lock().unwrap();
        if ticket.is_some_and(|t| s.target.pending.as_ref().is_some_and(|p| p.ticket() == *t)) {
            s.target.pending = None;
            s.target.revision += 1;
            s.target.phase = "consumed".into();
            true
        } else {
            false
        }
    }
    fn shutdown(&self) {
        let mut s = self.0.lock().unwrap();
        if !s.closing {
            s.closing = true;
            s.cache = None;
            s.target.pending = None;
            s.target.revision += 1;
            s.target.phase = "closed".into();
        }
    }
}
type Notifier = Arc<dyn Fn(u64) + Send + Sync>;
#[derive(Clone)]
pub struct TargetControl {
    manager: CaptureTargetManager,
    gate: OperationGate,
    selection: SelectionManager,
    capture: CaptureManager,
    notifier: Arc<Mutex<Option<Notifier>>>,
}
impl Default for TargetControl {
    fn default() -> Self {
        Self::new(
            CaptureTargetManager::default(),
            OperationGate::default(),
            SelectionManager::default(),
            CaptureManager::default(),
        )
    }
}
impl TargetControl {
    pub fn new(
        manager: CaptureTargetManager,
        gate: OperationGate,
        selection: SelectionManager,
        capture: CaptureManager,
    ) -> Self {
        Self {
            manager,
            gate,
            selection,
            capture,
            notifier: Default::default(),
        }
    }
    pub fn manager(&self) -> CaptureTargetManager {
        self.manager.clone()
    }
    pub fn bind_notifications(&self, f: impl Fn(u64) + Send + Sync + 'static) {
        *self.notifier.lock().unwrap() = Some(Arc::new(f));
    }
    fn notify(&self) {
        let f = self.notifier.lock().unwrap().clone();
        if let Some(f) = f {
            f(self.snapshot().revision);
        }
    }
    pub fn snapshot(&self) -> TargetSnapshot {
        self.manager.snapshot()
    }
    pub fn displays(&self) -> Result<DisplaySnapshot, String> {
        self.manager.displays()
    }
    pub fn update_displays(&self, result: Result<Vec<DisplayInfo>, String>) {
        if self.manager.update_displays(result) {
            self.notify();
        }
    }
    fn admission(&self) -> Result<crate::operation_gate::Lease, String> {
        let lease = self
            .gate
            .acquire(Owner::Selection)
            .map_err(|e| e.to_string())?;
        if crate::capture_area::SELECTING.load(std::sync::atomic::Ordering::SeqCst)
            || self.capture.busy()
            || matches!(
                self.selection.snapshot().phase.as_str(),
                "starting" | "running" | "cancelling" | "restoring"
            )
        {
            return Err("operation_busy".into());
        }
        Ok(lease)
    }
    pub fn set(&self, r: SetTargetRequest) -> Result<TargetSnapshot, String> {
        let _lease = self.admission()?;
        let result = self.manager.set(r)?;
        self.notify();
        Ok(result)
    }
    pub fn clear(&self, ticket: TargetTicket) -> Result<TargetSnapshot, String> {
        let _lease = self.admission()?;
        let result = self.manager.clear(&ticket)?;
        self.notify();
        Ok(result)
    }
    pub(crate) fn consume_for_capture(
        &self,
        ticket: Option<&TargetTicket>,
        display_id: u32,
        area: Option<&CaptureArea>,
    ) -> Result<Option<TemporaryTarget>, String> {
        let result = self.manager.consume(ticket, display_id, area);
        if result.as_ref().is_ok_and(|p| p.is_some()) {
            self.notify();
        }
        if result.is_err() {
            self.discard_failed_start(ticket);
        }
        result
    }
    pub(crate) fn discard_failed_start(&self, ticket: Option<&TargetTicket>) {
        if self.manager.discard_if_matches(ticket) {
            self.notify();
        }
    }
    pub fn shutdown(&self) {
        self.manager.shutdown();
        self.notify();
    }
}
#[tauri::command]
pub fn temporary_capture_target_status(control: State<'_, TargetControl>) -> TargetSnapshot {
    control.snapshot()
}
#[tauri::command]
pub fn clear_temporary_capture_target(
    control: State<'_, TargetControl>,
    ticket: TargetTicket,
) -> Result<TargetSnapshot, AppMessage> {
    control
        .clear(ticket)
        .map_err(|e| AppMessage::known("capture.region_invalid", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn display() -> DisplayInfo {
        DisplayInfo {
            id: 7,
            width_points: 800,
            height_points: 600,
            is_main: true,
            display_ref: "display-7".into(),
            snapshot_token: "layout-1".into(),
            point_pixel_scale: Some(2.0),
            source_pixels: Some(crate::capture_engine::SourcePixelSize { width: 1600, height: 1200 }),
        }
    }
    fn seeded() -> TargetControl {
        let c = TargetControl::default();
        c.update_displays(Ok(vec![display()]));
        c
    }
    fn request(c: &TargetControl, id: &str) -> SetTargetRequest {
        SetTargetRequest {
            request_id: id.into(),
            display_id: 7,
            display_ref: "display-7".into(),
            snapshot_token: "layout-1".into(),
            snapshot_revision: c.displays().unwrap().revision,
            region: Region {
                x: 10.,
                y: 20.,
                width: 400.,
                height: 300.,
                origin: "top-left".into(),
                unit: "display-logical".into(),
            },
        }
    }
    #[test]
    fn cache_is_explicit_and_identical_refresh_preserves_pending() {
        let c = TargetControl::default();
        assert!(c.displays().is_err());
        c.update_displays(Ok(vec![display()]));
        let initial = c.displays().unwrap();
        let p = c.set(request(&c, "one")).unwrap().pending.unwrap();
        let revision = c.snapshot().revision;
        c.update_displays(Ok(vec![display()]));
        assert_eq!(c.displays().unwrap().revision, initial.revision);
        assert!(c.displays().unwrap().observed_at_unix_ms >= initial.observed_at_unix_ms);
        assert_eq!(c.snapshot().revision, revision);
        assert_eq!(c.snapshot().pending.unwrap().ticket(), p.ticket());
        let mut d = display();
        d.snapshot_token = "layout-2".into();
        c.update_displays(Ok(vec![d]));
        assert!(c.snapshot().pending.is_none());
        assert_eq!(c.snapshot().phase, "invalidated");
        assert!(c.set(request(&c, "two")).is_err());
        c.update_displays(Err("query failed".into()));
        assert!(c.displays().is_err());
        assert!(c.set(request_with_revision(initial.revision)).is_err());
    }
    #[test]
    fn changed_capture_source_metadata_invalidates_the_cached_target() {
        let c = seeded();
        let original = request(&c, "original");
        c.set(original.clone()).unwrap();
        let mut changed = display();
        changed.point_pixel_scale = Some(1.0);
        changed.source_pixels = Some(crate::capture_engine::SourcePixelSize { width: 800, height: 600 });
        c.update_displays(Ok(vec![changed]));
        assert!(c.snapshot().pending.is_none());
        assert_eq!(c.snapshot().phase, "invalidated");
        let mut stale = original;
        stale.request_id = "stale".into();
        assert_eq!(c.set(stale).unwrap_err(), "target_display_snapshot_stale");
        assert!(c.set(request(&c, "fresh")).unwrap().pending.is_some());
    }

    fn request_with_revision(revision: u64) -> SetTargetRequest {
        let c = seeded();
        let mut r = request(&c, "r");
        r.snapshot_revision = revision;
        r
    }
    #[test]
    fn logical_validation_rejects_bad_coordinates_identity_and_snapshot() {
        let c = seeded();
        for (x, y, w, h) in [
            (f64::NAN, 0., 100., 100.),
            (0., f64::INFINITY, 100., 100.),
            (-1., 0., 100., 100.),
            (0., 0., 15., 16.),
            (790., 0., 20., 20.),
            (0., 590., 20., 20.),
        ] {
            let mut r = request(&c, "invalid");
            r.region.x = x;
            r.region.y = y;
            r.region.width = w;
            r.region.height = h;
            assert!(c.set(r).is_err());
        }
        for field in 0..6 {
            let mut r = request(&c, "invalid");
            match field {
                0 => r.display_id = 8,
                1 => r.display_ref = "other".into(),
                2 => r.snapshot_token = "old".into(),
                3 => r.snapshot_revision = 0,
                4 => r.region.origin = "bottom-left".into(),
                _ => r.region.unit = "pixels".into(),
            };
            assert!(c.set(r).is_err());
        }
        let mut r = request(&c, "valid");
        r.region.x = 0.25; // No fixed 0.5pt rounding: Swift validates the actual scale.
        assert!(c.set(r).is_ok());
    }
    #[test]
    fn strict_input_rejects_unknown_nested_fields() {
        let c = seeded();
        let mut value = serde_json::to_value(request(&c, "r")).unwrap();
        value["region"]["scale"] = 2.into();
        assert!(serde_json::from_value::<SetTargetRequest>(value).is_err());
        let mut value = serde_json::to_value(request(&c, "r")).unwrap();
        value["extra"] = true.into();
        assert!(serde_json::from_value::<SetTargetRequest>(value).is_err());
    }
    #[test]
    fn clear_and_consume_require_exact_ticket_and_never_reuse_request() {
        let c = seeded();
        let p = c.set(request(&c, "one")).unwrap().pending.unwrap();
        assert!(c.set(request(&c, "two")).is_err());
        let mut wrong = p.ticket();
        wrong.revision += 1;
        assert!(c.clear(wrong.clone()).is_err());
        assert!(c
            .consume_for_capture(Some(&wrong), p.display_id, Some(&p.area))
            .is_err());
        assert!(c.snapshot().pending.is_some());
        assert!(c
            .consume_for_capture(None, p.display_id, Some(&p.area))
            .is_err());
        assert!(c.snapshot().pending.is_some());
        c.clear(p.ticket()).unwrap();
        assert!(c.clear(p.ticket()).is_err());
        assert!(c.set(request(&c, "one")).is_err());
        let p = c.set(request(&c, "two")).unwrap().pending.unwrap();
        assert_eq!(
            c.consume_for_capture(Some(&p.ticket()), p.display_id, Some(&p.area))
                .unwrap()
                .unwrap()
                .ticket(),
            p.ticket()
        );
        assert_eq!(c.snapshot().phase, "consumed");
        assert!(c.snapshot().pending.is_none());
        assert!(c
            .consume_for_capture(Some(&p.ticket()), p.display_id, Some(&p.area))
            .is_err());
        assert!(c.consume_for_capture(None, 7, None).unwrap().is_none());
    }
    #[test]
    fn failed_start_discards_only_acknowledged_target_and_later_validation_cannot_restore_it() {
        let c = seeded();
        let p = c.set(request(&c, "one")).unwrap().pending.unwrap();
        assert!(c
            .consume_for_capture(Some(&p.ticket()), 99, Some(&p.area))
            .is_err());
        assert!(c.snapshot().pending.is_none());
        let next = c.set(request(&c, "two")).unwrap().pending.unwrap();
        c.discard_failed_start(Some(&p.ticket()));
        assert_eq!(c.snapshot().pending.unwrap().ticket(), next.ticket());
        let gate = c.gate.clone();
        let _lease = gate.acquire(Owner::Capture).unwrap();
        let consumed = c
            .consume_for_capture(Some(&next.ticket()), next.display_id, Some(&next.area))
            .unwrap()
            .unwrap();
        // The normal filename/destination/Swift checks occur after this point.
        assert!(crate::save_destination::validated_stem("../invalid").is_err());
        assert!(c.snapshot().pending.is_none());
        assert_eq!(consumed.display_id, 7);
    }
    #[test]
    fn gate_and_real_selection_state_block_mutations_and_shutdown_clears_memory() {
        let c = seeded();
        for owner in [Owner::Capture, Owner::Selection, Owner::Processing] {
            let l = c.gate.acquire(owner).unwrap();
            assert!(c.set(request(&c, "r")).is_err());
            drop(l);
        }
        let s = c.selection.begin("native").unwrap();
        assert!(c.set(request(&c, "r")).is_err());
        drop(s);
        let p = c.set(request(&c, "r")).unwrap().pending.unwrap();
        let l = c.gate.acquire(Owner::Capture).unwrap();
        assert!(c.clear(p.ticket()).is_err());
        drop(l);
        c.gate.quitting(true);
        assert!(c.clear(p.ticket()).is_err());
        c.gate.quitting(false);
        c.shutdown();
        assert!(c.snapshot().pending.is_none());
        assert!(c.displays().is_err());
        assert!(c.set(request_with_revision(1)).is_err());
        assert!(TargetControl::default().snapshot().pending.is_none());
    }
    #[test]
    fn concurrent_sets_and_clear_vs_gui_start_have_one_effect() {
        for _ in 0..12 {
            let c = seeded();
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let threads: Vec<_> = ["a", "b"]
                .into_iter()
                .map(|id| {
                    let c = c.clone();
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        let r = request(&c, id);
                        b.wait();
                        c.set(r).is_ok()
                    })
                })
                .collect();
            barrier.wait();
            assert_eq!(
                threads
                    .into_iter()
                    .filter_map(|t| t.join().ok())
                    .filter(|x| *x)
                    .count(),
                1
            );
            let p = c.snapshot().pending.unwrap();
            let b = Arc::new(std::sync::Barrier::new(3));
            let cc = c.clone();
            let pp = p.clone();
            let bb = b.clone();
            let clear = std::thread::spawn(move || {
                bb.wait();
                cc.clear(pp.ticket()).is_ok()
            });
            let cc = c.clone();
            let bb = b.clone();
            let start = std::thread::spawn(move || {
                bb.wait();
                let Ok(_lease) = cc.gate.acquire(Owner::Capture) else {
                    cc.discard_failed_start(Some(&p.ticket()));
                    return false;
                };
                cc.consume_for_capture(Some(&p.ticket()), p.display_id, Some(&p.area))
                    .is_ok()
            });
            b.wait();
            let results = [clear.join().unwrap(), start.join().unwrap()];
            assert!(results.into_iter().filter(|x| *x).count() <= 1);
            assert!(c.snapshot().pending.is_none());
        }
    }
    #[test]
    fn notifications_can_read_state_without_deadlocking_and_cache_never_runs_helper() {
        let c = seeded();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let reader = c.manager();
        c.bind_notifications(move |revision| {
            assert!(reader.snapshot().revision >= revision);
            s.lock().unwrap().push(revision);
        });
        let p = c.set(request(&c, "r")).unwrap().pending.unwrap();
        c.clear(p.ticket()).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
        // No app handle, executable, helper, filesystem or runtime needed.
        assert_eq!(c.displays().unwrap().source, "last-gui-display-query");
    }
}
