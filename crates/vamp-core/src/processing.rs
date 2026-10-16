use super::*;

/// Pending or running job of `processor` for this asset's current original, if any.
fn active_job(tx: &Transaction, processor: &str, asset: &Asset) -> Result<Option<Job>> {
    tx.query_row(
        "SELECT document FROM jobs WHERE asset_id=?1 AND processor=?2 AND status IN ('pending','running') AND json_extract(document,'$.source_hash')=?3 LIMIT 1",
        params![asset.id, processor, asset.original.source_key()],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(parse)
    .transpose()
}
/// Queue a job unless one is already pending/running for (asset, processor, source hash);
/// then the existing job is returned.
pub(crate) fn insert_job(
    tx: &Transaction,
    processor: &ProcessorSpec,
    asset: &Asset,
    event_id: &str,
) -> Result<Job> {
    if let Some(job) = active_job(tx, &processor.name, asset)? {
        return Ok(job);
    }
    let job = Job {
        id: id(),
        asset_id: asset.id.clone(),
        source_hash: asset.original.source_key(),
        processor: processor.clone(),
        event_id: event_id.into(),
        status: "pending".into(),
        attempts: 0,
        error: None,
        created_at: now(),
        lease: None,
    };
    tx.execute("INSERT INTO jobs(id,asset_id,processor,event_id,status,document) VALUES(?1,?2,?3,?4,?5,?6)",params![job.id,job.asset_id,processor.name,event_id,job.status,json_text(&job)?])?;
    Ok(job)
}
fn save_job(conn: &Connection, job: &Job) -> Result<()> {
    conn.execute(
        "UPDATE jobs SET status=?2,document=?3 WHERE id=?1",
        params![job.id, job.status, json_text(job)?],
    )?;
    Ok(())
}
/// Compare-and-set a job document: false when another worker changed its status first.
fn transition_job(conn: &Connection, job: &Job, from: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE jobs SET status=?2,document=?3 WHERE id=?1 AND status=?4",
        params![job.id, job.status, json_text(job)?, from],
    )? > 0)
}
/// Expired leases beyond this many attempts fail instead of returning to the queue.
pub const MAX_LEASE_ATTEMPTS: u32 = 3;
/// Lease length bounds for `claim_jobs`/`heartbeat_job` (seconds).
pub const LEASE_SECONDS: std::ops::RangeInclusive<u64> = 1..=3600;
fn lease_expiry(seconds: u64) -> Result<String> {
    ensure!(
        LEASE_SECONDS.contains(&seconds),
        "lease_seconds must be {}–{}",
        LEASE_SECONDS.start(),
        LEASE_SECONDS.end()
    );
    Ok((Utc::now() + chrono::Duration::seconds(seconds as i64)).to_rfc3339())
}
fn lease_expired(lease: &JobLease) -> bool {
    chrono::DateTime::parse_from_rfc3339(&lease.expires_at)
        .map_or(true, |expires| expires <= Utc::now())
}
/// Running job held by `lease_id`; errors when the job moved on (reclaimed, finished).
fn leased_job(conn: &Connection, id: &str, lease_id: &str) -> Result<Job> {
    let job: Job = conn
        .query_row("SELECT document FROM jobs WHERE id=?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .map(parse)
        .transpose()?
        .context("job not found")?;
    ensure!(
        job.status == "running" && job.lease.as_ref().is_some_and(|l| l.id == lease_id),
        "lease conflict: job {id} is no longer held by this lease (status {})",
        job.status
    );
    Ok(job)
}
/// What a remote worker needs to run one claimed job.
#[derive(Debug, Clone, Serialize)]
pub struct ClaimedJob {
    pub job: Job,
    pub lease: JobLease,
    /// Representation (`original`, `thumbnail`, `preview`) to download as `input_path`.
    pub input: String,
    pub request: ToolRequest,
}
impl Library {
    pub fn processor_add(&mut self, spec: ProcessorSpec) -> Result<()> {
        validate_name(&spec.name)?;
        ensure!(
            !spec.command.is_empty() && !spec.command[0].is_empty(),
            "processor command cannot be empty"
        );
        ensure!(
            !spec.events.is_empty()
                && spec.events.iter().all(|e| matches!(
                    e.as_str(),
                    "media_added" | "media_updated" | "representation_ready"
                )),
            "processor events must be media_added, media_updated or representation_ready"
        );
        ensure!(
            matches!(spec.input.as_str(), "original" | "thumbnail" | "preview"),
            "processor input must be original, thumbnail or preview"
        );
        ensure!(
            matches!(
                spec.output.as_str(),
                "observation" | "thumbnail" | "preview"
            ),
            "processor output must be observation, thumbnail or preview"
        );
        ensure!(
            spec.input != spec.output,
            "processor input and output representations must differ"
        );
        ensure!(
            (1..=3600).contains(&spec.timeout_seconds),
            "processor timeout must be 1–3600 seconds"
        );
        ensure!(
            spec.placement != Placement::Remote || spec.output == "observation",
            "remote placement requires output observation (workers cannot upload representations)"
        );
        self.conn.execute("INSERT INTO processors(name,document) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET document=excluded.document",params![spec.name,json_text(&spec)?])?;
        Ok(())
    }
    pub fn processors(&self) -> Result<Vec<ProcessorSpec>> {
        documents(
            &self.conn,
            "SELECT document FROM processors ORDER BY name",
            [],
        )
    }
    pub fn processor_remove(&mut self, name: &str) -> Result<()> {
        ensure!(
            self.conn
                .execute("DELETE FROM processors WHERE name=?1", [name])?
                > 0,
            "processor not found: {name}"
        );
        // Existing jobs retain their explicit trusted configuration snapshot.
        Ok(())
    }
    /// Change who executes a processor's jobs (see `ProcessorSpec::placement`).
    pub fn processor_placement(
        &mut self,
        name: &str,
        placement: Placement,
    ) -> Result<ProcessorSpec> {
        let mut spec: ProcessorSpec = self
            .conn
            .query_row(
                "SELECT document FROM processors WHERE name=?1",
                [name],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(parse)
            .transpose()?
            .with_context(|| format!("processor not found: {name}"))?;
        spec.placement = placement;
        self.processor_add(spec.clone())?;
        Ok(spec)
    }
    fn processor_spec(tx: &Transaction, name: &str) -> Result<ProcessorSpec> {
        tx.query_row(
            "SELECT document FROM processors WHERE name=?1",
            [name],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(parse)
        .transpose()?
        .with_context(|| format!("processor not found: {name}"))
    }
    /// Queue a job; returns the existing one when a job for this asset's current original is
    /// already pending or running.
    pub fn enqueue(&mut self, name: &str, asset_id: &str) -> Result<Job> {
        let tx = self.conn.transaction()?;
        let spec = Self::processor_spec(&tx, name)?;
        let asset = get_asset(&tx, asset_id)?;
        if let Some(job) = active_job(&tx, name, &asset)? {
            return Ok(job);
        }
        let event = emit(
            &tx,
            &self.context,
            "processing_requested",
            Some(&asset),
            json!({"processor":name}),
        )?;
        let job = insert_job(&tx, &spec, &asset, &event.id)?;
        tx.commit()?;
        Ok(job)
    }
    /// Queue `name` for every live (not trashed/deleted) asset. Assets with a pending/running
    /// job are skipped; with `missing_only`, so are assets whose current original already has a
    /// completed job of this processor.
    pub fn enqueue_all(&mut self, name: &str, missing_only: bool) -> Result<Value> {
        let tx = self.conn.transaction()?;
        let spec = Self::processor_spec(&tx, name)?;
        let assets: Vec<Asset> = documents(
            &tx,
            "SELECT document FROM assets WHERE deleted=0 AND trashed=0 ORDER BY rowid",
            [],
        )?;
        let (mut enqueued, mut active, mut done) = (0u64, 0u64, 0u64);
        for asset in &assets {
            if active_job(&tx, name, asset)?.is_some() {
                active += 1;
                continue;
            }
            if missing_only
                && tx
                    .query_row(
                        "SELECT 1 FROM jobs WHERE asset_id=?1 AND processor=?2 AND status='completed' AND json_extract(document,'$.source_hash')=?3 LIMIT 1",
                        params![asset.id, name, asset.original.source_key()],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some()
            {
                done += 1;
                continue;
            }
            let event = emit(
                &tx,
                &self.context,
                "processing_requested",
                Some(asset),
                json!({"processor":name}),
            )?;
            insert_job(&tx, &spec, asset, &event.id)?;
            enqueued += 1;
        }
        tx.commit()?;
        Ok(
            json!({"processor":name,"enqueued":enqueued,"already_queued":active,"already_completed":done}),
        )
    }
    /// Delete queued (`pending`, default) or `failed` jobs, optionally of one processor.
    /// Running jobs cannot be cancelled. Returns how many were removed.
    pub fn cancel_jobs(&mut self, status: &str, processor: Option<&str>) -> Result<usize> {
        ensure!(
            matches!(status, "pending" | "failed"),
            "only pending or failed jobs can be cancelled"
        );
        if let Some(name) = processor {
            validate_name(name)?;
        }
        Ok(self.conn.execute(
            "DELETE FROM jobs WHERE status=?1 AND (?2 IS NULL OR processor=?2)",
            params![status, processor],
        )?)
    }
    pub fn jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
        ensure!((1..=1000).contains(&limit), "job limit must be 1–1000");
        if let Some(status) = status {
            ensure!(
                matches!(status, "pending" | "running" | "completed" | "failed"),
                "unknown job status"
            );
            documents(
                &self.conn,
                "SELECT document FROM jobs WHERE status=?1 ORDER BY rowid LIMIT ?2",
                params![status, limit as i64],
            )
        } else {
            documents(
                &self.conn,
                "SELECT document FROM jobs ORDER BY rowid DESC LIMIT ?1",
                [limit as i64],
            )
        }
    }
    pub fn retry_job(&mut self, id: &str) -> Result<Job> {
        let tx = self.conn.transaction()?;
        let mut job: Job = tx
            .query_row("SELECT document FROM jobs WHERE id=?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .map(parse)
            .transpose()?
            .context("job not found")?;
        ensure!(job.status == "failed", "only failed jobs may be retried");
        let asset = get_asset(&tx, &job.asset_id)?;
        ensure!(
            asset.original.source_key() == job.source_hash,
            "job refers to an old original; enqueue a new job for the current revision"
        );
        ensure!(
            active_job(&tx, &job.processor.name, &asset)?.is_none(),
            "a pending or running job of this processor already exists for this asset"
        );
        job.status = "pending".into();
        job.error = None;
        save_job(&tx, &job)?;
        tx.commit()?;
        Ok(job)
    }
    pub fn run_jobs(&mut self, limit: usize) -> Result<Value> {
        ensure!((1..=1000).contains(&limit), "job run limit must be 1–1000");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.state_dir.join("worker.lock"))?;
        lock.try_lock_exclusive()
            .context("another worker is already running for this library")?;
        // Holding the process lock proves no prior local worker can still own a running job.
        // Leased jobs belong to remote workers and are reclaimed only when their lease expires.
        for mut job in documents::<Job>(
            &self.conn,
            "SELECT document FROM jobs WHERE status='running'",
            [],
        )? {
            if job.lease.is_none() {
                job.status = "failed".into();
                job.error=Some("previous worker was interrupted; retry explicitly (processor execution may have occurred)".into());
                save_job(&self.conn, &job)?;
            }
        }
        self.reclaim_expired_leases()?;
        let mut completed = 0;
        let mut failed = 0;
        let mut deferred = 0;
        let mut jobs = Vec::new();
        let mut after = 0i64;
        while jobs.len() < limit {
            // The current registration decides placement: `remote` jobs wait for /v1/jobs/claim.
            let Some((rowid, mut job)) = self
                .conn
                .query_row(
                    "SELECT j.rowid,j.document FROM jobs j LEFT JOIN processors p ON p.name=j.processor WHERE j.status='pending' AND j.rowid>?1 AND coalesce(json_extract(p.document,'$.placement'),'any')!='remote' ORDER BY j.rowid LIMIT 1",
                    [after],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?
                .map(|(rowid, document)| parse::<Job>(document).map(|job| (rowid, job)))
                .transpose()?
            else {
                break;
            };
            after = rowid;
            job.status = "running".into();
            job.attempts += 1;
            job.lease = None;
            if !transition_job(&self.conn, &job, "pending")? {
                continue; // claimed by a remote worker meanwhile
            }
            match self.execute_job(&mut job) {
                Ok(()) => {
                    job.status = "completed".into();
                    job.error = None;
                    completed += 1;
                }
                Err(error) if crate::io::is_stalled(&error) => {
                    // The original's storage is not responding: retry on a later run without
                    // spending an attempt; the next pending job runs meanwhile.
                    job.status = "pending".into();
                    job.attempts -= 1;
                    job.error = Some(format!("{error:#}"));
                    deferred += 1;
                }
                Err(error) => {
                    job.status = "failed".into();
                    job.error = Some(format!("{error:#}"));
                    failed += 1;
                }
            }
            save_job(&self.conn, &job)?;
            jobs.push(job);
        }
        // Cloud uploads share the worker lock so `jobs run/watch` and vamp-server drain both.
        let storage = self.run_uploads(limit)?;
        // Eagle thumbnails (import phase B) have their own lock: the app bridge may copy too.
        let thumbnails = self.copy_eagle_thumbnails(limit.max(64), &mut |_| {})?;
        let count = |key: &str| storage[key].as_u64().unwrap_or(0);
        let evicted = storage["evicted"].as_array().map_or(0, Vec::len) as u64;
        Ok(json!({
            "completed": completed + count("completed") + evicted + thumbnails["copied"].as_u64().unwrap_or(0),
            "failed": failed + count("failed"),
            "deferred": deferred,
            "jobs": jobs,
            "uploads": storage["uploads"],
            "evicted": storage["evicted"],
            "thumbnails": thumbnails,
        }))
    }
    /// Return running jobs whose lease expired to the queue (or fail them after
    /// `MAX_LEASE_ATTEMPTS`). Returns how many jobs changed.
    pub fn reclaim_expired_leases(&mut self) -> Result<usize> {
        let mut changed = 0;
        for mut job in documents::<Job>(
            &self.conn,
            "SELECT document FROM jobs WHERE status='running'",
            [],
        )? {
            let Some(lease) = job.lease.clone() else {
                continue;
            };
            if !lease_expired(&lease) {
                continue;
            }
            if job.attempts >= MAX_LEASE_ATTEMPTS {
                job.status = "failed".into();
                job.error = Some(format!(
                    "lease of worker {} expired after {} attempts; retry explicitly",
                    lease.worker_id, job.attempts
                ));
            } else {
                job.status = "pending".into();
                job.error = Some(format!("lease of worker {} expired", lease.worker_id));
                job.lease = None;
            }
            if transition_job(&self.conn, &job, "running")? {
                changed += 1;
            }
        }
        Ok(changed)
    }
    /// Lease up to `limit` pending jobs of the named processors to a remote worker.
    /// Only observation processors whose current placement is not `local` are claimable.
    pub fn claim_jobs(
        &mut self,
        worker_id: &str,
        processors: &[String],
        limit: usize,
        lease_seconds: u64,
    ) -> Result<Vec<ClaimedJob>> {
        ensure!(
            !worker_id.trim().is_empty() && worker_id.len() <= 128,
            "worker_id must be 1–128 characters"
        );
        ensure!((1..=100).contains(&limit), "claim limit must be 1–100");
        lease_expiry(lease_seconds)?;
        self.reclaim_expired_leases()?;
        let mut claimed = Vec::new();
        let mut after = 0i64;
        while claimed.len() < limit {
            let next = self
                .conn
                .query_row(
                    "SELECT j.rowid,j.document,p.document FROM jobs j JOIN processors p ON p.name=j.processor WHERE j.status='pending' AND j.rowid>?1 AND j.processor IN (SELECT value FROM json_each(?2)) AND coalesce(json_extract(p.document,'$.placement'),'any')!='local' AND json_extract(p.document,'$.output')='observation' ORDER BY j.rowid LIMIT 1",
                    params![after, json_text(&processors)?],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                )
                .optional()?;
            let Some((rowid, document, spec)) = next else {
                break;
            };
            after = rowid;
            let mut job: Job = parse(document)?;
            let spec: ProcessorSpec = parse(spec)?;
            let asset = get_asset(&self.conn, &job.asset_id)?;
            if asset.original.source_key() != job.source_hash {
                job.status = "failed".into();
                job.error = Some("stale job: original revision changed; enqueue a new job".into());
                transition_job(&self.conn, &job, "pending")?;
                continue;
            }
            let lease = JobLease {
                id: id(),
                worker_id: worker_id.trim().into(),
                expires_at: lease_expiry(lease_seconds)?,
            };
            job.status = "running".into();
            job.attempts += 1;
            job.error = None;
            job.lease = Some(lease.clone());
            if !transition_job(&self.conn, &job, "pending")? {
                continue;
            }
            let request = ToolRequest {
                operation: "process".into(),
                asset: Some(asset),
                library_id: Some(self.context.library_id.clone()),
                event_id: Some(job.event_id.clone()),
                ..ToolRequest::default()
            };
            // The job's snapshot names the input; the registration's input wins if changed.
            claimed.push(ClaimedJob {
                input: spec.input,
                job,
                lease,
                request,
            });
        }
        Ok(claimed)
    }
    /// Extend a held lease; fails with "lease conflict" once the job was reclaimed.
    pub fn heartbeat_job(&mut self, id: &str, lease_id: &str, lease_seconds: u64) -> Result<Job> {
        let expires_at = lease_expiry(lease_seconds)?;
        let mut job = leased_job(&self.conn, id, lease_id)?;
        job.lease.as_mut().expect("leased").expires_at = expires_at;
        ensure!(
            transition_job(&self.conn, &job, "running")?,
            "lease conflict: job {id} changed concurrently"
        );
        Ok(job)
    }
    /// Apply a remote worker's ToolResponse. Stale results (original changed) fail the job.
    pub fn complete_job(
        &mut self,
        id: &str,
        lease_id: &str,
        response: ToolResponse,
    ) -> Result<Job> {
        let applied = (|| -> Result<Job> {
            // Lease check and result application share one transaction: a reclaimed job
            // can never also receive a late result.
            let tx = self.conn.transaction()?;
            let mut job = leased_job(&tx, id, lease_id)?;
            apply_response(&tx, &self.context, &job, response, None)?;
            job.status = "completed".into();
            job.error = None;
            save_job(&tx, &job)?;
            tx.commit()?;
            Ok(job)
        })();
        match applied {
            Err(error) if !format!("{error:#}").contains("lease conflict") => {
                let mut job = leased_job(&self.conn, id, lease_id)?;
                job.status = "failed".into();
                job.error = Some(format!("{error:#}"));
                transition_job(&self.conn, &job, "running")?;
                Err(error)
            }
            result => result,
        }
    }
    /// Record a remote worker's failure (no automatic retry; `jobs retry` re-queues).
    pub fn fail_job(&mut self, id: &str, lease_id: &str, error: &str) -> Result<Job> {
        let mut job = leased_job(&self.conn, id, lease_id)?;
        job.status = "failed".into();
        job.error = Some(format!(
            "worker {}: {error}",
            job.lease.as_ref().expect("leased").worker_id
        ));
        save_job(&self.conn, &job)?;
        Ok(job)
    }
    fn execute_job(&mut self, job: &mut Job) -> Result<()> {
        let mut asset = get_asset(&self.conn, &job.asset_id)?;
        ensure!(
            asset.original.source_key() == job.source_hash,
            "stale job: original revision changed; enqueue a new job"
        );
        let spec = job.processor.clone();
        let derivative = spec.output != "observation";
        if derivative
            && self.inspect(&asset.id)?.representations.iter().any(|r| {
                r.kind == spec.output
                    && local_representation(&self.state_dir, &r.path).is_ok_and(|p| p.is_file())
            })
        {
            return Ok(());
        }
        let input = self.fetch_or_generate(&asset.id, &spec.input, media_helper().as_deref())?;
        if spec.input == "original" {
            if asset.original.pending() {
                // Reading the original anyway: settle the lazy fingerprint. Same bytes keep
                // this job (rekeyed); changed bytes make it stale.
                let revision = asset.revision;
                asset = self.resolve_hash(&asset.id)?;
                ensure!(
                    asset.revision == revision,
                    "stale job: original changed since it was cataloged; enqueue a new job"
                );
                job.source_hash = asset.original.source_key();
            }
            let digest = {
                let input = input.clone();
                crate::io::background().try_run(
                    format!("verify job input {}", asset.id),
                    crate::io::DRIVE_DEADLINE,
                    move || fingerprint(&input),
                )?
            };
            ensure!(
                digest.blake3 == asset.original.blake3,
                "original changed; run media refresh before processing"
            );
        }
        let relative = derivative.then(|| {
            format!(
                "representations/{}/{}-{}.jpg",
                asset.original.source_key().replace(':', "-"),
                spec.output,
                id()
            )
        });
        let output = relative.as_ref().map(|r| self.state_dir.join(r));
        if let Some(path) = &output {
            fs::create_dir_all(path.parent().unwrap())?;
        }
        let request = ToolRequest {
            operation: if derivative { "generate" } else { "process" }.into(),
            input_path: Some(path_text(&input)?),
            output_path: output.as_ref().map(|p| path_text(p)).transpose()?,
            max_edge: derivative.then_some(if spec.output == "thumbnail" {
                512
            } else {
                1600
            }),
            asset: Some(asset.clone()),
            library_id: Some(self.context.library_id.clone()),
            event_id: Some(job.event_id.clone()),
            ..ToolRequest::default()
        };
        let response = run_tool(&spec.command, &request, spec.timeout_seconds)?;
        if spec.input == "original" {
            ensure!(
                fingerprint(&input)?.blake3 == asset.original.blake3,
                "original changed during processing; refresh and enqueue again"
            );
        }
        let tx = self.conn.transaction()?;
        apply_response(
            &tx,
            &self.context,
            job,
            response,
            relative.as_deref().zip(output.as_deref()),
        )?;
        tx.commit()?;
        Ok(())
    }
}
/// Apply a processor's response for `job` inside `tx`. `derivative` is the
/// (catalog-relative, absolute) output path expected for representation processors.
fn apply_response(
    tx: &Transaction,
    context: &Context,
    job: &Job,
    response: ToolResponse,
    derivative: Option<(&str, &Path)>,
) -> Result<()> {
    let spec = &job.processor;
    let current = get_asset(tx, &job.asset_id)?;
    ensure!(
        current.original.source_key() == job.source_hash,
        "stale result: original revision changed while processing"
    );
    if let Some((relative, output)) = derivative {
        let result = response
            .representation
            .context("processor returned no representation")?;
        ensure!(
            Path::new(&result.path) == output && output.is_file(),
            "processor returned an unexpected or missing output file"
        );
        ensure!(
            result.width > 0 && result.height > 0,
            "processor returned empty representation dimensions"
        );
        let rep = Representation {
            kind: spec.output.clone(),
            path: relative.into(),
            source_hash: job.source_hash.clone(),
            width: result.width,
            height: result.height,
            mime: result.mime,
            byte_size: fs::metadata(output)?.len(),
        };
        attach_representation(tx, context, &current, &rep)?;
        return Ok(());
    }
    ensure!(
        spec.output == "observation",
        "{} output requires a local representation file",
        spec.output
    );
    let result = response
        .observation
        .context("processor returned no observation")?;
    ensure!(!result.kind.is_empty(), "observation kind cannot be empty");
    let mut value = result.value;
    if result.kind == "embedding" {
        let embedding = value
            .get_mut("embedding")
            .context("embedding observation requires value.embedding")?;
        embeddings::store(tx, &current.id, &job.source_hash, embedding)?;
    }
    // Job identity makes applying a retried observation idempotent after a crash.
    let observation = Observation {
        id: job.id.clone(),
        asset_id: current.id.clone(),
        source_hash: job.source_hash.clone(),
        processor: spec.name.clone(),
        model: result.model,
        kind: result.kind,
        value,
        created_at: now(),
    };
    let added = tx.execute(
        "INSERT OR IGNORE INTO observations(id,asset_id,source_hash,document) VALUES(?1,?2,?3,?4)",
        params![
            observation.id,
            observation.asset_id,
            observation.source_hash,
            json_text(&observation)?
        ],
    )?;
    if added > 0 {
        reindex(tx, &current)?;
        emit(
            tx,
            context,
            "observation_added",
            Some(&current),
            json!({"observation_id":observation.id,"processor":spec.name}),
        )?;
    }
    Ok(())
}
