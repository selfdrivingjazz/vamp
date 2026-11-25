//! Bundled, immutable processor toolkit profiles. Keep old profiles and their source assets
//! when adding a recommendation. A new implementation gets a new processor name (worker/job
//! identity); a different embedding space also gets a new model key (vector/query identity).

use super::Extra;
use serde_json::{Value, json};

pub(super) struct Processor {
    pub role: &'static str,
    pub name: &'static str,
    pub version: u32,
    pub arguments: &'static [&'static str],
    pub timeout: u64,
}

#[derive(Clone, Copy)]
pub(super) struct Component {
    pub script: &'static str,
    pub source: &'static str,
    pub manifest: &'static str,
    pub model: &'static str,
    pub revision: &'static str,
    pub embedding_model: Option<&'static str>,
    pub processors: &'static [Processor],
}

pub(crate) struct Profile {
    pub(super) version: u32,
    pub(super) requirements: &'static str,
    search: Component,
    captions: Component,
}

impl Profile {
    pub(super) fn component(&self, extra: Extra) -> &Component {
        match extra {
            Extra::Search => &self.search,
            Extra::Captions => &self.captions,
        }
    }

    pub(super) fn json(&self) -> Value {
        let component = |extra| {
            let spec = self.component(extra);
            let processors: Vec<_> = spec.processors.iter()
                .map(|p| json!({"name":p.name, "version":p.version}))
                .collect();
            json!({"model":spec.model, "revision":spec.revision,
                "embedding_model":spec.embedding_model, "processors":processors})
        };
        json!({"name":"recommended", "version":self.version,
            "components":{"search":component(Extra::Search), "captions":component(Extra::Captions)}})
    }
}

pub(super) const SIGLIP_MODEL: &str = "google/siglip-base-patch16-224";
pub(super) const SIGLIP_REVISION: &str = "7fd15f0689c79d79e38b1c2e2e2370a7bf2761ed";
pub(super) const SIGLIP_SCRIPT: &str = include_str!("../../../../processors/siglip/processor.py");
pub(super) const FLORENCE_SCRIPT: &str = include_str!("../../../../processors/florence/processor.py");

// Version 1 adopts the existing 0.4.2 commands and embedding keys. Unversioned names and
// tools/models are reserved for this profile; later profiles get their own runtime directory.
pub(super) static V1: Profile = Profile {
    version: 1,
    requirements: include_str!("../../../../processors/pyproject.toml"),
    search: Component {
        script: "siglip.py",
        source: SIGLIP_SCRIPT,
        manifest: "siglip.json",
        model: SIGLIP_MODEL,
        revision: SIGLIP_REVISION,
        embedding_model: Some(SIGLIP_MODEL),
        processors: &[Processor { role: "embedding", name: "siglip-embedding", version: 1, arguments: &[], timeout: 300 }],
    },
    captions: Component {
        script: "florence.py",
        source: FLORENCE_SCRIPT,
        manifest: "florence.json",
        model: "florence-community/Florence-2-base",
        revision: "00921df66db728a9ceb750f5eca43e5c203a2051",
        embedding_model: None,
        processors: &[
            Processor { role: "description", name: "florence-description", version: 1, arguments: &[], timeout: 600 },
            Processor { role: "tags", name: "florence-tags", version: 1, arguments: &["--task", "tags"], timeout: 600 },
        ],
    },
};

// Newest first; workers continue to offer older immutable processor IDs.
pub(super) const SUPPORTED: &[&Profile] = &[&V1];
pub(super) const RECOMMENDED: &Profile = SUPPORTED[0];

#[cfg(test)]
pub(super) static TEST_NEXT: Profile = Profile {
    version: 2,
    requirements: V1.requirements,
    search: Component {
        revision: "0000000000000000000000000000000000000002",
        embedding_model: Some("google/siglip-base-patch16-224@test-v2"),
        processors: &[Processor {
            role: "embedding", name: "siglip-embedding-v2", version: 2, arguments: &[], timeout: 300,
        }],
        ..V1.search
    },
    captions: V1.captions,
};
