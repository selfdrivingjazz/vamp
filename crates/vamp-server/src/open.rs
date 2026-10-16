//! `GET /open?library=<id>&folder=<id>[&asset=<id>]`: an http bounce to the Mac app's
//! `vampire://open?…` deeplink (printed by `vamp explore link`), for terminals and chat
//! clients that only linkify http(s) URLs. Unauthenticated like `/install.sh`: it reads
//! nothing from the library and only echoes UUID-shaped ids back into the link.
use tiny_http::{Method, Request, Response, StatusCode};

use crate::{Reply, boxed, error_reply, header};

/// The reply for `/open`, or `None` when the request is not for it.
pub(crate) fn route(request: &Request) -> Option<Reply> {
    if !matches!(request.method(), Method::Get | Method::Head) {
        return None;
    }
    let (path, query) = request.url().split_once('?').unwrap_or((request.url(), ""));
    (path == "/open").then(|| page(query))
}

/// `8-4-4-4-12` hex digits, the shape of every library, folder and asset id.
fn uuid_shaped(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn page(query: &str) -> Reply {
    let params: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    let get = |key: &str| params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let (Some(library), Some(folder), asset) = (get("library"), get("folder"), get("asset")) else {
        return error_reply(400, "library and folder are required");
    };
    if ![library, folder].into_iter().chain(asset).all(uuid_shaped) {
        return error_reply(400, "library, folder and asset must be UUIDs");
    }
    // Only hex digits, '-', and the fixed URL text below: nothing needs escaping but '&'.
    let target = vamp_core::explore::link(library, folder, asset).replace('&', "&amp;");
    let body = format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>Open in Vampire</title>\n\
         <meta http-equiv=\"refresh\" content=\"0; url={target}\">\n\
         <meta name=\"viewport\" content=\"width=device-width\">\n\
         <style>body{{font:15px -apple-system,system-ui,sans-serif;background:#121212;color:#ddd;margin:4em auto;max-width:36em;padding:0 1em}}a{{color:#6fb6ff}}</style>\n\
         </head><body>\n<p>Opening Vampire…</p>\n\
         <p>If nothing happens, <a href=\"{target}\">open it in Vampire</a> (needs the Vampire app on this Mac).</p>\n\
         </body></html>\n"
    );
    let len = body.len();
    Response::new(
        StatusCode(200),
        vec![
            header("Content-Type", "text/html; charset=utf-8"),
            header("Cache-Control", "no-store"),
            header("Referrer-Policy", "no-referrer"),
        ],
        boxed(body.into_bytes()),
        Some(len),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::uuid_shaped;

    #[test]
    fn accepts_only_hyphenated_uuids() {
        assert!(uuid_shaped("6f1c0e2a-3b4d-4e5f-8a9b-0c1d2e3f4a5b"));
        assert!(uuid_shaped("6F1C0E2A-3B4D-4E5F-8A9B-0C1D2E3F4A5B"));
        for bad in [
            "",
            "6f1c0e2a3b4d4e5f8a9b0c1d2e3f4a5b",
            "6f1c0e2a-3b4d-4e5f-8a9b-0c1d2e3f4a5",
            "6f1c0e2a-3b4d-4e5f-8a9b-0c1d2e3f4a5g",
            "6f1c0e2a-3b4d-4e5f-8a9b_0c1d2e3f4a5b",
            "6f1c0e2a-3b4d-4e5f-8a9b-0c1d2e3f4a5b\"",
        ] {
            assert!(!uuid_shaped(bad), "{bad}");
        }
    }
}
