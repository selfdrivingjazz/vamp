//! Tailnet-only Apple diagnostic intake. No HTTP metadata reaches the database.
use crate::{HttpError, Reply, ok, read_body, request_header};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::{fs, path::Path, sync::Mutex, time::{Duration, SystemTime, UNIX_EPOCH}};
use tiny_http::Request;
use vamp_diagnostics::apple::{MAX_BYTES, Report};

const RETENTION: i64 = 90 * 86400;

pub(crate) struct Collector(Mutex<Connection>);

fn now() -> i64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 }

fn signature(report: &Report) -> String {
    // Stable across ASLR, OS versions and hardware; no deduplication across app builds.
    let primary = report.stacks.first().map(|s| &s[..s.len().min(8)]);
    blake3::hash(&serde_json::to_vec(&(report.kind, report.platform, &report.version, &report.build,
        report.signal, report.exception_type, report.exception_code, primary, &report.categories)).unwrap())
        .to_hex().to_string()
}

impl Collector {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(path)?;
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(1))?;
        db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;
            CREATE TABLE IF NOT EXISTS reports(signature TEXT PRIMARY KEY, report TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS occurrences(id TEXT PRIMARY KEY, signature TEXT NOT NULL REFERENCES reports(signature), received INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS occurrences_received ON occurrences(received);
            CREATE INDEX IF NOT EXISTS occurrences_signature ON occurrences(signature);")?;
        let collector = Self(Mutex::new(db));
        collector.prune(now())?;
        Ok(collector)
    }

    pub(crate) fn sweep(&self) {
        if let Err(e) = self.prune(now()) { eprintln!("vamp-server: could not expire crash reports: {e}"); }
    }

    fn prune(&self, at: i64) -> Result<()> {
        let mut db = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let tx = db.transaction()?;
        tx.execute("DELETE FROM occurrences WHERE received <= ?", [at.saturating_sub(RETENTION)])?;
        tx.execute("DELETE FROM reports WHERE NOT EXISTS (SELECT 1 FROM occurrences WHERE occurrences.signature = reports.signature)", [])?;
        tx.commit()?;
        Ok(())
    }

    fn store(&self, report: &Report, at: i64) -> Result<()> {
        let mut db = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let tx = db.transaction()?;
        let replay = tx.query_row("SELECT EXISTS(SELECT 1 FROM occurrences WHERE id = ?)", [&report.occurrence], |row| row.get::<_, bool>(0))?;
        if !replay {
            let key = signature(report);
            tx.execute("INSERT INTO reports VALUES (?, ?) ON CONFLICT(signature) DO UPDATE SET report=excluded.report",
                params![key, serde_json::to_string(report)?])?;
            tx.execute("INSERT INTO occurrences VALUES (?, ?, ?)", params![report.occurrence, key, at])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn receive(&self, request: &mut Request) -> Result<Reply, HttpError> {
        if request_header(request, "Origin").is_some()
            || request_header(request, "Content-Type") != Some("application/json") {
            return Err(HttpError::new(415, "expected a native JSON request".into()));
        }
        let body = read_body(request, MAX_BYTES as u64)?;
        let report = decode(&body)?;
        self.store(&report, now()).map_err(|_| HttpError::new(500, "report storage failed".into()))?;
        ok(serde_json::json!({"accepted":true}))
    }
}

fn decode(body: &[u8]) -> Result<Report, HttpError> {
    if body.len() > MAX_BYTES { return Err(HttpError::new(413, "crash report is too large".into())); }
    match serde_json::from_slice::<Report>(body) {
        Ok(report) if report.valid() => Ok(report),
        _ => Err(HttpError::new(400, "invalid crash report".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload() -> serde_json::Value {
        json!({"policy":1,"occurrence":"1".repeat(32),"kind":"crash","source":"metric_kit",
            "platform":"macos","version":"0.2.8","build":"1","os":"27.0","model":"Mac16,1",
            "signal":5,"stacks":[[{"uuid":"a".repeat(32),"offset":100}]],"categories":[]})
    }

    #[test]
    fn validates_before_storage() {
        let mut value = payload();
        assert!(decode(&value.to_string().into_bytes()).is_ok());
        value["query"] = json!("private");
        assert_eq!(decode(&value.to_string().into_bytes()).err().unwrap().status, 400);
        assert_eq!(decode(&vec![b' '; MAX_BYTES + 1]).err().unwrap().status, 413);
    }

    #[test]
    fn groups_distinct_occurrences_and_expires_replays() {
        let dir = tempfile::tempdir().unwrap();
        let collector = Collector::open(&dir.path().join("reports.sqlite3")).unwrap();
        let mut report = decode(payload().to_string().as_bytes()).ok().unwrap();
        collector.store(&report, 1).unwrap();
        collector.store(&report, 10).unwrap();
        report.occurrence = "2".repeat(32);
        collector.store(&report, 11).unwrap();
        let counts = || collector.0.lock().unwrap().query_row(
            "SELECT (SELECT count(*) FROM reports), (SELECT count(*) FROM occurrences)", [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))).unwrap();
        assert_eq!(counts(), (1, 2));
        collector.prune(RETENTION + 1).unwrap();
        assert_eq!(counts(), (1, 1));
        collector.prune(RETENTION + 11).unwrap();
        assert_eq!(counts(), (0, 0));
    }
}
