//! Unauthenticated distribution routes for the tailnet: `GET /install.sh` (the CLI installer,
//! pointed back at this server) and `GET /dist/<path>` (files under the `--dist` directory:
//! per-target CLI binaries + SHA256SUMS, SKILL.md, the Mac DMG and Sparkle appcast).
use std::fs::File;
use std::path::Path;
use tiny_http::{Method, Request, Response, StatusCode};

use crate::{Reply, boxed, error_reply, header, request_header};

const INSTALLER: &str = include_str!("install.sh");

/// The reply for a distribution route, or `None` when the request is not one (or dist is off).
pub(crate) fn route(dir: Option<&Path>, request: &Request) -> Option<Reply> {
    let dir = dir?;
    if !matches!(request.method(), Method::Get | Method::Head) {
        return None;
    }
    let path = request.url().split(['?', '#']).next().unwrap_or_default();
    if path == "/install.sh" {
        return Some(installer(request));
    }
    let rest = path.strip_prefix("/dist/")?;
    Some(file(dir, rest))
}

fn installer(request: &Request) -> Reply {
    // The script fetches binaries from whatever address the client used to reach us.
    let Some(host) = request_header(request, "Host").filter(|h| {
        !h.is_empty()
            && h.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
    }) else {
        return error_reply(400, "missing or invalid Host header");
    };
    let body = INSTALLER.replace("@BASE@", &format!("http://{host}"));
    let len = body.len();
    Response::new(
        StatusCode(200),
        vec![
            header("Content-Type", "text/x-shellscript; charset=utf-8"),
            header("Cache-Control", "no-cache"),
        ],
        boxed(body.into_bytes()),
        Some(len),
        None,
    )
}

/// Relative path segments of plain names only: no `..`, hidden files, or empty segments.
fn safe(rest: &str) -> bool {
    !rest.is_empty()
        && rest.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
        })
}

fn file(dir: &Path, rest: &str) -> Reply {
    if !safe(rest) {
        return error_reply(404, "no such dist file");
    }
    let path = dir.join(rest);
    let Ok(file) = File::open(&path) else {
        return error_reply(404, "no such dist file");
    };
    let Ok(meta) = file.metadata() else {
        return error_reply(404, "no such dist file");
    };
    if !meta.is_file() {
        return error_reply(404, "no such dist file");
    }
    let mime = match path.extension().and_then(|e| e.to_str()) {
        Some("xml") => "application/xml",
        Some("md") => "text/markdown; charset=utf-8",
        Some("sh") => "text/x-shellscript; charset=utf-8",
        Some("dmg") => "application/x-apple-diskimage",
        // Firefox (and Zen) offer to install an add-on only when served with this type.
        Some("xpi") => "application/x-xpinstall",
        Some("zip") => "application/zip",
        _ if rest.ends_with("SHA256SUMS") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    };
    Response::new(
        StatusCode(200),
        vec![
            header("Content-Type", mime),
            header("Cache-Control", "no-cache"),
        ],
        Box::new(file),
        Some(meta.len() as usize),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::safe;

    #[test]
    fn rejects_traversal_and_hidden_paths() {
        for ok in [
            "SKILL.md",
            "x86_64-unknown-linux-gnu/vamp",
            "Vampire-1.2.dmg",
        ] {
            assert!(safe(ok), "{ok}");
        }
        for bad in [
            "",
            "..",
            "../etc/passwd",
            "a/../b",
            ".git/config",
            "a//b",
            "a/",
            "%2e%2e/x",
            "a\\b",
        ] {
            assert!(!safe(bad), "{bad}");
        }
    }
}
