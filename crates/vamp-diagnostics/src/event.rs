use serde::{Deserialize, Serialize, Serializer, ser::SerializeSeq as _};
use std::{borrow::Cow, time::Duration};

macro_rules! wire_enum {
    ($visibility:vis enum $name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        $visibility enum $name {
            $(#[serde(rename = $wire)] $variant),+
        }
        impl $name {
            pub(crate) const VALUES: &[&str] = &[$($wire),+];
            const fn wire(self) -> &'static str {
                match self { $(Self::$variant => $wire),+ }
            }
        }
    };
}

wire_enum!(pub enum Feature {
    Launch => "launch", Setup => "setup", LibraryCreate => "library_create",
    LibraryConnect => "library_connect", LibraryImport => "library_import",
    LibraryOpen => "library_open", GridLoad => "grid_load", Sync => "sync",
    Save => "save", Search => "search", Similar => "similar", Riff => "riff",
    Galaxy => "galaxy", Browse => "browse", Viewer => "viewer", Clip => "clip",
    Previews => "previews", Video => "video", Captions => "captions",
    Embedding => "embedding", OtherProcessor => "other_processor",
    Diagnostics => "diagnostics", Other => "other",
});
wire_enum!(pub enum Outcome { Ok => "ok", Failed => "failed" });
wire_enum!(pub enum LibraryRole { Authority => "authority", Replica => "replica" });
wire_enum!(pub(crate) enum Kind { Usage => "usage", Error => "error", Panic => "panic", Test => "test" });
wire_enum!(enum Latency {
    Under16 => "<16ms", Under64 => "16-63ms", Under256 => "64-255ms",
    Under1024 => "256-1023ms", Under4096 => "1024-4095ms",
    Under16384 => "4096-16383ms", Long => "16384ms+",
});
wire_enum!(enum Size {
    Empty => "0", Under100 => "1-99", Under1000 => "100-999",
    Under10000 => "1000-9999", Under100000 => "10000-99999", Large => "100000+",
});
wire_enum!(pub(crate) enum Component { Cli => "vamp-cli", Server => "vamp-server", Apple => "vampire" });
wire_enum!(enum Platform { Linux => "linux", Darwin => "darwin", Ios => "ios", Windows => "windows", Other => "other" });

pub(crate) const DURATIONS: &[&str] = Latency::VALUES;
pub(crate) const SIZES: &[&str] = Size::VALUES;
pub(crate) const PLATFORMS: &[&str] = Platform::VALUES;

pub struct Product {
    pub(crate) component: Component,
    pub(crate) version: Cow<'static, str>,
    platform: Platform,
}

impl Product {
    fn rust(component: Component) -> Self {
        Self {
            component,
            version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
            platform: match std::env::consts::OS {
                "linux" => Platform::Linux,
                "macos" => Platform::Darwin,
                "windows" => Platform::Windows,
                _ => Platform::Other,
            },
        }
    }

    pub fn cli() -> Self {
        Self::rust(Component::Cli)
    }

    pub fn server() -> Self {
        Self::rust(Component::Server)
    }

    pub fn apple(version: &str) -> Option<Self> {
        Some(Self {
            component: Component::Apple,
            version: Cow::Owned(release(version)?.to_owned()),
            platform: match std::env::consts::OS {
                "macos" => Platform::Darwin,
                "ios" => Platform::Ios,
                _ => return None,
            },
        })
    }
}

fn release(version: &str) -> Option<&str> {
    let mut parts = version.split('.');
    for _ in 0..3 {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
    }
    parts.next().is_none().then_some(version)
}

impl Default for Outcome {
    fn default() -> Self {
        Self::Ok
    }
}

/// Only fixed dimensions enter this pipeline. Exact counters and timings are bucketed before queuing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub feature: Feature,
    #[serde(default)]
    pub outcome: Outcome,
    pub duration_ms: Option<u64>,
    pub library_size: Option<u64>,
    pub library_role: Option<LibraryRole>,
}

impl Event {
    pub fn new(feature: Feature, ok: bool) -> Self {
        Self {
            feature,
            outcome: if ok { Outcome::Ok } else { Outcome::Failed },
            duration_ms: None,
            library_size: None,
            library_role: None,
        }
    }

    pub fn timed(mut self, duration: Duration) -> Self {
        self.duration_ms = Some(duration.as_millis().min(u64::MAX as u128) as u64);
        self
    }

    pub(crate) fn bucket(self) -> Record {
        Record {
            kind: match self.outcome { Outcome::Ok => Kind::Usage, Outcome::Failed => Kind::Error },
            feature: self.feature,
            outcome: self.outcome,
            duration: self.duration_ms.map(|ms| match ms {
                0..=15 => Latency::Under16,
                16..=63 => Latency::Under64,
                64..=255 => Latency::Under256,
                256..=1023 => Latency::Under1024,
                1024..=4095 => Latency::Under4096,
                4096..=16383 => Latency::Under16384,
                _ => Latency::Long,
            }),
            size: self.library_size.map(|count| match count {
                0 => Size::Empty,
                1..=99 => Size::Under100,
                100..=999 => Size::Under1000,
                1000..=9999 => Size::Under10000,
                10000..=99999 => Size::Under100000,
                _ => Size::Large,
            }),
            role: self.library_role,
        }
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) kind: Kind,
    feature: Feature,
    outcome: Outcome,
    duration: Option<Latency>,
    size: Option<Size>,
    role: Option<LibraryRole>,
}

impl Record {
    pub(crate) fn test() -> Self {
        Self { kind: Kind::Test, ..Event::new(Feature::Diagnostics, true).bucket() }
    }

    pub(crate) fn panic(feature: Feature) -> Self {
        Self { kind: Kind::Panic, ..Event::new(feature, false).bucket() }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Body<'a> {
    resource_logs: [ResourceLog<'a>; 1],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResourceLog<'a> {
    resource: Resource<'a>,
    scope_logs: [ScopeLog<'a>; 1],
}

#[derive(Serialize)]
struct Resource<'a> {
    attributes: [Attribute<'a>; 3],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeLog<'a> {
    scope: Scope<'a>,
    log_records: Records<'a>,
}

#[derive(Serialize)]
struct Scope<'a> {
    name: &'static str,
    version: &'a str,
}

#[derive(Clone, Copy, Serialize)]
struct Attribute<'a> {
    key: &'static str,
    value: AttributeValue<'a>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
struct AttributeValue<'a> {
    string_value: &'a str,
}

fn attr<'a>(key: &'static str, string_value: &'a str) -> Attribute<'a> {
    Attribute { key, value: AttributeValue { string_value } }
}

struct Records<'a>(&'a [Record]);

impl Serialize for Records<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct WireRecord<'a> {
            severity_number: u8,
            attributes: &'a [Attribute<'a>],
        }
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for record in self.0 {
            let mut attributes = [attr("", ""); 6];
            attributes[..3].copy_from_slice(&[
                attr("app.diagnostics.kind", record.kind.wire()),
                attr("app.feature", record.feature.wire()),
                attr("app.outcome", record.outcome.wire()),
            ]);
            let mut len = 3;
            for attribute in [
                record.duration.map(|v| attr("app.duration_bucket", v.wire())),
                record.size.map(|v| attr("app.size_bucket", v.wire())),
                record.role.map(|v| attr("app.library_role", v.wire())),
            ].into_iter().flatten() {
                attributes[len] = attribute;
                len += 1;
            }
            seq.serialize_element(&WireRecord {
                severity_number: match record.kind {
                    Kind::Usage | Kind::Test => 9,
                    Kind::Error => 17,
                    Kind::Panic => 21,
                },
                attributes: &attributes[..len],
            })?;
        }
        seq.end()
    }
}

pub(crate) fn body(product: &Product, policy: u32, records: &[Record]) -> Vec<u8> {
    serde_json::to_vec(&Body {
        resource_logs: [ResourceLog {
            resource: Resource { attributes: [
                attr("service.name", product.component.wire()),
                attr("service.version", &product.version),
                attr("os.type", product.platform.wire()),
            ] },
            scope_logs: [ScopeLog {
                scope: Scope { name: "vamp.diagnostics", version: &policy.to_string() },
                log_records: Records(records),
            }],
        }],
    }).expect("the typed OTLP body can always be serialized")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn dimensions_are_closed_and_bucketed_before_serialization() {
        let invalid = json!({"feature":"search","query":"test-canary"});
        assert!(serde_json::from_value::<Event>(invalid).is_err());
        let mut event = Event::new(Feature::GridLoad, true);
        event.duration_ms = Some(257);
        event.library_size = Some(8765);
        event.library_role = Some(LibraryRole::Replica);
        let payload: Value = serde_json::from_slice(&body(&Product::cli(), 2, &[event.bucket()])).unwrap();
        let scope = &payload["resourceLogs"][0]["scopeLogs"][0];
        assert_eq!(scope["scope"], json!({"name":"vamp.diagnostics","version":"2"}));
        assert_eq!(scope["logRecords"], json!([{"severityNumber":9,"attributes":[
            {"key":"app.diagnostics.kind","value":{"stringValue":"usage"}},
            {"key":"app.feature","value":{"stringValue":"grid_load"}},
            {"key":"app.outcome","value":{"stringValue":"ok"}},
            {"key":"app.duration_bucket","value":{"stringValue":"256-1023ms"}},
            {"key":"app.size_bucket","value":{"stringValue":"1000-9999"}},
            {"key":"app.library_role","value":{"stringValue":"replica"}}
        ]}]));
    }

    #[test]
    fn release_has_no_build_or_machine_metadata() {
        assert_eq!(release("0.2.4"), Some("0.2.4"));
        for bad in ["0.2.4-private", "0.02.4", "0.2.10000", "0.2", "0.2.4.5", "0..4"] {
            assert_eq!(release(bad), None, "{bad}");
        }
    }
}
