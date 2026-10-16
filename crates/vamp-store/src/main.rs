//! vamp-store: S3-compatible object storage helper for VAMP cloud originals.
//! One `StoreRequest` JSON on stdin, one `StoreResponse` JSON on stdout; errors are an
//! `ErrorResponse` on stderr with exit status 1. Credentials: AWS_ACCESS_KEY_ID,
//! AWS_SECRET_ACCESS_KEY, optional AWS_SESSION_TOKEN.
use anyhow::{Context, Result, bail, ensure};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;
use ureq::Agent;
use vamp_protocol::{ErrorDetail, ErrorResponse, PROTOCOL_VERSION, StoreRequest, StoreResponse};

/// Objects above this size use multipart upload.
const MULTIPART_THRESHOLD: u64 = 64 * 1024 * 1024;
const PART_SIZE: usize = 16 * 1024 * 1024;
const SIGN: Duration = Duration::from_secs(3600);
const META_HASH: &str = "x-amz-meta-blake3";

fn main() {
    if let Err(error) = run() {
        let response = ErrorResponse {
            protocol_version: PROTOCOL_VERSION.into(),
            error: ErrorDetail {
                code: "store_error".into(),
                message: format!("{error:#}"),
            },
        };
        let _ = serde_json::to_writer(std::io::stderr().lock(), &response);
        eprintln!();
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!(
            "vamp-store: reads one StoreRequest JSON (put/get/head/delete/presign) on stdin, writes one StoreResponse JSON on stdout"
        );
        return Ok(());
    }
    let mut input = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut input)?;
    ensure!(input.len() <= 1024 * 1024, "request exceeds 1 MiB");
    let request: StoreRequest = serde_json::from_slice(&input).context("invalid StoreRequest")?;
    let response = handle(&request)?;
    serde_json::to_writer(std::io::stdout().lock(), &response)?;
    println!();
    Ok(())
}

struct Store {
    bucket: Bucket,
    credentials: Credentials,
    agent: Agent,
}

fn handle(request: &StoreRequest) -> Result<StoreResponse> {
    ensure!(!request.key.is_empty(), "object key is required");
    let credentials = Credentials::from_env()
        .context("S3 credentials unavailable: set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY")?;
    let style = if request.path_style {
        UrlStyle::Path
    } else {
        UrlStyle::VirtualHost
    };
    let bucket = Bucket::new(
        request.endpoint.parse().context("invalid S3 endpoint")?,
        style,
        request.bucket.clone(),
        request.region.clone(),
    )
    .map_err(|e| anyhow::anyhow!("invalid S3 bucket configuration: {e}"))?;
    let agent: Agent = Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .build()
        .into();
    let store = Store {
        bucket,
        credentials,
        agent,
    };
    let path = || request.path.as_deref().context("path is required");
    match request.operation.as_str() {
        "put" => store.put(request, Path::new(path()?)),
        "get" => store.get(&request.key, Path::new(path()?)),
        "head" => store.head(&request.key),
        "delete" => store.delete(&request.key),
        "presign" => {
            let seconds = request.expires_seconds.unwrap_or(900);
            ensure!(
                (1..=7 * 24 * 3600).contains(&seconds),
                "presign expiry must be 1s–7d"
            );
            let url = store
                .bucket
                .get_object(Some(&store.credentials), &request.key)
                .sign(Duration::from_secs(seconds));
            Ok(StoreResponse {
                url: Some(url.into()),
                ..StoreResponse::default()
            })
        }
        other => bail!("unknown operation: {other}"),
    }
}

fn check(status: u16, what: &str, body: impl FnOnce() -> String) -> Result<()> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    bail!("{what} failed with HTTP {status}: {}", body().trim())
}
fn body_text(response: &mut ureq::http::Response<ureq::Body>) -> String {
    response
        .body_mut()
        .read_to_string()
        .unwrap_or_default()
        .chars()
        .take(2000)
        .collect()
}

impl Store {
    /// Uploads the file, hashing the exact bytes sent; refuses to finish if they differ from
    /// the expected blake3 (the local file changed mid-upload).
    fn put(&self, request: &StoreRequest, path: &Path) -> Result<StoreResponse> {
        let expected = request
            .blake3
            .as_deref()
            .context("blake3 is required for put")?;
        let mut file =
            File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
        let size = file.metadata()?.len();
        let content_type = request
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream");
        let mut hasher = blake3::Hasher::new();
        if size <= MULTIPART_THRESHOLD {
            let mut bytes = Vec::with_capacity(size as usize);
            file.read_to_end(&mut bytes)?;
            hasher.update(&bytes);
            ensure!(
                hasher.finalize().to_hex().as_str() == expected,
                "file changed before upload (blake3 mismatch)"
            );
            let mut action = self
                .bucket
                .put_object(Some(&self.credentials), &request.key);
            action.headers_mut().insert(META_HASH, expected);
            action.headers_mut().insert("content-type", content_type);
            let mut response = self
                .agent
                .put(action.sign(SIGN).as_str())
                .header(META_HASH, expected)
                .header("content-type", content_type)
                .send(&bytes[..])?;
            let status = response.status().as_u16();
            check(status, "PUT", || body_text(&mut response))?;
        } else {
            let mut create = self
                .bucket
                .create_multipart_upload(Some(&self.credentials), &request.key);
            create.headers_mut().insert(META_HASH, expected);
            create.headers_mut().insert("content-type", content_type);
            let mut response = self
                .agent
                .post(create.sign(SIGN).as_str())
                .header(META_HASH, expected)
                .header("content-type", content_type)
                .send_empty()?;
            let status = response.status().as_u16();
            let text = body_text(&mut response);
            check(status, "CreateMultipartUpload", || text.clone())?;
            let upload_id = rusty_s3::actions::CreateMultipartUpload::parse_response(&text)
                .map_err(|e| anyhow::anyhow!("invalid CreateMultipartUpload response: {e}"))?
                .upload_id()
                .to_owned();
            let result =
                self.upload_parts(&request.key, &upload_id, &mut file, &mut hasher, expected);
            if let Err(error) = result {
                let abort = self.bucket.abort_multipart_upload(
                    Some(&self.credentials),
                    &request.key,
                    &upload_id,
                );
                let _ = self.agent.delete(abort.sign(SIGN).as_str()).call();
                return Err(error);
            }
        }
        Ok(StoreResponse {
            exists: true,
            byte_size: Some(size),
            blake3: Some(expected.into()),
            url: None,
        })
    }
    fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        file: &mut File,
        hasher: &mut blake3::Hasher,
        expected: &str,
    ) -> Result<()> {
        let mut etags = Vec::new();
        let mut buffer = vec![0u8; PART_SIZE];
        loop {
            let mut filled = 0;
            while filled < PART_SIZE {
                let n = file.read(&mut buffer[filled..])?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            let part = &buffer[..filled];
            hasher.update(part);
            let number = u16::try_from(etags.len() + 1).context("too many parts")?;
            let action = self
                .bucket
                .upload_part(Some(&self.credentials), key, number, upload_id);
            let mut response = self.agent.put(action.sign(SIGN).as_str()).send(part)?;
            let status = response.status().as_u16();
            check(status, "UploadPart", || body_text(&mut response))?;
            let etag = response
                .headers()
                .get("etag")
                .and_then(|v| v.to_str().ok())
                .context("UploadPart returned no ETag")?
                .to_owned();
            etags.push(etag);
            if filled < PART_SIZE {
                break;
            }
        }
        ensure!(
            hasher.finalize().to_hex().as_str() == expected,
            "file changed during upload (blake3 mismatch)"
        );
        let complete = self.bucket.complete_multipart_upload(
            Some(&self.credentials),
            key,
            upload_id,
            etags.iter().map(String::as_str),
        );
        let url = complete.sign(SIGN);
        let mut response = self.agent.post(url.as_str()).send(complete.body())?;
        let status = response.status().as_u16();
        let text = body_text(&mut response);
        check(status, "CompleteMultipartUpload", || text.clone())?;
        // S3 may report a failed completion inside a 200 response.
        ensure!(
            !text.contains("<Error>"),
            "CompleteMultipartUpload failed: {text}"
        );
        Ok(())
    }
    /// Streams to `path` (created new). The caller verifies blake3 before trusting the bytes.
    fn get(&self, key: &str, path: &Path) -> Result<StoreResponse> {
        let action = self.bucket.get_object(Some(&self.credentials), key);
        let mut response = self.agent.get(action.sign(SIGN).as_str()).call()?;
        let status = response.status().as_u16();
        check(status, "GET", || body_text(&mut response))?;
        let meta = meta_hash(&response);
        let result = (|| -> Result<u64> {
            let mut out = OpenOptions::new().write(true).create_new(true).open(path)?;
            let size = std::io::copy(&mut response.into_body().into_reader(), &mut out)?;
            out.flush()?;
            out.sync_all()?;
            Ok(size)
        })();
        match result {
            Ok(size) => Ok(StoreResponse {
                exists: true,
                byte_size: Some(size),
                blake3: meta,
                url: None,
            }),
            Err(error) => {
                let _ = fs::remove_file(path);
                Err(error)
            }
        }
    }
    fn head(&self, key: &str) -> Result<StoreResponse> {
        let action = self.bucket.head_object(Some(&self.credentials), key);
        let response = self.agent.head(action.sign(SIGN).as_str()).call()?;
        let status = response.status().as_u16();
        if status == 404 {
            return Ok(StoreResponse::default());
        }
        check(status, "HEAD", String::new)?;
        let size = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        Ok(StoreResponse {
            exists: true,
            byte_size: size,
            blake3: meta_hash(&response),
            url: None,
        })
    }
    fn delete(&self, key: &str) -> Result<StoreResponse> {
        let action = self.bucket.delete_object(Some(&self.credentials), key);
        let mut response = self.agent.delete(action.sign(SIGN).as_str()).call()?;
        let status = response.status().as_u16();
        check(status, "DELETE", || body_text(&mut response))?;
        Ok(StoreResponse::default())
    }
}
fn meta_hash(response: &ureq::http::Response<ureq::Body>) -> Option<String> {
    response
        .headers()
        .get(META_HASH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}
